use super::*;

#[test]
fn a_budget_is_a_whole_number_of_mebibytes_and_zero_means_unbounded() {
    assert_eq!(Budget::parse(None, None).unwrap(), Budget::Unset);
    assert_eq!(Budget::parse(Some(""), None).unwrap(), Budget::Unset);
    assert_eq!(Budget::parse(Some("0"), None).unwrap(), Budget::Unbounded);
    assert_eq!(
        Budget::parse(Some(" 100000 "), Some("20000")).unwrap(),
        Budget::Limited {
            max_bytes: 100_000 * MEBIBYTE,
            max_executable_bytes: Some(20_000 * MEBIBYTE),
        }
    );
    for (max, executables) in [
        (Some("100GB"), None),
        (Some("-1"), None),
        (Some("1.5"), None),
        (Some("100"), Some("many")),
        // A share of a budget that does not exist.
        (None, Some("10")),
        (Some("0"), Some("10")),
    ] {
        assert!(
            Budget::parse(max, executables).is_err(),
            "{max:?} {executables:?}"
        );
    }
}

#[test]
fn a_build_collects_only_above_the_budget_and_then_to_four_fifths_of_it() {
    let budget = Budget::Limited {
        max_bytes: 1000 * MEBIBYTE,
        max_executable_bytes: None,
    };
    assert_eq!(budget.action(1000 * MEBIBYTE), BudgetAction::Nothing);
    assert_eq!(
        budget.action(1000 * MEBIBYTE + 1),
        BudgetAction::Collect {
            max_bytes: 1000 * MEBIBYTE,
            to_bytes: 800 * MEBIBYTE,
            max_executable_bytes: None,
        }
    );
    // Without a budget nothing is collected. A large store is reported,
    // unless it is unbounded on purpose.
    assert_eq!(
        Budget::Unset.action(UNBUDGETED_NOTICE_BYTES - 1),
        BudgetAction::Nothing
    );
    assert_eq!(
        Budget::Unset.action(UNBUDGETED_NOTICE_BYTES),
        BudgetAction::Notice
    );
    assert_eq!(Budget::Unbounded.action(u64::MAX), BudgetAction::Nothing);
    assert_eq!(Budget::collect_to(u64::MAX), u64::MAX / 100 * 80);
}

#[test]
fn budget_settings_never_reach_a_cache_key() {
    for name in ["BELLOWS_MAX_MB", "BELLOWS_MAX_EXECUTABLE_MB"] {
        assert!(!is_relevant_environment_name(name), "{name}");
    }
}

#[test]
fn protocol_skew_is_rejected_in_both_directions() {
    assert!(validate_protocol(PROTOCOL_VERSION).is_ok());
    assert!(validate_protocol(PROTOCOL_VERSION - 1).is_err());
    assert!(validate_protocol(PROTOCOL_VERSION + 1).is_err());
}

#[test]
fn remote_configuration_rejects_unsafe_urls_and_unbounded_timeouts() {
    for url in [
        "file:///tmp/cache",
        "http://user:secret@localhost:7878",
        "http://localhost:7878?token=secret",
        "://not-a-url",
    ] {
        assert!(Remote::new(url, None).is_err(), "accepted {url}");
    }
    assert!(Remote::new("http://127.0.0.1:7878", None).is_ok());
    assert!(bounded_timeout("TEST", Some("99"), 2_000, 100, 30_000).is_err());
    assert!(bounded_timeout("TEST", Some("30001"), 2_000, 100, 30_000).is_err());
    assert_eq!(
        bounded_timeout("TEST", Some("2500"), 2_000, 100, 30_000).unwrap(),
        Duration::from_millis(2_500)
    );
}

#[cfg(unix)]
#[test]
fn normalizer_bases_resolve_symlinked_mounts() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    fs::create_dir(&real).unwrap();
    symlink(&real, &alias).unwrap();
    assert_eq!(canonical_base(alias), real.canonicalize().unwrap());
}

#[test]
fn ordinary_spellings_drop_verbatim_prefixes() {
    assert_eq!(
        ordinary_spelling(Path::new(r"\\?\C:\Users\x")),
        r"C:\Users\x"
    );
    assert_eq!(
        ordinary_spelling(Path::new(r"\\?\UNC\server\share\x")),
        r"\\server\share\x"
    );
    assert_eq!(ordinary_spelling(Path::new("/home/x")), "/home/x");
}

#[cfg(unix)]
#[test]
fn a_checkout_found_from_the_crate_root_keeps_cargos_spelling() {
    use std::os::unix::fs::symlink;

    // The checkout is reached through an alias (a symlink here, an 8.3
    // name on Windows); the working directory is outside it.
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real checkout");
    fs::create_dir_all(real.join(".git")).unwrap();
    fs::create_dir_all(real.join("ui")).unwrap();
    fs::write(real.join("ui/wrong.rs"), "").unwrap();
    let alias = temp.path().join("alias");
    symlink(&real, &alias).unwrap();
    let elsewhere = temp.path().join("target/tests/trybuild/ui");
    fs::create_dir_all(&elsewhere).unwrap();
    let source = alias.join("ui/wrong.rs");
    let checkout = compile_checkout(&elsewhere, Some(&source)).unwrap();
    assert_eq!(checkout, real.canonicalize().unwrap());
    assert_eq!(
        source_spelling(&elsewhere, Some(&source), &checkout),
        Some(alias)
    );
    assert_eq!(compile_checkout(&elsewhere, None), None);
}

#[test]
fn infers_custom_cargo_target_roots_from_deps_outputs() {
    assert_eq!(
        infer_target_root(Path::new("/tmp/runner-a/release/deps")),
        Some(PathBuf::from("/tmp/runner-a"))
    );
    assert_eq!(
        infer_target_root(Path::new(
            "/tmp/runner-b/wasm32-unknown-unknown/release/deps"
        )),
        Some(PathBuf::from("/tmp/runner-b/wasm32-unknown-unknown"))
    );
}

fn test_identity(workspace: &Path) -> Identity {
    let bases = vec![("$WORKSPACE".to_owned(), workspace.to_path_buf())];
    Identity {
        static_key: digest_bytes(b"identity"),
        fingerprint: diagnostics::Fingerprint::default(),
        diagnostic_group: String::new(),
        normalizer: PathNormalizer::new(bases.clone()),
        root_normalizer: PathNormalizer::new(bases),
        workspace: workspace.to_path_buf(),
        digests: digests::Digests::uncached(),
        pins: vec![(
            format!("{PIN_PREFIX}$WORKSPACE"),
            workspace.to_string_lossy().into_owned(),
        )],
        virtual_env: BTreeMap::new(),
        checkout: None,
        own_roots: vec![workspace.to_path_buf()],
        diagnostic_width: None,
    }
}

