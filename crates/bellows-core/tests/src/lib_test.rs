use super::*;

fn valid_candidate() -> ActionCandidate {
    let static_key = digest_bytes(b"static");
    let files = vec![FileInput {
        path: "$WORKSPACE/src/lib.rs".into(),
        digest: digest_bytes(b"source"),
    }];
    let env = vec![EnvInput::capture("MODE", Some("release"))];
    ActionCandidate {
        protocol: PROTOCOL_VERSION,
        action_key: compiler_action_key(&static_key, &files, &[], &env),
        static_key,
        crate_name: "fixture".into(),
        created_ms: 1,
        files,
        host_files: vec![],
        env,
        artifacts: vec![Artifact {
            file_name: "libfixture-abc.rmeta".into(),
            digest: digest_bytes(b"metadata"),
            executable: false,
        }],
        stdout: StreamArtifact {
            digest: digest_bytes(b"stdout"),
            len: 6,
        },
        stderr: StreamArtifact {
            digest: digest_bytes(b"stderr"),
            len: 6,
        },
        proc_macros: vec![],
        pinned_checkouts: vec![],
    }
}

#[cfg(windows)]
#[test]
fn localizes_to_the_ordinary_drive_spelling() {
    let n = PathNormalizer::new(vec![(
        "$TARGET".into(),
        PathBuf::from(r"\\?\C:\work\target"),
    )]);
    for spelling in [
        r"\\?\C:\work\target\out",
        r"C:\work\target\out",
        "C:/work/target/out",
    ] {
        assert!(n.normalize(spelling).starts_with("$TARGET"), "{spelling}");
    }
    assert_eq!(n.localize(r"$TARGET\out"), r"C:\work\target\out");
    assert_eq!(
        n.localize_bytes(br"-L native=$TARGET\out"),
        br"-L native=C:\work\target\out"
    );
}

#[test]
fn normalizes_longest_base_first_and_round_trips() {
    let n = PathNormalizer::new(vec![
        ("$HOME".into(), PathBuf::from("/home/alice")),
        ("$WORKSPACE".into(), PathBuf::from("/home/alice/code/app")),
    ]);
    let normalized = n.normalize("/home/alice/code/app/src/lib.rs:/home/alice/.cargo");
    assert_eq!(normalized, "$WORKSPACE/src/lib.rs:$HOME/.cargo");
    assert_eq!(
        n.localize(&normalized),
        "/home/alice/code/app/src/lib.rs:/home/alice/.cargo"
    );
}

#[test]
fn parses_files_and_environment_from_dep_info() {
    let dep = "target/foo.rlib: src/lib.rs src/a\\ b.rs \\\n src/nested.rs\n\n# env-dep:MODE=fast\n# env-dep:OPTIONAL\n";
    let (files, env) = parse_dep_info(dep);
    assert_eq!(files, vec!["src/a b.rs", "src/lib.rs", "src/nested.rs"]);
    assert_eq!(
        env,
        vec![
            ("MODE".into(), Some("fast".into())),
            ("OPTIONAL".into(), None)
        ]
    );
}

#[test]
fn dep_info_preserves_windows_backslashes_and_escaped_spaces() {
    let dep = concat!(
        "C:\\build\\foo.rlib: C:\\repo\\src\\lib.rs C:\\my\\ project\\a.rs \\\r\n",
        " \\\\server\\share\\source.rs \\\\?\\C:\\long\\source.rs\r\n",
    );
    let (files, _) = parse_dep_info(dep);
    assert_eq!(
        files,
        vec![
            r"C:\my project\a.rs",
            r"C:\repo\src\lib.rs",
            r"\\?\C:\long\source.rs",
            r"\\server\share\source.rs",
        ]
    );
}

