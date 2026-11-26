use super::*;
use crate::{
    ActionCandidate, Artifact, FileInput, PROTOCOL_VERSION, StreamArtifact, compiler_action_key,
    digest_bytes,
};
use std::time::Duration;

struct Fixture {
    root: PathBuf,
    store: Store,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "bellows-lru-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let store = Store::open(&root).unwrap();
        Self { root, store }
    }

    /// Publishes a candidate whose only artifact is 100 bytes of `fill`.
    fn publish(&self, name: &str, fill: u8, created_ms: u64, protocol: u32) -> ActionCandidate {
        self.publish_output(name, &format!("lib{name}.rlib"), fill, created_ms, protocol)
    }

    /// Publishes a candidate whose only artifact, `file_name`, is 100
    /// bytes of `fill`.
    fn publish_output(
        &self,
        name: &str,
        file_name: &str,
        fill: u8,
        created_ms: u64,
        protocol: u32,
    ) -> ActionCandidate {
        let payload = vec![fill; 100];
        let digest = digest_bytes(&payload);
        self.store.put_blob(&digest, &payload).unwrap();
        let empty = digest_bytes(b"");
        self.store.put_blob(&empty, b"").unwrap();
        let static_key = digest_bytes(name.as_bytes());
        let files = vec![FileInput {
            path: "$WORKSPACE/src/lib.rs".into(),
            digest: digest_bytes(name.as_bytes()),
        }];
        let candidate = ActionCandidate {
            protocol,
            action_key: compiler_action_key(&static_key, &files, &[], &[]),
            static_key,
            crate_name: name.into(),
            created_ms,
            files,
            host_files: vec![],
            env: vec![],
            artifacts: vec![Artifact {
                file_name: file_name.into(),
                digest,
                executable: !file_name.ends_with(".rlib"),
            }],
            stdout: StreamArtifact {
                digest: empty.clone(),
                len: 0,
            },
            stderr: StreamArtifact {
                digest: empty,
                len: 0,
            },
            proc_macros: vec![],
            pinned_checkouts: vec![],
        };
        if protocol == PROTOCOL_VERSION {
            self.store.put_candidate(candidate.clone(), 8).unwrap();
        } else {
            // Stores keep older protocols' records, which current
            // validation rejects.
            let path = self.store.action_path(&candidate.static_key).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let index = CandidateIndex {
                candidates: vec![candidate.clone()],
            };
            atomic_write(&path, &serde_json::to_vec(&index).unwrap()).unwrap();
        }
        candidate
    }

    /// Replaces the journal with uses at chosen times.
    fn uses(&self, uses: &[(&ActionCandidate, u64)]) {
        let directory = self.root.join(JOURNAL_DIR);
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let lines = uses
            .iter()
            .map(|(candidate, ms)| {
                format!("{} {} {ms}\n", candidate.static_key, candidate.action_key)
            })
            .collect::<String>();
        fs::write(directory.join(JOURNAL_FILE), lines).unwrap();
    }

    /// Ages every blob past the windows that protect racing publications.
    fn age_blobs(&self) {
        let mut blobs = Vec::new();
        collect_paths(&self.root.join("blobs"), &mut blobs).unwrap();
        for blob in blobs {
            fs::OpenOptions::new()
                .append(true)
                .open(blob)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
                .unwrap();
        }
    }

    fn present(&self, candidate: &ActionCandidate) -> bool {
        self.store
            .action_path(&candidate.static_key)
            .unwrap()
            .exists()
    }

    fn blob(&self, candidate: &ActionCandidate) -> bool {
        self.store.read_blob(&candidate.artifacts[0].digest).is_ok()
    }

    fn options(&self, max_bytes: u64) -> GcOptions {
        GcOptions {
            max_bytes,
            ..GcOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn evicts_the_least_recently_used_candidate_first() {
    let fixture = Fixture::new("lru");
    let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
    let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
    // The older publication was reused after the newer one was written.
    fixture.uses(&[(&older, 3_000)]);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(&fixture.options(100), &unserialized)
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert_eq!(report.bytes_after, 100);
    assert!(fixture.present(&older) && fixture.blob(&older));
    assert!(!fixture.present(&newer) && !fixture.blob(&newer));
}

#[test]
fn executables_are_classified_apart_from_inputs_to_other_compilations() {
    let output = |file_name: &str| Artifact {
        file_name: file_name.into(),
        digest: String::new(),
        executable: true,
    };
    let kind = |crate_name: &str, file_name: &str| {
        candidate_kind(crate_name, &[output(file_name), output("dep.d")])
    };
    assert!(kind("suite", "suite-0123abcd") == Kind::Executable);
    assert!(kind("flagship", "flagship.exe") == Kind::Executable);
    assert!(kind("serde_derive", "libserde_derive-0123abcd.so") == Kind::Linked);
    assert!(kind("serde_derive", "serde_derive-0123abcd.dll") == Kind::Linked);
    assert!(kind("build_script_build", "build_script_build-0123abcd") == Kind::Linked);
    assert!(kind("serde", "libserde-0123abcd.rlib") == Kind::Library);
}

#[test]
fn executables_never_push_out_libraries_under_their_own_budget() {
    let fixture = Fixture::new("executables");
    // Oldest first, as one build publishes them: a procedural macro, the
    // library using it, then two test harnesses linking both.
    let proc_macro = fixture.publish_output("derive", "libderive-1.so", 1, 500, PROTOCOL_VERSION);
    let library = fixture.publish("library", 2, 1_000, PROTOCOL_VERSION);
    let older_test = fixture.publish_output("suite", "suite-1", 3, 2_000, PROTOCOL_VERSION);
    let newer_test = fixture.publish_output("other", "other-1", 4, 3_000, PROTOCOL_VERSION);
    // Publication times alone order them.
    fixture.uses(&[]);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(
            &GcOptions {
                max_executable_bytes: Some(100),
                ..fixture.options(300)
            },
            &unserialized,
        )
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert_eq!(report.bytes_after, 300);
    assert_eq!(report.max_executable_bytes, Some(100));
    assert!(fixture.present(&proc_macro) && fixture.blob(&proc_macro));
    assert!(fixture.present(&library) && fixture.blob(&library));
    assert!(!fixture.present(&older_test) && !fixture.blob(&older_test));
    assert!(fixture.present(&newer_test) && fixture.blob(&newer_test));
    let executables = report
        .breakdown
        .iter()
        .find(|bucket| bucket.group == "kind" && bucket.label == "executable")
        .unwrap();
    assert_eq!((executables.records, executables.evicted_records), (2, 1));
}

#[test]
fn without_an_executable_budget_recency_alone_decides() {
    let fixture = Fixture::new("recency");
    let library = fixture.publish("library", 1, 1_000, PROTOCOL_VERSION);
    let test = fixture.publish_output("suite", "suite-1", 2, 2_000, PROTOCOL_VERSION);
    // Publication times alone order them.
    fixture.uses(&[]);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(&fixture.options(100), &unserialized)
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert!(!fixture.present(&library));
    assert!(fixture.present(&test));
}

#[test]
fn the_overall_budget_still_applies_after_the_executable_budget() {
    let fixture = Fixture::new("both-budgets");
    let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
    let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
    let test = fixture.publish_output("suite", "suite-1", 3, 3_000, PROTOCOL_VERSION);
    // Publication times alone order them.
    fixture.uses(&[]);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(
            &GcOptions {
                max_executable_bytes: Some(0),
                ..fixture.options(100)
            },
            &unserialized,
        )
        .unwrap();
    assert_eq!(report.records_evicted, 2);
    assert!(!fixture.present(&test));
    assert!(!fixture.present(&older));
    assert!(fixture.present(&newer));
}

#[test]
fn a_dry_run_reports_the_plan_and_changes_nothing() {
    let fixture = Fixture::new("dry");
    let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
    let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
    fixture.uses(&[(&older, 3_000)]);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(
            &GcOptions {
                dry_run: true,
                ..fixture.options(100)
            },
            &unserialized,
        )
        .unwrap();
    assert!(report.dry_run);
    assert_eq!((report.records_evicted, report.evicted_bytes), (1, 100));
    assert_eq!((report.bytes_before, report.bytes_after), (200, 100));
    assert_eq!(report.journal_entries, 1);
    let crates = report
        .breakdown
        .iter()
        .filter(|bucket| bucket.group == "crate" && bucket.evicted_records == 1)
        .map(|bucket| bucket.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(crates, ["newer"]);
    assert!(fixture.present(&older) && fixture.blob(&older));
    assert!(fixture.present(&newer) && fixture.blob(&newer));
    assert!(fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE).exists());
}

#[test]
fn older_protocols_are_evicted_whatever_the_budget() {
    let fixture = Fixture::new("protocol");
    let old = fixture.publish("old", 1, now_ms(), PROTOCOL_VERSION - 1);
    let current = fixture.publish("current", 2, 1_000, PROTOCOL_VERSION);
    fixture.age_blobs();
    let report = fixture
        .store
        .collect(
            &GcOptions {
                min_protocol: Some(PROTOCOL_VERSION),
                ..fixture.options(u64::MAX)
            },
            &unserialized,
        )
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert!(!fixture.present(&old) && !fixture.blob(&old));
    assert!(fixture.present(&current) && fixture.blob(&current));
}

#[test]
fn a_blob_offered_again_during_collection_is_kept() {
    let fixture = Fixture::new("offered");
    let candidate = fixture.publish("only", 1, 1_000, PROTOCOL_VERSION);
    // Touched after the collection starts: a client re-offering the blob
    // for a new record while the collection runs.
    fixture.age_blobs();
    fs::OpenOptions::new()
        .append(true)
        .open(
            fixture
                .store
                .blob_path(&candidate.artifacts[0].digest)
                .unwrap(),
        )
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let report = fixture
        .store
        .collect(&fixture.options(0), &unserialized)
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert!(report.skipped_blobs >= 1);
    assert!(!fixture.present(&candidate));
    assert!(fixture.blob(&candidate));
}

#[test]
fn a_candidate_used_during_collection_is_kept_with_its_blobs() {
    let fixture = Fixture::new("racing");
    let candidate = fixture.publish("hot", 1, 1_000, PROTOCOL_VERSION);
    fixture.uses(&[]);
    fixture.age_blobs();
    rotate_journal(&fixture.root).unwrap();
    let snapshot = Snapshot::read(&fixture.store, SystemTime::now(), false).unwrap();
    let options = fixture.options(0);
    let plan = Plan::new(&snapshot, &options, now_ms());
    let mut report = plan.report(&snapshot, &options);
    assert_eq!(report.records_evicted, 1);
    // A hit lands between the snapshot and the eviction.
    fixture
        .store
        .record_use(&candidate.static_key, &candidate.action_key)
        .unwrap();
    plan.apply(&fixture.store, &snapshot, &unserialized, &mut report)
        .unwrap();
    assert_eq!(report.records_evicted, 0);
    assert!(fixture.present(&candidate) && fixture.blob(&candidate));
}

#[test]
fn compaction_keeps_last_use_for_the_next_collection() {
    let fixture = Fixture::new("compact");
    let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
    let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
    fixture.uses(&[(&older, 3_000)]);
    fixture.age_blobs();
    let first = fixture
        .store
        .collect(&fixture.options(u64::MAX), &unserialized)
        .unwrap();
    assert_eq!(first.records_evicted, 0);
    let journals = fs::read_dir(fixture.root.join(JOURNAL_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(journals.len(), 1, "{journals:?}");
    assert!(journals[0].starts_with("compact-"));
    let second = fixture
        .store
        .collect(&fixture.options(100), &unserialized)
        .unwrap();
    assert_eq!(second.records_evicted, 1);
    assert!(fixture.present(&older));
    assert!(!fixture.present(&newer));
}

#[test]
fn blob_bytes_measure_what_a_budget_counts() {
    let fixture = Fixture::new("bytes");
    assert_eq!(fixture.store.blob_bytes().unwrap(), 0);
    fixture.publish("one", 1, 1_000, PROTOCOL_VERSION);
    fixture.publish("two", 2, 2_000, PROTOCOL_VERSION);
    // A write in flight is not stored yet.
    let temp = fixture.store.blob_temp_path(&digest_bytes(b"x")).unwrap();
    fs::write(&temp, vec![0u8; 50]).unwrap();
    assert_eq!(fixture.store.blob_bytes().unwrap(), 200);
    assert_eq!(
        fixture.store.blob_bytes().unwrap(),
        crate::directory_bytes(&fixture.root.join("blobs")).unwrap()
    );
}

#[test]
fn a_collection_steps_aside_while_another_is_running() {
    let fixture = Fixture::new("contended");
    let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
    let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
    fixture.uses(&[]);
    fixture.age_blobs();
    let running = collection_lock(&fixture.root, false).unwrap().unwrap();
    assert!(
        fixture
            .store
            .try_collect_locally(&fixture.options(100))
            .unwrap()
            .is_none()
    );
    assert!(fixture.present(&older) && fixture.present(&newer));
    // A dry run only reads, so it never has to wait.
    let plan = fixture
        .store
        .try_collect_locally(&GcOptions {
            dry_run: true,
            ..fixture.options(100)
        })
        .unwrap()
        .unwrap();
    assert_eq!(plan.records_evicted, 1);
    drop(running);
    let report = fixture
        .store
        .try_collect_locally(&fixture.options(100))
        .unwrap()
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert!(!fixture.present(&older) && fixture.present(&newer));
}

/// Republishes `candidate` as one pinned to a checkout: with the pin a
/// leaking compile records, and with `checkout` when the client knew it.
fn pin(
    fixture: &Fixture,
    mut candidate: ActionCandidate,
    checkout: Option<PinnedCheckout>,
) -> ActionCandidate {
    fs::remove_file(fixture.store.action_path(&candidate.static_key).unwrap()).unwrap();
    candidate.env = vec![crate::EnvInput::capture(
        format!("{PIN_PREFIX}$WORKSPACE"),
        Some("a checkout path"),
    )];
    candidate.action_key = compiler_action_key(
        &candidate.static_key,
        &candidate.files,
        &candidate.host_files,
        &candidate.env,
    );
    candidate.pinned_checkouts = checkout.into_iter().collect();
    fixture.store.put_candidate(candidate.clone(), 8).unwrap();
    candidate
}

fn local(max_bytes: u64) -> GcOptions {
    GcOptions {
        max_bytes,
        evict_missing_checkouts: true,
        ..GcOptions::default()
    }
}

fn scope<'a>(report: &'a GcReport, label: &str) -> Option<&'a GcBucket> {
    report
        .breakdown
        .iter()
        .find(|bucket| bucket.group == "scope" && bucket.label == label)
}

#[test]
fn a_checkout_is_deleted_only_when_it_is_gone_from_a_volume_that_is_there() {
    let fixture = Fixture::new("checkout-state");
    let checkout = fixture.root.join("checkouts").join("one");
    fs::create_dir_all(&checkout).unwrap();
    let located = PinnedCheckout::locate(&checkout).unwrap();
    assert!(Path::new(&located.volume).is_absolute());
    assert!(Path::new(&located.path).starts_with(&located.volume));
    assert!(!located.path.starts_with(r"\\?\"), "{}", located.path);
    assert_eq!(located.state(), CheckoutState::Present);
    fs::remove_dir_all(&checkout).unwrap();
    assert_eq!(located.state(), CheckoutState::Deleted);
    // Its parent directory is gone too: still deleted.
    fs::remove_dir_all(fixture.root.join("checkouts")).unwrap();
    assert_eq!(located.state(), CheckoutState::Deleted);
    assert!(PinnedCheckout::locate(Path::new("relative/checkout")).is_none());
    // A volume that is not there says nothing about what it holds.
    let unplugged = missing_volume(&fixture);
    assert_eq!(unplugged.state(), CheckoutState::Unavailable);
    // Neither does a path this platform cannot have recorded.
    let foreign = if cfg!(windows) {
        ("/home/dev/checkout", "/")
    } else {
        (r"C:\Users\dev\checkout", r"C:\")
    };
    let foreign = PinnedCheckout {
        path: foreign.0.into(),
        volume: foreign.1.into(),
    };
    assert_eq!(foreign.state(), CheckoutState::Unavailable);
}

/// A checkout on a volume that is not mounted.
fn missing_volume(fixture: &Fixture) -> PinnedCheckout {
    if cfg!(windows) {
        let letter = ('D'..='Z')
            .rev()
            .find(|letter| !Path::new(&format!(r"{letter}:\")).exists())
            .expect("a drive letter with no volume");
        PinnedCheckout {
            path: format!(r"{letter}:\work\checkout"),
            volume: format!(r"{letter}:\"),
        }
    } else {
        // An unmounted mount point is an ordinary empty directory.
        let mount = fixture.root.join("mnt");
        fs::create_dir_all(&mount).unwrap();
        PinnedCheckout {
            path: mount.join("checkout").to_string_lossy().into_owned(),
            volume: mount.to_string_lossy().into_owned(),
        }
    }
}

#[test]
fn candidates_pinned_to_a_deleted_checkout_are_evicted_whatever_the_budget() {
    let fixture = Fixture::new("deleted-checkout");
    let kept_dir = fixture.root.join("kept");
    let gone_dir = fixture.root.join("gone");
    fs::create_dir_all(&kept_dir).unwrap();
    fs::create_dir_all(&gone_dir).unwrap();
    // Newest first by use: age alone would evict none of them.
    let now = now_ms();
    let gone_checkout = PinnedCheckout::locate(&gone_dir).unwrap();
    let gone = pin(
        &fixture,
        fixture.publish("gone", 1, now, PROTOCOL_VERSION),
        Some(gone_checkout.clone()),
    );
    let kept = pin(
        &fixture,
        fixture.publish("kept", 2, 2_000, PROTOCOL_VERSION),
        PinnedCheckout::locate(&kept_dir),
    );
    // Stored before checkouts were recorded: treated as it always was.
    let unrecorded = pin(
        &fixture,
        fixture.publish("unrecorded", 3, 3_000, PROTOCOL_VERSION),
        None,
    );
    let unplugged = pin(
        &fixture,
        fixture.publish("unplugged", 4, 4_000, PROTOCOL_VERSION),
        Some(missing_volume(&fixture)),
    );
    let shareable = fixture.publish("shareable", 5, 5_000, PROTOCOL_VERSION);
    fs::remove_dir_all(&gone_dir).unwrap();
    fixture.age_blobs();
    let all = [&gone, &kept, &unrecorded, &unplugged, &shareable];

    // A server's collection never looks for a client's checkout.
    let remote = fixture
        .store
        .collect(&fixture.options(u64::MAX), &unserialized)
        .unwrap();
    assert_eq!(remote.records_evicted, 0);
    assert_eq!(remote.deleted_checkout_records, 0);
    assert_eq!(scope(&remote, "pinned to a checkout").unwrap().records, 4);
    assert!(all.iter().all(|candidate| fixture.present(candidate)));

    // A dry run reports the group and changes nothing.
    let plan = fixture
        .store
        .collect(
            &GcOptions {
                dry_run: true,
                ..local(u64::MAX)
            },
            &unserialized,
        )
        .unwrap();
    assert_eq!((plan.records_evicted, plan.evicted_bytes), (1, 100));
    assert_eq!(
        (plan.deleted_checkout_records, plan.deleted_checkout_bytes),
        (1, 100)
    );
    assert_eq!(plan.deleted_checkouts.len(), 1);
    assert_eq!(plan.deleted_checkouts[0].path, gone_checkout.path);
    assert_eq!(
        (
            plan.deleted_checkouts[0].records,
            plan.deleted_checkouts[0].evicted_bytes
        ),
        (1, 100)
    );
    let deleted = scope(&plan, "pinned to a deleted checkout").unwrap();
    assert_eq!((deleted.records, deleted.evicted_records), (1, 1));
    assert_eq!(scope(&plan, "pinned to a checkout").unwrap().records, 1);
    assert_eq!(
        scope(&plan, "pinned to a checkout out of reach")
            .unwrap()
            .records,
        1
    );
    assert_eq!(
        scope(&plan, "pinned, checkout not recorded")
            .unwrap()
            .records,
        1
    );
    assert!(all.iter().all(|candidate| fixture.present(candidate)));

    // A local collection evicts it, and only it.
    let report = fixture
        .store
        .collect(&local(u64::MAX), &unserialized)
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert_eq!(report.deleted_checkout_records, 1);
    assert!(!fixture.present(&gone) && !fixture.blob(&gone));
    for candidate in [&kept, &unrecorded, &unplugged, &shareable] {
        assert!(fixture.present(candidate) && fixture.blob(candidate));
    }
}

#[test]
fn a_candidate_pinned_to_two_checkouts_dies_with_either() {
    let fixture = Fixture::new("two-checkouts");
    let source = fixture.root.join("source");
    let target = fixture.root.join("elsewhere");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    let mut candidate = fixture.publish("both", 1, now_ms(), PROTOCOL_VERSION);
    candidate = pin(&fixture, candidate, PinnedCheckout::locate(&source));
    fs::remove_file(fixture.store.action_path(&candidate.static_key).unwrap()).unwrap();
    candidate
        .pinned_checkouts
        .push(PinnedCheckout::locate(&target).unwrap());
    fixture.store.put_candidate(candidate.clone(), 8).unwrap();
    fixture.age_blobs();
    let kept = fixture
        .store
        .collect(&local(u64::MAX), &unserialized)
        .unwrap();
    assert_eq!(kept.records_evicted, 0);
    fs::remove_dir_all(&target).unwrap();
    let report = fixture
        .store
        .collect(&local(u64::MAX), &unserialized)
        .unwrap();
    assert_eq!(report.records_evicted, 1);
    assert!(!fixture.present(&candidate));
}

#[test]
fn a_torn_journal_line_is_ignored() {
    let fixture = Fixture::new("torn");
    let candidate = fixture.publish("torn", 1, 1_000, PROTOCOL_VERSION);
    let path = fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&candidate.static_key.as_bytes()[..20])
        .unwrap();
    let uses = read_journal(&fixture.root).unwrap();
    assert_eq!(uses.len(), 1);
}