fn test_invocation(out_dir: &Path, expected: &[&str], inputs: Vec<PathBuf>) -> Invocation {
    Invocation {
        rustc: PathBuf::from("rustc"),
        args: vec![],
        crate_name: "fixture".into(),
        out_dir: out_dir.to_path_buf(),
        kind: OutputKind::Library,
        naming: Naming::for_target(None),
        stems: vec!["fixture-abc".into(), "libfixture-abc".into()],
        expected_names: expected.iter().map(|name| (*name).to_owned()).collect(),
        explicit_inputs: inputs,
        proc_macros: vec![],
        incremental: None,
        unit: "fixture-abc".into(),
        proc_macro_crate: false,
        static_libraries: vec![],
        native_search: vec![],
        response_file: false,
    }
}

fn empty_candidate(identity: &Identity, protocol: u32) -> ActionCandidate {
    ActionCandidate {
        protocol,
        static_key: identity.static_key.clone(),
        action_key: compiler_action_key(&identity.static_key, &[], &[], &[]),
        crate_name: "fixture".into(),
        created_ms: 0,
        files: vec![],
        host_files: vec![],
        env: vec![],
        artifacts: vec![],
        stdout: StreamArtifact {
            digest: digest_bytes(b""),
            len: 0,
        },
        stderr: StreamArtifact {
            digest: digest_bytes(b""),
            len: 0,
        },
        proc_macros: vec![],
        pinned_checkouts: vec![],
    }
}

#[test]
fn a_pinned_candidate_names_its_checkout_locally_and_never_to_a_server() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // A git checkout holding a package and its target directory.
    let checkout = root.join("checkout");
    let package = checkout.join("crates").join("fixture");
    fs::create_dir_all(&package).unwrap();
    fs::create_dir_all(checkout.join(".git")).unwrap();
    fs::create_dir_all(checkout.join("target")).unwrap();
    let workspace_pin = format!("{PIN_PREFIX}$WORKSPACE");
    let target_pin = format!("{PIN_PREFIX}$TARGET");
    let mut identity = test_identity(&package);
    identity.pins.push((
        target_pin.clone(),
        checkout.join("target").to_string_lossy().into_owned(),
    ));
    let pins = |identity: &Identity, names: &[&String]| {
        names
            .iter()
            .map(|name| EnvInput::capture(name.as_str(), identity.pin_value(name)))
            .collect::<Vec<_>>()
    };
    let expected = PinnedCheckout::locate(&checkout).unwrap();
    // Both roots lie in one checkout. It is named once, and the target
    // directory, which `cargo clean` removes, is not named itself.
    assert_eq!(
        pinned_checkouts(&identity, &pins(&identity, &[&workspace_pin, &target_pin])),
        vec![expected.clone()]
    );
    assert!(pinned_checkouts(&identity, &[EnvInput::capture("MODE", Some("release"))]).is_empty());
    // A target directory shared by several checkouts belongs to none.
    if git_root(&root).is_none() {
        let shared = root.join("shared-target");
        fs::create_dir_all(&shared).unwrap();
        identity.pins[1].1 = shared.to_string_lossy().into_owned();
        assert!(pinned_checkouts(&identity, &pins(&identity, &[&target_pin])).is_empty());
    }

    // A candidate restored from a server carries no checkout; the local
    // store's copy names this compile's, under the same action key.
    let mut candidate = empty_candidate(&identity, PROTOCOL_VERSION);
    candidate.env = pins(&identity, &[&workspace_pin]);
    candidate.action_key = compiler_action_key(&identity.static_key, &[], &[], &candidate.env);
    let local = for_local_store(&candidate, &identity);
    assert_eq!(local.pinned_checkouts, vec![expected]);
    assert_eq!(local.action_key, candidate.action_key);
    validate_candidate_manifest(&local).unwrap();
    let sent = for_server(&local);
    assert_eq!(sent, candidate);
    assert!(
        !serde_json::to_string(&sent)
            .unwrap()
            .contains("pinned_checkouts")
    );
}

/// Links `link` to the directory `target`: a symlink, or on Windows a
/// junction, which needs no privilege.
#[cfg(any(unix, windows))]
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    assert!(
        Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

/// `<root>/real/{pkg/src/lib.rs, shared/data.txt}` and `<root>/outside`,
/// with `<root>/link` leading to `real` and `pkg/selected` to `outside`.
/// Returns the canonical `real`, and `link` as Cargo would spell it.
#[cfg(any(unix, windows))]
fn linked_tree(temp: &Path) -> (PathBuf, PathBuf) {
    let root = temp.canonicalize().unwrap();
    let real = root.join("real");
    for (directory, file) in [
        (real.join("pkg").join("src"), "lib.rs"),
        (real.join("shared"), "data.txt"),
        (root.join("outside"), "value.rs"),
    ] {
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(file), "contents").unwrap();
    }
    let link = PathBuf::from(ordinary_spelling(&root)).join("link");
    link_dir(&real, &link);
    link_dir(&root.join("outside"), &real.join("pkg").join("selected"));
    (real, link)
}

#[cfg(any(unix, windows))]
#[test]
fn a_link_is_followed_only_at_or_above_the_compiles_own_roots() {
    let temp = tempfile::tempdir().unwrap();
    let (real, link) = linked_tree(temp.path());
    let package = real.join("pkg");
    let spelled = link.join("pkg");
    let source = package.join("src").join("lib.rs");
    let refused = |path: &Path, roots: &[PathBuf]| {
        let error = canonical_compiler_input(path, roots).unwrap_err();
        assert!(
            format!("{error:#}").contains("symlinked compiler input is not cacheable"),
            "{error:#}"
        );
    };

    // No link on the way: as it always was, whatever the roots.
    assert_eq!(canonical_compiler_input(&source, &[]).unwrap(), source);
    // Reached through the spelling of the compile's own root: the record
    // names the file under that root, so the link is the root's affair.
    let own = vec![package.clone(), spelled.clone()];
    assert_eq!(
        canonical_compiler_input(&spelled.join("src").join("lib.rs"), &own).unwrap(),
        source
    );
    assert_eq!(
        canonical_compiler_input(
            &spelled.join("src").join("..").join("src").join("lib.rs"),
            &own
        )
        .unwrap(),
        source
    );
    // The same path when the link is not a spelling of a root.
    refused(
        &spelled.join("src").join("lib.rs"),
        std::slice::from_ref(&package),
    );
    // A link below the root still loses what it pointed at.
    for base in [&spelled, &package] {
        refused(&base.join("selected").join("value.rs"), &own);
    }
    // A file beside the package is inside the link only as far as the
    // checkout the link stands for is one of the roots.
    let sibling = spelled
        .join("src")
        .join("..")
        .join("..")
        .join("shared")
        .join("data.txt");
    refused(&sibling, &own);
    let with_checkout = [own, vec![real.clone(), link.clone()]].concat();
    assert_eq!(
        canonical_compiler_input(&sibling, &with_checkout).unwrap(),
        real.join("shared").join("data.txt")
    );
    // `..` straight after a link is not judged by its spelling.
    refused(
        &link
            .join("..")
            .join("real")
            .join("pkg")
            .join("src")
            .join("lib.rs"),
        &with_checkout,
    );
}