#[test]
fn a_candidates_checkouts_are_optional_and_outside_its_key() {
    let mut candidate = valid_candidate();
    // Absent when empty: such a record is what earlier versions wrote.
    let plain = serde_json::to_value(&candidate).unwrap();
    assert!(plain.get("pinned_checkouts").is_none());
    candidate.pinned_checkouts = vec![PinnedCheckout {
        path: "/work/checkout".into(),
        volume: "/".into(),
    }];
    // Not part of the action key: the record is as valid with it.
    validate_candidate_manifest(&candidate).unwrap();
    // A reader that does not know a field ignores it, as a client from
    // before this one ignores `pinned_checkouts`.
    let mut newer = serde_json::to_value(&candidate).unwrap();
    newer["a_field_added_later"] = serde_json::json!(["ignored"]);
    let read: ActionCandidate = serde_json::from_value(newer).unwrap();
    assert_eq!(read, candidate);
    let read: ActionCandidate = serde_json::from_value(plain).unwrap();
    assert!(read.pinned_checkouts.is_empty());
    candidate.pinned_checkouts[0].path.clear();
    assert!(validate_candidate_manifest(&candidate).is_err());
    candidate.pinned_checkouts = vec![
        PinnedCheckout {
            path: "/work/checkout".into(),
            volume: "/".into(),
        };
        MAX_PINNED_CHECKOUTS + 1
    ];
    assert!(validate_candidate_manifest(&candidate).is_err());
}

#[test]
fn a_token_can_be_localized_to_a_chosen_spelling() {
    let mut normalizer = PathNormalizer::new(vec![
        ("$WORKSPACE".into(), "/real/checkout/crate".into()),
        ("$WORKSPACE".into(), "/link/crate".into()),
    ]);
    assert_eq!(
        normalizer.localize("$WORKSPACE/src/lib.rs"),
        "/real/checkout/crate/src/lib.rs"
    );
    normalizer.localize_as("$WORKSPACE", "/link/crate");
    assert_eq!(
        normalizer.localize("$WORKSPACE/src/lib.rs"),
        "/link/crate/src/lib.rs"
    );
    assert_eq!(normalizer.localize_bytes(b"$WORKSPACE/x"), b"/link/crate/x");
    // Every spelling still normalizes.
    assert_eq!(
        normalizer.normalize("/real/checkout/crate/a /link/crate/b"),
        "$WORKSPACE/a $WORKSPACE/b"
    );
    // Only a recognized spelling can be chosen.
    normalizer.localize_as("$WORKSPACE", "/elsewhere");
    assert_eq!(normalizer.localize("$WORKSPACE"), "/link/crate");
}