#[cfg(any(unix, windows))]
#[test]
fn own_roots_are_the_spellings_that_resolve_to_this_compiles_roots() {
    let temp = tempfile::tempdir().unwrap();
    let (real, link) = linked_tree(temp.path());
    let package = real.join("pkg");
    let elsewhere = real.parent().unwrap().join("outside");
    let mut normalizer = PathNormalizer::new(vec![
        ("$WORKSPACE".into(), package.clone()),
        ("$WORKSPACE".into(), link.join("pkg")),
        // Recorded as a spelling, but it leads somewhere else.
        (
            "$WORKSPACE".into(),
            PathBuf::from(ordinary_spelling(&elsewhere)),
        ),
        ("$CHECKOUT".into(), link.clone()),
        ("$CHECKOUT".into(), real.clone()),
    ]);
    let roots = own_roots(&package, Some(&real), &normalizer);
    for root in [&package, &link.join("pkg"), &real, &link] {
        assert!(roots.contains(root), "{root:?} in {roots:?}");
    }
    assert!(
        !roots.iter().any(|root| root.ends_with("outside")),
        "{roots:?}"
    );

    // A restore spells each root through its link, as rustc would.
    assert_eq!(
        normalizer.localize("$WORKSPACE"),
        ordinary_spelling(&package)
    );
    let mut root_normalizer = normalizer.clone();
    localize_through_links(
        [&mut normalizer, &mut root_normalizer],
        &package,
        Some(&real),
    );
    for normalizer in [&normalizer, &root_normalizer] {
        assert_eq!(
            normalizer.localize("$WORKSPACE"),
            link.join("pkg").to_string_lossy()
        );
        assert_eq!(normalizer.localize("$CHECKOUT"), link.to_string_lossy());
    }
    // A root reached plainly has no such spelling.
    let plain = PathNormalizer::new(vec![("$WORKSPACE".into(), package.clone())]);
    assert_eq!(link_spelling(&plain, "$WORKSPACE", &package), None);
    assert_eq!(own_roots(&package, None, &plain).len(), 2);
}

#[test]
fn a_pin_covers_every_spelling_that_reaches_its_root() {
    let (canonical, link, other) = if cfg!(windows) {
        (
            r"\\?\C:\work\checkout",
            r"C:\link\checkout",
            r"C:\other\checkout",
        )
    } else {
        ("/work/checkout", "/link/checkout", "/other/checkout")
    };
    let spellings = |alias: Option<&str>| {
        let mut bases = vec![("$WORKSPACE".to_owned(), PathBuf::from(canonical))];
        bases.extend(alias.map(|alias| ("$WORKSPACE".to_owned(), PathBuf::from(alias))));
        PathNormalizer::new(bases).spellings("$WORKSPACE")
    };
    // Reached plainly: the canonical path alone, as before, so pinned
    // candidates stored by earlier versions still match.
    assert_eq!(pin_value(canonical, &spellings(None)), canonical);
    // Reached through a link: only by a build that spells it the same.
    let linked = pin_value(canonical, &spellings(Some(link)));
    assert_ne!(linked, canonical);
    assert_eq!(linked.lines().next(), Some(canonical));
    assert!(linked.contains(link), "{linked}");
    assert_ne!(linked, pin_value(canonical, &spellings(Some(other))));
    assert_eq!(linked, pin_value(canonical, &spellings(Some(link))));
    // A shell that writes the drive letter in lower case reaches the
    // same root the same way: its pinned results are not set apart.
    if cfg!(windows) {
        assert_eq!(
            pin_value(canonical, &spellings(Some(r"c:\work\checkout"))),
            canonical
        );
    }
}

#[test]
fn checkout_pins_and_normalized_environment_validate_per_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let identity = test_identity(&first);
    let mut candidate = empty_candidate(&identity, PROTOCOL_VERSION);
    let pin = format!("{PIN_PREFIX}$WORKSPACE");
    candidate.env = vec![EnvInput::capture(&pin, identity.pin_value(&pin))];
    candidate.action_key = compiler_action_key(&candidate.static_key, &[], &[], &candidate.env);
    assert!(validate_candidate(&candidate, &identity).is_ok());
    let other = test_identity(&second);
    let reason = validate_candidate(&candidate, &other).unwrap_err();
    assert!(reason.contains("another checkout's path"), "{reason}");

    // A normalized env-dep matches the same relative location anywhere.
    let name = "BELLOWS_TEST_NORMALIZED_DIR";
    // SAFETY: only this test reads or writes this variable.
    unsafe { env::set_var(name, second.join("out")) };
    let mut normalized = empty_candidate(&other, PROTOCOL_VERSION);
    normalized.env = vec![EnvInput::capture_normalized(
        name,
        &format!("$WORKSPACE{}out", std::path::MAIN_SEPARATOR),
    )];
    normalized.action_key = compiler_action_key(&normalized.static_key, &[], &[], &normalized.env);
    assert!(validate_candidate(&normalized, &other).is_ok());
    unsafe { env::set_var(name, second.join("elsewhere")) };
    assert!(validate_candidate(&normalized, &other).is_err());
    unsafe { env::remove_var(name) };
}

fn ar_archive(members: &[&str]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for name in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            644,
            2
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(b"xx");
    }
    out
}

#[test]
fn response_files_expand_into_the_identity_and_feed_the_compile() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("cargo-args");
    fs::write(
        &file,
        "--crate-name\nfixture\n--edition=2024\r\nsrc/lib.rs\n",
    )
    .unwrap();
    let args = vec![
        "-C".into(),
        "opt-level=3".into(),
        format!("@{}", file.display()),
    ];
    assert_eq!(
        expand_response_files(args).unwrap(),
        [
            "-C",
            "opt-level=3",
            "--crate-name",
            "fixture",
            "--edition=2024",
            "src/lib.rs"
        ]
    );
    assert!(expand_response_files(vec!["@shell:x".into()]).is_err());
    assert!(
        expand_response_files(vec![format!("@{}", temp.path().join("gone").display())]).is_err()
    );

    let mut invocation = test_invocation(temp.path(), &[], vec![]);
    let args = ["--crate-name", "fixture", "src/lib.rs"].map(str::to_owned);
    let (command, file) = rustc_command(&invocation, &args).unwrap();
    assert!(file.is_none());
    assert_eq!(command.get_args().count(), 3);
    invocation.response_file = true;
    let (command, file) = rustc_command(&invocation, &args).unwrap();
    let file = file.expect("response file");
    let passed = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(passed, [format!("@{}", file.display())]);
    assert_eq!(fs::read_to_string(&file).unwrap(), args.join("\n"));
}

#[test]
fn raw_dylib_import_members_are_stored_unless_a_searched_archive_supplies_them() {
    let temp = tempfile::tempdir().unwrap();
    let mut invocation = test_invocation(temp.path(), &[], vec![]);
    let rlib = ar_archive(&["lib.rmeta", "x.rcgu.o", "kernel32.dll"]);
    assert!(check_bundled_members(&invocation, &rlib, temp.path()).is_ok());

    let unknown = ar_archive(&["lib.rmeta", "vendored.o"]);
    assert!(check_bundled_members(&invocation, &unknown, temp.path()).is_err());

    // An import library in a searched directory could be the bundled
    // source of the same member: that stays unmodeled.
    let search = temp.path().join("native");
    fs::create_dir_all(&search).unwrap();
    fs::write(search.join("kernel32.lib"), ar_archive(&["KERNEL32.dll"])).unwrap();
    invocation.native_search = vec![search];
    let error = check_bundled_members(&invocation, &rlib, temp.path()).unwrap_err();
    assert!(error.to_string().contains("kernel32.dll"), "{error}");
}

#[test]
fn diagnostic_width_is_checked_only_for_candidates_with_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let narrow = Identity {
        diagnostic_width: Some("80".into()),
        ..test_identity(temp.path())
    };
    let wide = Identity {
        diagnostic_width: Some("200".into()),
        ..test_identity(temp.path())
    };
    let unset = test_identity(temp.path());
    let with_width = |value: &str| {
        let mut candidate = empty_candidate(&unset, PROTOCOL_VERSION);
        candidate.env = vec![EnvInput::capture(DIAGNOSTIC_WIDTH_INPUT, Some(value))];
        candidate.action_key = compiler_action_key(&candidate.static_key, &[], &[], &candidate.env);
        candidate
    };
    let silent = with_width(ANY_DIAGNOSTIC_WIDTH);
    for identity in [&narrow, &wide, &unset] {
        assert!(validate_candidate(&silent, identity).is_ok());
    }
    let rendered = with_width("80");
    assert!(validate_candidate(&rendered, &narrow).is_ok());
    let reason = validate_candidate(&rendered, &wide).unwrap_err();
    assert!(reason.contains("diagnostic width"), "{reason}");
    assert!(validate_candidate(&rendered, &unset).is_err());
    let rendered_unset = with_width("absent");
    assert!(validate_candidate(&rendered_unset, &unset).is_ok());
    assert!(validate_candidate(&rendered_unset, &narrow).is_err());
    // Candidates from before the width input were compiled without one.
    let legacy = empty_candidate(&unset, PROTOCOL_VERSION);
    assert!(validate_candidate(&legacy, &unset).is_ok());
    assert!(validate_candidate(&legacy, &narrow).is_err());

    let args = [
        "--crate-name",
        "x",
        "--diagnostic-width=120",
        "--diagnostic-width",
        "90",
    ]
    .map(str::to_owned);
    assert_eq!(diagnostic_width(&args).as_deref(), Some("120"));
    let mut invocation = test_invocation(temp.path(), &[], vec![]);
    invocation.args = args.to_vec();
    let normalizer = PathNormalizer::new(vec![]);
    assert_eq!(
        normalized_compiler_arguments(&invocation, &normalizer),
        ["--crate-name", "x"]
    );
    assert!(!has_rendered_diagnostics(
        b"{\"$message_type\":\"artifact\",\"artifact\":\"/t/libx.rmeta\",\"emit\":\"metadata\"}\n"
    ));
    assert!(has_rendered_diagnostics(
        b"{\"$message_type\":\"diagnostic\",\"message\":\"unused variable\"}\n"
    ));
    assert!(!has_rendered_diagnostics(b"\n"));
}

#[test]
fn prior_protocol_candidates_are_cleanly_rejected() {
    let workspace = std::env::temp_dir().join(format!("bellows-protocol-test-{}", now_ms()));
    let identity = test_identity(&workspace);
    let candidate = empty_candidate(&identity, PROTOCOL_VERSION - 1);
    assert_eq!(
        validate_candidate(&candidate, &identity).unwrap_err(),
        format!("unsupported candidate protocol {}", PROTOCOL_VERSION - 1)
    );
}

#[test]
fn captured_stream_lengths_describe_normalized_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp
        .path()
        .canonicalize()
        .unwrap()
        .join("a-very-long-workspace-name");
    let out_dir = workspace.join("target/debug/deps");
    fs::create_dir_all(&out_dir).unwrap();
    let source = workspace.join("src/lib.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();
    let dep_name = "fixture-abc.d";
    let rmeta_name = "libfixture-abc.rmeta";
    fs::write(out_dir.join(rmeta_name), b"metadata").unwrap();
    fs::write(
        out_dir.join(dep_name),
        format!(
            "{rmeta_name}: {}\n",
            source.to_string_lossy().replace(' ', "\\ ")
        ),
    )
    .unwrap();
    let invocation = test_invocation(&out_dir, &[dep_name, rmeta_name], vec![source]);
    let identity = test_identity(&workspace);
    let stdout = format!("compiled {}", workspace.display()).into_bytes();
    let stderr = format!("warning in {}", workspace.display()).into_bytes();
    let captured = capture_outputs(
        &invocation,
        &identity,
        stdout.clone(),
        stderr.clone(),
        // The compile started after the source was written.
        std::time::SystemTime::now() + Duration::from_secs(10),
        None,
    )
    .unwrap();
    assert!(!captured.pinned);
    assert_eq!(
        captured.candidate.files[0].path,
        format!("$WORKSPACE{0}src{0}lib.rs", std::path::MAIN_SEPARATOR)
    );
    assert_eq!(
        captured.candidate.stdout.len,
        identity.normalizer.normalize_bytes(&stdout).len() as u64
    );
    assert_eq!(
        captured.candidate.stderr.len,
        identity.normalizer.normalize_bytes(&stderr).len() as u64
    );
    assert_ne!(captured.candidate.stdout.len, stdout.len() as u64);
}

#[test]
fn recognizes_rustc_and_clippy_wrapper_invocations() {
    for compiler in ["rustc", "rustc-1.92.0", "clippy-driver"] {
        assert!(is_wrapper_invocation(&[
            OsString::from("bellows"),
            OsString::from(compiler),
            OsString::from("-vV"),
        ]));
    }
    assert!(!is_wrapper_invocation(&[
        OsString::from("bellows"),
        OsString::from("doctor"),
    ]));
}

#[test]
fn cargo_shorthand_accepts_ordinary_cargo_flags() {
    let cli =
        Cli::try_parse_from(["bellows", "cargo", "run", "--release", "-p", "flagship"]).unwrap();
    let Commands::Cargo { arguments } = cli.command else {
        panic!("cargo shorthand parsed as the wrong command");
    };
    assert_eq!(
        arguments,
        ["run", "--release", "-p", "flagship"].map(OsString::from)
    );
}