#[test]
fn store_rejects_corrupt_blob() {
    let root = std::env::temp_dir().join(format!("bellows-core-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let digest = digest_bytes(b"good");
    store.put_blob(&digest, b"good").unwrap();
    fs::write(store.blob_path(&digest).unwrap(), b"bad").unwrap();
    assert!(
        store
            .read_blob(&digest)
            .unwrap_err()
            .to_string()
            .contains("corrupt")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rejects_archive_traversal_paths() {
    assert!(validate_relative_path("bin/test").is_ok());
    assert!(validate_relative_path("../escape").is_err());
    assert!(validate_relative_path("/absolute").is_err());
    assert!(validate_relative_path("a/./b").is_err());
}

#[test]
fn normalized_windows_paths_keep_the_same_traversal_boundary() {
    assert!(validate_normalized_input_path(r"$WORKSPACE\src\lib.rs").is_ok());
    assert!(validate_normalized_input_path(r"$WORKSPACE\..\escape.rs").is_err());
    assert!(validate_normalized_input_path(r"C:\outside\lib.rs").is_err());
    assert!(validate_normalized_input_path(r"\\server\share\lib.rs").is_err());
}

#[test]
fn declared_root_coverage_uses_components_across_path_separators() {
    assert!(relative_path_is_within(
        r"output\release\app.exe",
        "output/release"
    ));
    assert!(relative_path_is_within(
        "output/release/app.exe",
        r"output\release"
    ));
    assert!(relative_path_is_within("output", "output"));
    assert!(!relative_path_is_within("output-other/app.exe", "output"));
    assert!(!relative_path_is_within("other/output/app.exe", "output"));
}

#[test]
fn archive_names_are_publish_once() {
    let root = std::env::temp_dir().join(format!("bellows-archive-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let first = ArchiveManifest {
        protocol: PROTOCOL_VERSION,
        name: "tests-v1".into(),
        tree_digest: tree_digest(&[]),
        created_ms: 1,
        producer_action: None,
        files: vec![],
    };
    assert!(store.put_archive(&first).unwrap());
    assert!(!store.put_archive(&first).unwrap());
    let mut second = first.clone();
    second.files.push(Artifact {
        file_name: "two".into(),
        digest: digest_bytes(b"two"),
        executable: false,
    });
    second.tree_digest = tree_digest(&second.files);
    assert!(store.put_archive(&second).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn declared_keys_ignore_manifest_order() {
    let platform = PlatformIdentity {
        rustc: "rustc test".into(),
        cargo: "cargo test".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        path_digest: digest_bytes(b"path"),
        rustup_home_digest: digest_bytes(b"rustup"),
        rustup_toolchain: None,
        host_cc: None,
        host_ld: None,
        host_ar: None,
    };
    let first = Artifact {
        file_name: "a".into(),
        digest: digest_bytes(b"a"),
        executable: false,
    };
    let second = Artifact {
        file_name: "b".into(),
        digest: digest_bytes(b"b"),
        executable: false,
    };
    let one = declared_action_key(
        "demo",
        &platform,
        &["cargo".into()],
        &BTreeMap::new(),
        &[first.clone(), second.clone()],
        &["out/a".into(), "out/b".into()],
    );
    let two = declared_action_key(
        "demo",
        &platform,
        &["cargo".into()],
        &BTreeMap::new(),
        &[second, first],
        &["out/b".into(), "out/a".into()],
    );
    assert_eq!(one, two);
}

#[test]
fn gc_removes_records_before_referenced_blobs() {
    let root = std::env::temp_dir().join(format!("bellows-gc-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let bytes = b"archive payload";
    let digest = digest_bytes(bytes);
    store.put_blob(&digest, bytes).unwrap();
    store
        .put_archive(&ArchiveManifest {
            protocol: PROTOCOL_VERSION,
            name: "gc-test".into(),
            tree_digest: tree_digest(&[Artifact {
                file_name: "payload".into(),
                digest: digest.clone(),
                executable: false,
            }]),
            created_ms: now_ms(),
            producer_action: None,
            files: vec![Artifact {
                file_name: "payload".into(),
                digest: digest.clone(),
                executable: false,
            }],
        })
        .unwrap();
    // Blobs touched in the last second are kept for a racing publication.
    fs::OpenOptions::new()
        .append(true)
        .open(store.blob_path(&digest).unwrap())
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
        .unwrap();
    let report = store.gc(0).unwrap();
    assert_eq!(report.bytes_after, 0);
    assert!(store.read_archive("gc-test").unwrap().is_none());
    assert!(store.read_blob(&digest).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn declared_commands_cannot_escape_the_sandbox() {
    assert!(
        validate_declared_command(&[
            "cargo".into(),
            "build".into(),
            "--locked".into(),
            "--offline".into(),
            "--manifest-path=fixture/Cargo.toml".into(),
        ])
        .is_ok()
    );
    for command in [
        vec![
            "cargo".into(),
            "build".into(),
            "--locked".into(),
            "--offline".into(),
            "--manifest-path=../../Cargo.toml".into(),
        ],
        vec!["rustc".into(), "/tmp/ambient.rs".into()],
        vec!["rustc".into(), r"\rooted\ambient.rs".into()],
        vec!["rustc".into(), r"C:ambient.rs".into()],
        vec!["/tmp/cargo".into(), "--locked".into(), "--offline".into()],
    ] {
        assert!(validate_declared_command(&command).is_err());
    }
}

#[test]
fn compiler_action_key_is_stable_and_manifest_sensitive() {
    let static_key = digest_bytes(b"static identity");
    let files = vec![FileInput {
        path: "$WORKSPACE/src/lib.rs".into(),
        digest: digest_bytes(b"source"),
    }];
    let environment = vec![EnvInput::capture("MODE", Some("release"))];
    let key = compiler_action_key(&static_key, &files, &[], &environment);
    assert_eq!(key.len(), 64);
    assert_eq!(
        key,
        compiler_action_key(&static_key, &files, &[], &environment)
    );
    assert_ne!(
        key,
        compiler_action_key(&digest_bytes(b"other"), &files, &[], &environment)
    );
}

/// Every stored key and blob address is a BLAKE3 digest. A dependency
/// update that changed its output would silently re-key every store.
#[test]
fn digests_match_the_blake3_reference_vectors() {
    assert_eq!(
        digest_bytes(b""),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
    assert_eq!(
        digest_bytes(b"abc"),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}

#[test]
fn content_keys_are_canonical_blake3_hex() {
    assert!(validate_content_key(&digest_bytes(b"valid")).is_ok());
    assert!(validate_content_key("abc12345").is_err());
    assert!(validate_content_key(&"A".repeat(CONTENT_KEY_LEN)).is_err());
    assert!(validate_content_key(&"g".repeat(CONTENT_KEY_LEN)).is_err());
}

#[test]
fn candidate_manifest_rejects_poisoning_shapes() {
    assert!(validate_candidate_manifest(&valid_candidate()).is_ok());

    let mut wrong_key = valid_candidate();
    wrong_key.action_key = digest_bytes(b"forged");
    assert!(validate_candidate_manifest(&wrong_key).is_err());

    let mut duplicate = valid_candidate();
    duplicate.files.push(duplicate.files[0].clone());
    duplicate.action_key = compiler_action_key(
        &duplicate.static_key,
        &duplicate.files,
        &duplicate.host_files,
        &duplicate.env,
    );
    assert!(validate_candidate_manifest(&duplicate).is_err());

    let mut escaped = valid_candidate();
    escaped.files[0].path = "$WORKSPACE/../../etc/passwd".into();
    escaped.action_key = compiler_action_key(
        &escaped.static_key,
        &escaped.files,
        &escaped.host_files,
        &escaped.env,
    );
    assert!(validate_candidate_manifest(&escaped).is_err());

    let mut absolute = valid_candidate();
    absolute.files[0].path = "/etc/passwd".into();
    absolute.action_key = compiler_action_key(
        &absolute.static_key,
        &absolute.files,
        &absolute.host_files,
        &absolute.env,
    );
    assert!(validate_candidate_manifest(&absolute).is_err());
}

#[test]
fn host_inputs_are_absolute_and_part_of_the_action_key() {
    for path in [
        "/usr/lib/libasound.so",
        r"C:\Windows\System32\kernel32.lib",
        r"\\server\share\a.lib",
    ] {
        assert!(validate_host_input_path(path).is_ok(), "{path}");
    }
    for path in [
        "relative/lib.a",
        "/usr/../etc/passwd",
        "",
        r"C:relative.lib",
    ] {
        assert!(validate_host_input_path(path).is_err(), "{path}");
    }
    let mut candidate = valid_candidate();
    let before = candidate.action_key.clone();
    candidate.host_files.push(FileInput {
        path: "/usr/lib/libz.so".into(),
        digest: digest_bytes(b"zlib"),
    });
    assert!(validate_candidate_manifest(&candidate).is_err());
    candidate.action_key = compiler_action_key(
        &candidate.static_key,
        &candidate.files,
        &candidate.host_files,
        &candidate.env,
    );
    assert_ne!(candidate.action_key, before);
    assert!(validate_candidate_manifest(&candidate).is_ok());
    candidate.host_files[0].path = "$WORKSPACE/relative".into();
    candidate.action_key = compiler_action_key(
        &candidate.static_key,
        &candidate.files,
        &candidate.host_files,
        &candidate.env,
    );
    assert!(validate_candidate_manifest(&candidate).is_err());
}

#[test]
fn normalized_environment_inputs_change_the_action_key() {
    let static_key = digest_bytes(b"static");
    let literal = vec![EnvInput::capture("OUT_DIR", Some("$TARGET/out"))];
    let normalized = vec![EnvInput::capture_normalized("OUT_DIR", "$TARGET/out")];
    assert_eq!(literal[0].value_digest, normalized[0].value_digest);
    assert_ne!(
        compiler_action_key(&static_key, &[], &[], &literal),
        compiler_action_key(&static_key, &[], &[], &normalized)
    );
    // Version-5 records without the field still decode (as literal).
    let decoded: EnvInput = serde_json::from_str(r#"{"name":"A","value_digest":null}"#).unwrap();
    assert!(!decoded.normalized);
}

#[test]
fn corrupt_candidate_indexes_are_quarantined_and_recoverable() {
    let root = std::env::temp_dir().join(format!("bellows-quarantine-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let candidate = valid_candidate();
    store.put_candidate(candidate.clone(), 8).unwrap();
    let path = store.action_path(&candidate.static_key).unwrap();
    fs::write(&path, b"{truncated").unwrap();
    assert!(store.read_candidates(&candidate.static_key).is_err());
    assert!(!path.exists());
    assert_eq!(fs::read_dir(root.join("quarantine")).unwrap().count(), 1);
    store.put_candidate(candidate.clone(), 8).unwrap();
    assert_eq!(
        store
            .read_candidates(&candidate.static_key)
            .unwrap()
            .candidates,
        vec![candidate]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn structurally_invalid_records_are_quarantined_and_recoverable() {
    let root = std::env::temp_dir().join(format!("bellows-validation-quarantine-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let candidate = valid_candidate();
    store.put_candidate(candidate.clone(), 8).unwrap();
    let path = store.action_path(&candidate.static_key).unwrap();
    let mut invalid = candidate.clone();
    invalid.protocol = PROTOCOL_VERSION - 1;
    fs::write(
        &path,
        serde_json::to_vec(&CandidateIndex {
            candidates: vec![invalid],
        })
        .unwrap(),
    )
    .unwrap();
    assert!(store.read_candidates(&candidate.static_key).is_err());
    assert!(!path.exists());
    store.put_candidate(candidate.clone(), 8).unwrap();
    assert_eq!(
        store
            .read_candidates(&candidate.static_key)
            .unwrap()
            .candidates,
        vec![candidate]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn concurrent_candidate_publication_does_not_lose_records() {
    let root = std::env::temp_dir().join(format!("bellows-concurrency-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let mut threads = Vec::new();
    for index in 0..16 {
        let store = store.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut candidate = valid_candidate();
            candidate.files[0].digest = digest_bytes(format!("source-{index}").as_bytes());
            candidate.action_key = compiler_action_key(
                &candidate.static_key,
                &candidate.files,
                &candidate.host_files,
                &candidate.env,
            );
            barrier.wait();
            store.put_candidate(candidate, 32).unwrap();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let index = store.read_candidates(&digest_bytes(b"static")).unwrap();
    assert_eq!(index.candidates.len(), 16);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_aborts_on_malformed_records_without_deleting_blobs() {
    let root = std::env::temp_dir().join(format!("bellows-gc-corrupt-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let bytes = b"live artifact";
    let digest = digest_bytes(bytes);
    store.put_blob(&digest, bytes).unwrap();
    let candidate = valid_candidate();
    store.put_candidate(candidate.clone(), 8).unwrap();
    fs::write(
        store.action_path(&candidate.static_key).unwrap(),
        b"bad json",
    )
    .unwrap();
    assert!(store.gc(0).is_err());
    assert_eq!(store.read_blob(&digest).unwrap(), bytes);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_preserves_a_recent_blob_during_record_publication() {
    let root = std::env::temp_dir().join(format!("bellows-gc-upload-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let bytes = b"publication in progress";
    let digest = digest_bytes(bytes);
    store.put_blob(&digest, bytes).unwrap();
    let report = store.gc(0).unwrap();
    assert_eq!(report.blobs_evicted, 0);
    assert_eq!(store.read_blob(&digest).unwrap(), bytes);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_ignores_cancellation_stranded_atomic_temp_files() {
    let root = std::env::temp_dir().join(format!("bellows-gc-temp-test-{}", now_ms()));
    let store = Store::open(&root).unwrap();
    let directory = root.join("actions").join("aa");
    fs::create_dir_all(&directory).unwrap();
    let temp = directory.join(".tmp-cancelled-publication");
    fs::write(&temp, b"partial json").unwrap();
    assert!(store.gc(0).is_ok());
    assert!(temp.exists());
    let _ = fs::remove_dir_all(root);
}