#[test]
fn parses_cacheable_library_invocation() {
    let dir = std::env::temp_dir().join(format!("bellows-cli-test-{}", now_ms()));
    fs::create_dir_all(dir.join("out")).unwrap();
    fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
    let raw = vec![
        OsString::from("rustc"),
        OsString::from("--crate-name"),
        OsString::from("demo"),
        dir.join("lib.rs").into_os_string(),
        OsString::from("--crate-type"),
        OsString::from("lib"),
        OsString::from("--emit=dep-info,metadata,link"),
        OsString::from("-C"),
        OsString::from("extra-filename=-abc"),
        OsString::from("--out-dir"),
        dir.join("out").into_os_string(),
    ];
    let invocation = Invocation::analyze(&raw).unwrap();
    assert!(invocation.expected_names.contains("demo-abc.d"));
    assert!(invocation.expected_names.contains("libdemo-abc.rlib"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rejects_all_unmodeled_emit_and_custom_output_forms() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("lib.rs");
    fs::write(&source, "pub fn value() {}").unwrap();
    for extra in [
        vec!["--emit=dep-info,metadata,asm"],
        vec!["--emit=dep-info,metadata=custom.rmeta"],
        vec!["--emit=dep-info,metadata", "-ocustom"],
        vec!["--emit=dep-info,metadata", "-o", "custom"],
        vec!["--emit=dep-info,metadata", "--emit=link"],
    ] {
        let mut raw = [
            "rustc",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "-Cextra-filename=-fixture",
            "--out-dir=out",
        ]
        .map(OsString::from)
        .to_vec();
        raw.push(source.clone().into_os_string());
        raw.extend(extra.iter().map(OsString::from));
        assert!(
            Invocation::analyze(&raw)
                .unwrap_err()
                .contains("unsupported emit")
        );
    }
}

#[test]
fn accepts_inert_library_link_arguments() {
    let dir = std::env::temp_dir().join(format!("bellows-cli-link-arg-{}", now_ms()));
    fs::create_dir_all(dir.join("out")).unwrap();
    fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
    let base = vec![
        OsString::from("rustc"),
        OsString::from("--crate-name"),
        OsString::from("demo"),
        dir.join("lib.rs").into_os_string(),
        OsString::from("--crate-type=rlib"),
        OsString::from("--emit=dep-info,metadata,link"),
        OsString::from("-Cextra-filename=-abc"),
        OsString::from("--out-dir"),
        dir.join("out").into_os_string(),
    ];

    for link_args in [
        vec![
            OsString::from("-C"),
            OsString::from("link-arg=-fuse-ld=lld"),
        ],
        vec![OsString::from("-Clink-arg=-fuse-ld=lld")],
        vec![OsString::from("-Clink-args=-fuse-ld=lld")],
        vec![
            OsString::from("-C"),
            OsString::from("link-arg=-Wl,-rpath,/x"),
        ],
    ] {
        let mut raw = base.clone();
        raw.extend(link_args);
        Invocation::analyze(&raw).unwrap();
    }
    let _ = fs::remove_dir_all(dir);
}

fn analyze_with(dir: &Path, extra: &[&str]) -> std::result::Result<Invocation, String> {
    fs::create_dir_all(dir.join("out")).unwrap();
    fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
    let mut raw = vec![
        OsString::from("rustc"),
        OsString::from("--crate-name=demo"),
        dir.join("lib.rs").into_os_string(),
        OsString::from("-Cextra-filename=-abc"),
        OsString::from("--out-dir"),
        dir.join("out").into_os_string(),
    ];
    raw.extend(extra.iter().map(OsString::from));
    Invocation::analyze(&raw)
}

#[test]
fn linked_crate_types_and_test_harnesses_are_modeled() {
    let temp = tempfile::tempdir().unwrap();
    let exe = std::env::consts::EXE_SUFFIX;
    let linked = |extra: &[&str]| analyze_with(temp.path(), extra).unwrap();

    let test = linked(&["--emit=dep-info,link", "--test"]);
    assert_eq!(test.kind, OutputKind::Linked);
    assert!(test.expected_names.contains(&format!("demo-abc{exe}")));
    assert!(test.owns("demo-abc.pdb") && test.owns("demo-abc") && test.owns("demo-abc.d"));
    assert!(!test.owns("demo-abcd") && !test.owns("demo-abc2.pdb"));

    let bin = linked(&[
        "--crate-type=bin",
        "--emit=dep-info,link",
        "-Clink-arg=-fuse-ld=lld",
    ]);
    assert_eq!(bin.kind, OutputKind::Linked);

    let cdylib = linked(&["--crate-type=rlib,cdylib", "--emit=dep-info,metadata,link"]);
    assert_eq!(cdylib.kind, OutputKind::Linked);
    assert!(cdylib.expected_names.contains("libdemo-abc.rlib"));
    let dll = Naming::for_target(None);
    assert!(
        cdylib
            .expected_names
            .contains(&format!("{}demo-abc{}", dll.dll_prefix, dll.dll_suffix))
    );

    // Checking a test target writes metadata only: no linker runs.
    let check = linked(&["--emit=dep-info,metadata", "--test"]);
    assert_eq!(check.kind, OutputKind::Library);
    assert!(check.expected_names.contains("libdemo-abc.rmeta"));

    let staticlib = analyze_with(
        temp.path(),
        &["--crate-type=staticlib", "--emit=dep-info,link"],
    );
    assert!(staticlib.unwrap_err().contains("staticlib"));
}

#[test]
fn target_naming_follows_the_triple() {
    let windows = Naming::for_target(Some("x86_64-pc-windows-msvc"));
    assert_eq!(
        (windows.exe_suffix, windows.dll_suffix, windows.msvc),
        (".exe", ".dll", true)
    );
    let wasm = Naming::for_target(Some("wasm32-unknown-unknown"));
    assert_eq!(
        (wasm.exe_suffix, wasm.dll_prefix, wasm.dll_suffix),
        (".wasm", "", ".wasm")
    );
    let linux = Naming::for_target(Some("x86_64-unknown-linux-gnu"));
    assert_eq!(
        (linux.exe_suffix, linux.dll_prefix, linux.dll_suffix),
        ("", "lib", ".so")
    );
}

#[test]
fn incremental_invocations_share_the_non_incremental_identity() {
    let temp = tempfile::tempdir().unwrap();
    let emit = ["--crate-type=rlib", "--emit=dep-info,link"];
    let plain = analyze_with(temp.path(), &emit).unwrap();
    let spaced = analyze_with(
        temp.path(),
        &[emit[0], emit[1], "-C", "incremental=/x/incr"],
    )
    .unwrap();
    let joined = analyze_with(temp.path(), &[emit[0], emit[1], "-Cincremental=/y/incr"]).unwrap();
    assert_eq!(spaced.incremental, Some(PathBuf::from("/x/incr")));
    let normalizer = PathNormalizer::new(vec![]);
    let args = normalized_compiler_arguments(&plain, &normalizer);
    assert_eq!(normalized_compiler_arguments(&spaced, &normalizer), args);
    assert_eq!(normalized_compiler_arguments(&joined, &normalizer), args);
}

#[test]
fn proc_macro_consumers_record_macro_names() {
    let temp = tempfile::tempdir().unwrap();
    let so = temp.path().join("libderive-abc.so");
    fs::write(&so, b"dylib").unwrap();
    let external = format!("derive={}", so.display());
    let invocation = analyze_with(
        temp.path(),
        &[
            "--crate-type=rlib",
            "--emit=dep-info,metadata,link",
            "--extern",
            &external,
            "--extern",
            "proc_macro",
        ],
    )
    .unwrap();
    assert_eq!(invocation.proc_macros, ["derive"]);
    assert!(invocation.explicit_inputs.contains(&so));
}

#[test]
fn bypasses_host_dependent_and_unmodeled_toolchain_inputs() {
    for extra in [
        vec!["-Ctarget-cpu=native"],
        vec!["-C", "target-cpu=native"],
        vec!["--sysroot", "/custom/toolchain"],
        vec!["--target=custom.json"],
        vec!["-Csave-temps"],
    ] {
        let mut raw = vec![OsString::from("rustc")];
        raw.extend(extra.into_iter().map(OsString::from));
        let reason = Invocation::analyze(&raw).unwrap_err();
        assert!(reason.contains("not modeled"), "{reason}");
    }
}

#[test]
fn native_search_paths_that_can_supply_crates_stay_unmodeled() {
    let parse =
        |args: &[&str]| native_inputs(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    for search in ["/native", "all=/native", "crate=/native"] {
        assert!(parse(&["-L", search]).unwrap_err().contains("not modeled"));
        assert!(
            parse(&[&format!("-L{search}")])
                .unwrap_err()
                .contains("not modeled")
        );
    }
    let (dirs, libs) = parse(&[
        "-L",
        "dependency=/deps",
        "-Lnative=/opt/sdk",
        "-L",
        "framework=/f",
        "-l",
        "static=zstd",
        "-lstatic:+verbatim=libq.a",
        "-l",
        "asound",
        "-ldylib=z",
        "-lstatic:-bundle=nb",
    ])
    .unwrap();
    assert_eq!(dirs, [PathBuf::from("/opt/sdk"), PathBuf::from("/f")]);
    assert_eq!(
        libs.iter()
            .map(|l| (l.name.as_str(), l.verbatim))
            .collect::<Vec<_>>(),
        [("zstd", false), ("libq.a", true)]
    );
    assert!(parse(&["-l", "link-arg=-foo"]).is_err());
    assert!(has_unmodeled_codegen_inputs(&[
        "-C".into(),
        "profile-use=/p.profdata".into()
    ]));
    assert!(has_unmodeled_codegen_inputs(&[
        "-Cllvm-plugins=/opt/plugin".into()
    ]));
    assert!(has_unmodeled_codegen_inputs(&[
        "-C".into(),
        "linker-plugin-lto=/opt/p".into()
    ]));
    assert!(!has_unmodeled_codegen_inputs(&["-Clinker=mold".into()]));
    assert!(!has_unmodeled_codegen_inputs(&[
        "-C".into(),
        "link-arg=-fuse-ld=lld".into()
    ]));
}

#[test]
fn only_descriptive_unstable_flags_are_modeled() {
    let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    assert_eq!(
        unmodeled_unstable_flag(&args(&[
            "-Zunstable-options",
            "-Z",
            "force-unstable-if-unmarked",
            "-Zthreads=8"
        ])),
        None
    );
    assert_eq!(
        unmodeled_unstable_flag(&args(&["-Z", "self-profile"])).as_deref(),
        Some("self-profile")
    );
    assert_eq!(
        unmodeled_unstable_flag(&args(&["-Zdump-mir=all"])).as_deref(),
        Some("dump-mir")
    );
    let temp = tempfile::tempdir().unwrap();
    let std = temp.path().join("libstd-abc.rlib");
    fs::write(&std, b"rlib").unwrap();
    let external = format!("noprelude:std={}", std.display());
    let invocation = analyze_with(
        temp.path(),
        &[
            "--crate-type=rlib",
            "--emit=dep-info,metadata,link",
            "-Zunstable-options",
            "--extern",
            &external,
        ],
    )
    .unwrap();
    assert!(invocation.explicit_inputs.contains(&std));
}

#[test]
fn checkouts_come_from_git_roots_not_launch_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // A worktree has a `.git` file; a clone has a `.git` directory.
    let worktree = root.join("worktree");
    fs::create_dir_all(worktree.join("crates/macros/src")).unwrap();
    fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
    let clone = root.join("clone");
    fs::create_dir_all(clone.join(".git")).unwrap();
    fs::create_dir_all(clone.join("crates/x")).unwrap();
    assert_eq!(
        checkout_root(&worktree.join("crates/macros")),
        Some(worktree.clone())
    );
    assert_eq!(checkout_root(&clone.join("crates/x")), Some(clone.clone()));
    assert_eq!(git_root(&root), None);

    // A record whose input lives in another checkout (a poisoned record
    // from before this check) never validates here.
    let foreign = clone.join("crates/x/value.wit");
    fs::write(&foreign, "old").unwrap();
    let mut identity = test_identity(&worktree);
    identity.checkout = Some(worktree.clone());
    let mut candidate = empty_candidate(&identity, PROTOCOL_VERSION);
    candidate.host_files = vec![FileInput {
        path: foreign.to_string_lossy().into_owned(),
        digest: digest_bytes(b"old"),
    }];
    candidate.action_key =
        compiler_action_key(&candidate.static_key, &[], &candidate.host_files, &[]);
    let reason = validate_candidate(&candidate, &identity).unwrap_err();
    assert!(reason.contains("another checkout"), "{reason}");
    // The same input is valid in its own checkout.
    identity.checkout = Some(clone.clone());
    assert!(validate_candidate(&candidate, &identity).is_ok());
    assert!(!foreign_checkout(&foreign, Some(&clone)));
    assert!(outside_checkout_roots("$HOME/Documents/other/x.wit"));
    assert!(!outside_checkout_roots("$CHECKOUT/crates/x.wit"));
}

#[test]
fn doctor_flags_target_outputs_that_name_another_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // A main clone with two linked worktrees.
    let main = root.join("main");
    fs::create_dir_all(main.join(".git/worktrees/a")).unwrap();
    fs::create_dir_all(main.join(".git/worktrees/b")).unwrap();
    fs::create_dir_all(main.join(".git/refs/heads")).unwrap();
    for name in ["a", "b"] {
        let tree = root.join(name);
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::write(
            tree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees").join(name).display()
            ),
        )
        .unwrap();
        fs::write(main.join(".git/worktrees").join(name).join("HEAD"), "ref").unwrap();
        fs::write(tree.join("src/data.wit"), "wit").unwrap();
    }
    let a = root.join("a");
    let target = a.join("target");
    let unit = |name: &str| target.join("debug/build").join(name);
    fs::create_dir_all(unit("own-0123456789abcdef")).unwrap();
    fs::write(
        unit("own-0123456789abcdef").join("output"),
        format!(
            "cargo:rerun-if-changed={}\ncargo:rerun-if-changed={}\n",
            main.join(".git/worktrees/a/HEAD").display(),
            main.join(".git/refs/heads/main").display()
        ),
    )
    .unwrap();
    fs::create_dir_all(unit("stale-host-0123456789abcdef")).unwrap();
    fs::write(
        unit("stale-host-0123456789abcdef").join("output"),
        format!(
            "cargo:rerun-if-changed={}\n",
            main.join(".git/worktrees/b/HEAD").display()
        ),
    )
    .unwrap();
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::write(
        target.join("debug/deps/wasm_abi-0123456789abcdef.d"),
        format!(
            "x.rlib: {}\n# env-dep:OUT_DIR=/elsewhere\n",
            root.join("b/src/data.wit").display()
        ),
    )
    .unwrap();
    fs::write(
        target.join("debug/deps/fine-0123456789abcdef.d"),
        format!("x.rlib: {}\n", a.join("src/data.wit").display()),
    )
    .unwrap();
    assert_eq!(
        foreign_target_outputs(&target, &a),
        BTreeSet::from(["stale-host".to_owned(), "wasm-abi".to_owned()])
    );
}

#[test]
fn restored_dep_info_names_this_checkouts_env_values() {
    let temp = tempfile::tempdir().unwrap();
    let identity = test_identity(temp.path());
    let name = "BELLOWS_TEST_ENV_DEP_LOCALIZED";
    // SAFETY: only this test reads or writes this variable.
    unsafe { env::set_var(name, "C:\\here\\out") };
    let mut candidate = empty_candidate(&identity, PROTOCOL_VERSION);
    candidate.env = vec![EnvInput::capture_normalized(name, "$TARGET/out")];
    let dep_info = format!("a: b\n# env-dep:{name}=/producer/out\n# env-dep:KEEP=1\r\n");
    let localized = String::from_utf8(localize_env_deps(
        dep_info.as_bytes(),
        &candidate,
        &identity,
    ))
    .unwrap();
    assert_eq!(
        localized,
        format!("a: b\n# env-dep:{name}=C:\\\\here\\\\out\n# env-dep:KEEP=1\r\n")
    );
    unsafe { env::remove_var(name) };
}

#[test]
fn diagnostics_name_this_checkouts_real_paths() {
    let roots = vec![
        (
            "/bellows/workspace".to_owned(),
            "/home/dev/checkout a".to_owned(),
        ),
        (
            "/bellows/target".to_owned(),
            "/home/dev/checkout a/target".to_owned(),
        ),
    ];
    assert_eq!(
        devirtualize_line(
            b"error: --> /bellows/workspace/tests/ui/fail.rs:3:5\n",
            &roots
        ),
        b"error: --> /home/dev/checkout a/tests/ui/fail.rs:3:5\n"
    );
    let json = br#"{"rendered":"--> /bellows/workspace/src/lib.rs","spans":[{"file_name":"/bellows/target/out/gen.rs"}]}"#;
    let mut line = json.to_vec();
    line.extend_from_slice(b"\r\n");
    let converted = devirtualize_line(&line, &roots);
    assert!(converted.ends_with(b"\r\n"));
    let value: serde_json::Value =
        serde_json::from_slice(&converted[..converted.len() - 2]).unwrap();
    assert_eq!(value["rendered"], "--> /home/dev/checkout a/src/lib.rs");
    assert_eq!(
        value["spans"][0]["file_name"],
        "/home/dev/checkout a/target/out/gen.rs"
    );
    assert_eq!(devirtualize_line(b"unrelated\n", &roots), b"unrelated\n");
}

#[test]
fn library_compiles_accept_a_linker_they_never_run() {
    let temp = tempfile::tempdir().unwrap();
    let invocation = analyze_with(
        temp.path(),
        &[
            "--crate-type=rlib",
            "--emit=dep-info,metadata,link",
            "-Clinker=/opt/mold/cc",
            "-Lnative=/usr/lib",
        ],
    )
    .unwrap();
    assert_eq!(invocation.kind, OutputKind::Library);
}

#[test]
fn rustc_library_link_arguments_are_byte_inert() {
    let dir = std::env::temp_dir().join(format!("bellows-cli-link-inert-{}", now_ms()));
    let plain = dir.join("plain");
    let linked = dir.join("linked");
    fs::create_dir_all(&plain).unwrap();
    fs::create_dir_all(&linked).unwrap();
    let source = dir.join("lib.rs");
    fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();

    let compile = |out_dir: &Path, with_link_arg: bool| {
        let mut command = Command::new("rustc");
        command
            .arg("--crate-name=demo")
            .arg(&source)
            .arg("--crate-type=rlib")
            .arg("--emit=link,metadata")
            .arg("--out-dir")
            .arg(out_dir);
        if with_link_arg {
            command.arg("-Clink-arg=-fuse-ld=lld");
        }
        assert!(command.status().unwrap().success());
    };
    compile(&plain, false);
    compile(&linked, true);
    for output in ["libdemo.rlib", "libdemo.rmeta"] {
        assert_eq!(
            fs::read(plain.join(output)).unwrap(),
            fs::read(linked.join(output)).unwrap()
        );
    }
    let _ = fs::remove_dir_all(dir);
}

fn analyzed(crate_type: &str, extra: &[&str]) -> Invocation {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("out")).unwrap();
    fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    let mut raw = vec![
        OsString::from("rustc"),
        OsString::from("--crate-name"),
        OsString::from("demo"),
        temp.path().join("main.rs").into_os_string(),
        OsString::from("--crate-type"),
        OsString::from(crate_type),
        OsString::from("--emit=dep-info,link"),
        OsString::from("-C"),
        OsString::from("extra-filename=-abc"),
        OsString::from("--out-dir"),
        temp.path().join("out").into_os_string(),
    ];
    raw.extend(extra.iter().map(OsString::from));
    Invocation::analyze(&raw).unwrap()
}

const WINDOWS_HOST: Option<&str> = Some("x86_64-pc-windows-msvc");
const LINUX_HOST: Option<&str> = Some("x86_64-unknown-linux-gnu");

#[test]
fn path_reaches_a_key_only_where_it_can_choose_the_linker() {
    let path = |crate_type: &str, extra: &[&str], host, vc_environment| {
        path_use_with(&analyzed(crate_type, extra), host, vc_environment)
    };
    assert_eq!(path("lib", &[], WINDOWS_HOST, false), PathUse::None);
    assert_eq!(
        path("lib", &["-Clinker-flavor=lld-link"], LINUX_HOST, false),
        PathUse::None
    );
    let msvc = ["--target", "x86_64-pc-windows-msvc"];
    assert_eq!(path("bin", &msvc, WINDOWS_HOST, false), PathUse::None);
    assert_eq!(
        path(
            "bin",
            &[msvc[0], msvc[1], "-C", "linker-flavor=lld-link"],
            WINDOWS_HOST,
            false
        ),
        PathUse::Linker("lld-link")
    );
    assert_eq!(
        path(
            "cdylib",
            &["--target", "wasm32-unknown-unknown"],
            LINUX_HOST,
            false
        ),
        PathUse::None
    );
    assert_eq!(
        path(
            "bin",
            &["--target", "x86_64-unknown-linux-gnu"],
            LINUX_HOST,
            false
        ),
        PathUse::Whole
    );
    assert_eq!(
        path(
            "bin",
            &[
                "--target",
                "x86_64-unknown-linux-gnu",
                "-Clinker-flavor=gcc"
            ],
            LINUX_HOST,
            false
        ),
        PathUse::Whole
    );
}

#[test]
fn emscripten_links_with_the_emcc_on_path() {
    // The other wasm targets link with the sysroot's rust-lld.
    for target in ["wasm32-unknown-emscripten", "asmjs-unknown-emscripten"] {
        assert_eq!(
            path_use_with(&analyzed("bin", &["--target", target]), WINDOWS_HOST, false),
            PathUse::Whole,
            "{target}"
        );
    }
    for target in ["wasm32-unknown-unknown", "wasm32-wasip1", "wasm32v1-none"] {
        assert_eq!(
            path_use_with(&analyzed("bin", &["--target", target]), LINUX_HOST, false),
            PathUse::None,
            "{target}"
        );
    }
}

#[test]
fn a_host_build_follows_the_hosts_linker_not_the_bellows_binary() {
    // No `--target`: rustc links for its own host, whatever Bellows was
    // built for. A gcc-linked windows-gnu host keeps PATH in the key.
    let host_build = analyzed("bin", &[]);
    assert_eq!(
        path_use_with(&host_build, Some("x86_64-pc-windows-gnu"), false),
        PathUse::Whole
    );
    assert_eq!(
        path_use_with(&host_build, LINUX_HOST, false),
        PathUse::Whole
    );
    assert_eq!(
        path_use_with(&host_build, WINDOWS_HOST, false),
        PathUse::None
    );
    // An unknown host assumes nothing.
    assert_eq!(path_use_with(&host_build, None, false), PathUse::Whole);
}

#[test]
fn msvc_links_with_the_link_exe_on_path_when_visual_studio_is_not_asked() {
    let msvc = analyzed("bin", &["--target", "x86_64-pc-windows-msvc"]);
    // A loaded Visual Studio environment puts its link.exe first on PATH.
    assert_eq!(
        path_use_with(&msvc, WINDOWS_HOST, true),
        PathUse::Linker("link.exe")
    );
    // A cross build from another system has no Visual Studio to ask.
    assert_eq!(
        path_use_with(&msvc, LINUX_HOST, false),
        PathUse::Linker("link.exe")
    );
    assert_eq!(path_use_with(&msvc, WINDOWS_HOST, false), PathUse::None);
}

#[test]
fn the_host_is_read_from_the_compiler_version() {
    let version = b"rustc 1.92.0 (abc 2026-01-01)
binary: rustc
host: x86_64-pc-windows-gnu
release: 1.92.0
";
    assert_eq!(
        host_triple(version).as_deref(),
        Some("x86_64-pc-windows-gnu")
    );
    assert_eq!(
        host_triple(
            b"rustc 1.92.0
"
        ),
        None
    );
}

#[test]
fn compile_keys_leave_out_the_shell_but_build_scripts_keep_it() {
    // Build-script runs use the full relevant environment.
    for name in ["PATH", "HOME", "LANG", "LC_ALL", "TZ"] {
        assert!(is_relevant_environment_name(name), "{name}");
    }
    assert!(!is_relevant_environment_name("RUSTUP_AUTO_INSTALL"));
    let normalizer = PathNormalizer::new(Vec::new());
    // The test process has PATH; the others may be unset, so only check
    // that none of them survives.
    for path_use in [PathUse::None, PathUse::Linker("lld-link")] {
        let environment = compile_environment(&normalizer, &path_use);
        for name in ["PATH", "HOME", "LANG", "LC_ALL", "TZ"] {
            assert!(!environment.contains_key(name), "{name} in {path_use:?}");
        }
    }
    assert!(compile_environment(&normalizer, &PathUse::Whole).contains_key("PATH"));
}

#[test]
fn windows_keys_each_variable_in_one_spelling() {
    let normalizer = PathNormalizer::new(Vec::new());
    let vars = || {
        [
            ("Path", r"C:\bin"),
            ("Home", r"C:\Users\me"),
            ("ucrtversion", "10.0"),
            ("Cargo_Pkg_Name", "demo"),
            ("RUSTFLAGS", "-Dwarnings"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()))
    };
    let windows = relevant_environment_of(vars(), &normalizer, true);
    assert_eq!(
        windows.keys().map(String::as_str).collect::<Vec<_>>(),
        ["CARGO_PKG_NAME", "HOME", "PATH", "RUSTFLAGS", "UCRTVersion"]
    );
    // On other systems `Path` is a different variable from `PATH`.
    let posix = relevant_environment_of(vars(), &normalizer, false);
    assert_eq!(
        posix.keys().map(String::as_str).collect::<Vec<_>>(),
        ["RUSTFLAGS"]
    );
    // So the shell's variables leave a compile key however they are spelled.
    let compile = without_shell(windows.clone(), &PathUse::None);
    assert_eq!(
        compile.keys().map(String::as_str).collect::<Vec<_>>(),
        ["CARGO_PKG_NAME", "RUSTFLAGS", "UCRTVersion"]
    );
    assert!(without_shell(windows, &PathUse::Whole).contains_key("PATH"));
}

#[test]
fn excludes_per_run_ci_environment_from_static_keys() {
    assert!(!is_relevant_environment_name("GITHUB_RUN_ID"));
    assert!(!is_relevant_environment_name("GITHUB_ENV"));
    assert!(!is_relevant_environment_name("BELLOWS_AUTH_TOKEN"));
    assert!(is_relevant_environment_name("RUSTFLAGS"));
    assert!(is_relevant_environment_name("CARGO_MANIFEST_DIR"));
    assert!(is_relevant_environment_name("CC_x86_64_unknown_linux_gnu"));
    assert!(is_relevant_environment_name("CLIPPY_ARGS"));
    assert!(is_relevant_environment_name("CLIPPY_CONF_DIR"));
}

#[test]
fn a_build_schedulers_label_never_reaches_a_key() {
    // cargo-hold marks everything inside a hold with the holder's label:
    // by default the name of the folder Cargo was started in.
    for name in ["CARGO_HOLD_ACTIVE", "CARGO_HOLD_LABEL", "CARGO_HOLD_SKIP"] {
        assert!(!is_relevant_environment_name(name), "{name}");
    }
    let normalizer = PathNormalizer::new(Vec::new());
    let vars = |label: &str| {
        [
            ("CARGO_HOLD_ACTIVE", label),
            ("Cargo_Hold_Label", label),
            ("CARGO_PKG_NAME", "demo"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()))
    };
    // Neither a compile's key nor a build-script run's, which keeps the
    // whole relevant environment, differs between two labels.
    for case_insensitive in [false, true] {
        let one = relevant_environment_of(vars("worktree-one"), &normalizer, case_insensitive);
        let two = relevant_environment_of(vars("worktree-two"), &normalizer, case_insensitive);
        assert_eq!(one, two);
        assert_eq!(
            one.keys().map(String::as_str).collect::<Vec<_>>(),
            ["CARGO_PKG_NAME"]
        );
    }
}
