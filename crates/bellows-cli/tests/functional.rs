use bellows_core::PROTOCOL_VERSION;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BELLOWS: &str = env!("CARGO_BIN_EXE_bellows");

struct Fixture {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    cache: PathBuf,
}
impl Fixture {
    fn new(lib: &str, main: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace with spaces");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(workspace.join("src/lib.rs"), lib).unwrap();
        fs::write(workspace.join("src/main.rs"), main).unwrap();
        let cache = temp.path().join("cache");
        Self {
            temp,
            workspace,
            cache,
        }
    }
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.workspace);
        for (name, _) in std::env::vars_os() {
            let name_str = name.to_string_lossy();
            if name_str.starts_with("BELLOWS_")
                || name_str.starts_with("CARGO_")
                || matches!(
                    name_str.as_ref(),
                    "RUSTC_WRAPPER"
                        | "RUSTC_WORKSPACE_WRAPPER"
                        | "RUSTFLAGS"
                        | "RUSTDOCFLAGS"
                        | "RUSTC"
                )
            {
                // Keep CARGO_HOME for the installed toolchain's normal lookup.
                if name_str != "CARGO_HOME" {
                    cmd.env_remove(name);
                }
            }
        }
        cmd.env("BELLOWS_COLOR", "never");
        cmd
    }
    fn local(&self) -> Command {
        settle();
        let mut cmd = self.command(BELLOWS);
        cmd.args(["local", "--cache-dir"])
            .arg(&self.cache)
            .arg("--");
        cmd
    }
    fn build(&self) -> Output {
        checked(
            self.local()
                .args(["cargo", "build", "--release", "--offline"]),
        )
    }
    fn clean(&self) {
        fs::remove_dir_all(self.workspace.join("target")).unwrap();
    }
    fn value(&self, root: &str) -> String {
        let output = checked(
            &mut self.command(
                self.workspace
                    .join(root)
                    .join("release")
                    .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
            ),
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn explain(&self, flags: &[&str]) -> Value {
        let result = checked(
            self.command(BELLOWS)
                .args(["explain", "--local", "--cache-dir"])
                .arg(&self.cache)
                .args(flags)
                .arg("--json"),
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
    fn declared(&self) -> Command {
        let mut cmd = self.command(BELLOWS);
        cmd.args(["action", "run", "--local", "--cache-dir"])
            .arg(&self.cache)
            .args([
                "--name",
                "fixture",
                "--input",
                "Cargo.toml",
                "--input",
                "Cargo.lock",
                "--input",
                "src",
                "--output",
                "output",
            ]);
        cmd
    }
    fn lock(&self) {
        checked(
            self.command("cargo")
                .args(["generate-lockfile", "--offline"]),
        );
    }
}
fn checked(command: &mut Command) -> Output {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn explicit_target_directories_share_verified_library_outputs() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let first = f.temp.path().join("first target");
    let second = f.temp.path().join("second target");
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("CARGO_TARGET_DIR", &first),
    );
    let restored = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("CARGO_TARGET_DIR", &second),
    );
    assert!(
        stderr(&restored).contains("LOCAL HIT"),
        "{}",
        stderr(&restored)
    );
    let output = checked(
        &mut f.command(
            second
                .join("release")
                .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
        ),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn cargo_from_a_subdirectory_resolves_compiler_inputs_in_cargos_working_directory() {
    let f = Fixture::new("pub fn value() -> u32 { 42 }", "fn main() {}");
    let subdir = f.workspace.join("launcher");
    fs::create_dir(&subdir).unwrap();
    for warm in [false, true] {
        let output = checked(f.local().current_dir(&subdir).args([
            "cargo",
            "build",
            "--release",
            "--offline",
            "--manifest-path",
            "../Cargo.toml",
        ]));
        assert!(!stderr(&output).contains("FALLBACK"), "{}", stderr(&output));
        if warm {
            assert!(stderr(&output).contains("LOCAL HIT"), "{}", stderr(&output));
        }
        f.clean();
    }
}

#[test]
fn restored_cargo_json_artifact_messages_remain_valid() {
    let f = Fixture::new("pub fn value() -> u32 { 42 }", "fn main() {}");
    let target = f.temp.path().join("json target");
    for warm in [false, true] {
        let output = checked(
            f.local()
                .env(
                    "CARGO_TARGET_DIR",
                    target.to_string_lossy().replace('\\', "/"),
                )
                .args([
                    "cargo",
                    "build",
                    "--release",
                    "--offline",
                    "--message-format=json",
                ]),
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let messages: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            messages
                .iter()
                .any(|v| v["reason"] == "build-finished" && v["success"] == true)
        );
        for message in messages
            .iter()
            .filter(|v| v["reason"] == "compiler-artifact")
        {
            for path in message["filenames"].as_array().unwrap() {
                assert!(std::path::Path::new(path.as_str().unwrap()).is_file());
            }
        }
        if warm {
            assert!(String::from_utf8_lossy(&output.stderr).contains("LOCAL HIT"));
        }
        fs::remove_dir_all(&target).unwrap();
    }
}

#[test]
fn restored_dep_info_preserves_forward_slash_environment_paths() {
    let f = Fixture::new(
        "pub fn value() -> &'static str { env!(\"FORWARD_BUILD_PATH\") }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let value = f.workspace.to_string_lossy().replace('\\', "/");
    for warm in [false, true] {
        let output = checked(f.local().env("FORWARD_BUILD_PATH", &value).args([
            "cargo",
            "build",
            "--release",
            "--offline",
        ]));
        if warm {
            assert!(stderr(&output).contains("LOCAL HIT"), "{}", stderr(&output));
        }
        assert_eq!(f.value("target"), value);
        f.clean();
    }
}

#[test]
fn long_unicode_source_paths_restore_and_track_edits() {
    let f = Fixture::new("", "fn main() { println!(\"{}\", fixture::value()); }");
    let mut parent = f.workspace.join("src/長い workspace café");
    while parent.as_os_str().len() < 280 {
        parent = parent.join("a-long-source-directory-component");
    }
    fs::create_dir_all(&parent).unwrap();
    let source = parent.join("café value.rs");
    let relative = source
        .strip_prefix(f.workspace.join("src"))
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    fs::write(
        f.workspace.join("src/lib.rs"),
        format!("#[path={relative:?}] mod value; pub fn value() -> u32 {{ value::value() }}"),
    )
    .unwrap();
    fs::write(&source, "pub fn value() -> u32 { 42 }").unwrap();
    // Keep linker output short: this verifies long source paths without
    // silently depending on every native tool supporting long output paths.
    let target = f.temp.path().join("target");
    let build = || {
        checked(f.local().env("CARGO_TARGET_DIR", &target).args([
            "cargo",
            "build",
            "--release",
            "--offline",
        ]))
    };
    build();
    fs::remove_dir_all(&target).unwrap();
    let restored = build();
    assert!(
        stderr(&restored).contains("LOCAL HIT"),
        "{}",
        stderr(&restored)
    );
    fs::write(&source, "pub fn value() -> u32 { 43 }").unwrap();
    build();
    let output = checked(
        &mut f.command(
            target
                .join("release")
                .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
        ),
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "43");
}

#[test]
fn relocated_dep_info_tracks_transitive_inputs_with_spaces() {
    let lib = "#[path=\"value part.rs\"] mod value; pub fn value() -> u32 { value::value() }";
    let main = "fn main() { println!(\"{}\", fixture::value()); }";
    let first = Fixture::new(lib, main);
    let mut second = Fixture::new(lib, main);
    second.cache = first.cache.clone();
    for fixture in [&first, &second] {
        fs::write(
            fixture.workspace.join("src/value part.rs"),
            "pub fn value() -> u32 { 42 }",
        )
        .unwrap();
    }
    first.build();
    let restored = second.build();
    assert!(
        stderr(&restored).contains("LOCAL HIT"),
        "{}",
        stderr(&restored)
    );
    assert_eq!(second.value("target"), "42");
    // Keep Cargo output intact: Cargo must now track the receiving workspace's
    // transitive path, not the original producer's path in the restored .d file.
    fs::write(
        second.workspace.join("src/value part.rs"),
        "pub fn value() -> u32 { 43 }",
    )
    .unwrap();
    second.build();
    assert_eq!(second.value("target"), "43");
}

#[cfg(any(unix, windows))]
fn link_directory(f: &Fixture, source: &std::path::Path, destination: &std::path::Path) {
    #[cfg(unix)]
    {
        let _ = f;
        std::os::unix::fs::symlink(source, destination).unwrap();
    }
    #[cfg(windows)]
    {
        // NTFS junctions need neither administrator rights nor Developer Mode.
        checked(
            f.command("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(destination)
                .arg(source),
        );
    }
}

#[cfg(windows)]
#[test]
fn junction_retargeting_never_restores_old_code() {
    let f = Fixture::new(
        "#[path=\"../selected/value.rs\"] mod selected; pub fn value() -> u32 { selected::value() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let link = f.workspace.join("selected");
    for value in [1, 2] {
        let source = f.temp.path().join(format!("source-{value}"));
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("value.rs"),
            format!("pub fn value() -> u32 {{ {value} }}"),
        )
        .unwrap();
        link_directory(&f, &source, &link);
        assert!(stderr(&f.build()).contains("symlinked compiler input"));
        assert_eq!(f.value("target"), value.to_string());
        fs::remove_dir(&link).unwrap();
        f.clean();
    }
}

#[test]
fn embedded_environment_paths_are_not_reused_between_checkouts() {
    let mut f = Fixture::new(
        "pub fn value() -> &'static str { env!(\"CARGO_MANIFEST_DIR\") }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    f.build();
    assert_eq!(f.value("target"), f.workspace.to_str().unwrap());
    let other = f.temp.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    for path in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs"] {
        fs::copy(f.workspace.join(path), other.join(path)).unwrap();
    }
    f.workspace = other;
    let result = f.build();
    assert_eq!(f.value("target"), f.workspace.to_str().unwrap());
    assert!(stderr(&result).contains("environment changed: CARGO_MANIFEST_DIR"));
    f.clean();
    let warm = stderr(&f.build());
    assert!(warm.contains("LOCAL HIT"), "{warm}");
}

#[cfg(unix)]
#[test]
fn symlink_retargeting_never_restores_old_code() {
    let f = Fixture::new(
        "mod selected; pub fn value() -> u32 { selected::value() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    fs::write(
        f.workspace.join("src/one.rs"),
        "pub fn value() -> u32 { 1 }",
    )
    .unwrap();
    fs::write(
        f.workspace.join("src/two.rs"),
        "pub fn value() -> u32 { 2 }",
    )
    .unwrap();
    let link = f.workspace.join("src/selected.rs");
    std::os::unix::fs::symlink("one.rs", &link).unwrap();
    assert!(stderr(&f.build()).contains("symlinked compiler input"));
    assert_eq!(f.value("target"), "1");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("two.rs", &link).unwrap();
    f.clean();
    f.build();
    assert_eq!(f.value("target"), "2");
}

#[test]
fn unsupported_emit_products_are_produced_on_every_invocation() {
    let f = Fixture::new("pub fn value() -> u32 { 1 }", "fn main() {}");
    for _ in 0..2 {
        fs::create_dir_all(f.workspace.join("out")).unwrap();
        let result = checked(f.local().arg(BELLOWS).args([
            "rustc",
            "--crate-name",
            "fixture",
            "--crate-type",
            "rlib",
            "--emit=dep-info,metadata,llvm-ir",
            "-C",
            "extra-filename=-audit",
            "--out-dir",
            "out",
            "src/lib.rs",
        ]));
        assert!(stderr(&result).contains("unsupported emit set"));
        assert!(f.workspace.join("out/fixture-audit.ll").is_file());
        fs::remove_dir_all(f.workspace.join("out")).unwrap();
    }
}

#[test]
fn declared_actions_preserve_environment_and_configuration_rustflags() {
    let f = Fixture::new(
        "pub fn value() -> bool { cfg!(audit_flag) }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    f.lock();
    for (name, value) in [
        ("RUSTFLAGS", "--cfg audit_flag"),
        ("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}audit_flag"),
    ] {
        checked(
            f.declared()
                .args([
                    "--env",
                    name,
                    "--",
                    "cargo",
                    "build",
                    "--release",
                    "--locked",
                    "--offline",
                    "--target-dir",
                    "output",
                ])
                .env(name, value),
        );
        assert_eq!(f.value("output"), "true");
    }
    fs::create_dir(f.workspace.join(".cargo")).unwrap();
    fs::write(
        f.workspace.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"audit_flag\"]\n",
    )
    .unwrap();
    checked(f.declared().args([
        "--input",
        ".cargo",
        "--",
        "cargo",
        "build",
        "--release",
        "--locked",
        "--offline",
        "--target-dir",
        "output",
    ]));
    assert_eq!(f.value("output"), "true");
}

#[test]
fn declared_roots_replace_stale_files_and_allow_nested_declarations() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    f.lock();
    let build = || {
        checked(f.declared().args([
            "--output",
            "output/release",
            "--",
            "cargo",
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target-dir",
            "output",
        ]))
    };
    build();
    fs::write(f.workspace.join("output/stale"), "obsolete artifact").unwrap();
    fs::write(f.workspace.join("unrelated"), "keep me").unwrap();
    assert!(String::from_utf8_lossy(&build().stdout).contains("CACHE HIT"));
    assert!(!f.workspace.join("output/stale").exists());
    assert!(f.workspace.join("unrelated").exists());
    assert!(
        f.workspace
            .join("output/release")
            .join(format!("fixture{}", std::env::consts::EXE_SUFFIX))
            .is_file()
    );
    let rejected = f
        .declared()
        .args([
            "--output",
            "src",
            "--",
            "cargo",
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target-dir",
            "output",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(stderr(&rejected).contains("input/output overlap"));
    assert!(f.workspace.join("src/lib.rs").exists());
}

#[test]
fn broken_local_cache_does_not_prevent_cargo_from_running() {
    let f = Fixture::new("", "fn main() {}");
    fs::write(&f.cache, "not a directory").unwrap();
    let result = checked(f.local().args(["cargo", "--version"]));
    assert!(String::from_utf8_lossy(&result.stdout).starts_with("cargo "));
    assert!(stderr(&result).contains("cache initialization failed"));
    // The wrapped command's own failure status is still returned.
    let result = f
        .local()
        .args(["cargo", "not-a-real-subcommand"])
        .output()
        .unwrap();
    assert!(!result.status.success());
}

#[test]
fn diagnostics_distinguish_source_environment_flags_corruption_and_sessions() {
    let f = Fixture::new(
        "mod value; pub fn value() -> u32 { value::value() }",
        "fn main() {}",
    );
    fs::write(
        f.workspace.join("src/value.rs"),
        "pub fn value() -> u32 { 1 }",
    )
    .unwrap();
    f.build();
    // The library and the final binary are both cacheable.
    let first = f.explain(&["--latest", "--summary"]);
    assert_eq!(first["decisions"]["miss"], 2);
    let first_id = first["session_ids"][0].as_str().unwrap().to_owned();
    f.clean();
    let warm = stderr(&f.build());
    assert!(warm.contains("LOCAL HIT"), "{warm}");
    let summary = f.explain(&["--latest", "--summary"]);
    assert_eq!(summary["decisions"]["l1_hit"], 2);
    assert!(summary["decisions"].get("miss").is_none());
    assert_eq!(
        f.explain(&["--session", &first_id, "--summary"])["decisions"]["miss"],
        2
    );
    fs::write(
        f.workspace.join("src/value.rs"),
        "pub fn value() -> u32 { 2 }",
    )
    .unwrap();
    f.clean();
    let edited = stderr(&f.build());
    assert!(
        edited.contains(&format!(
            "input changed: $WORKSPACE{0}src{0}value.rs",
            std::path::MAIN_SEPARATOR
        )),
        "{edited}"
    );
    let events = f.explain(&["--latest", "--crate", "fixture"]);
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["reason"] == "input_changed")
    );
    // A primary source edit changes the static key; explain the component delta.
    fs::write(
        f.workspace.join("src/lib.rs"),
        "mod value; pub fn value() -> u32 { value::value() + 1 }",
    )
    .unwrap();
    f.clean();
    let edited = stderr(&f.build());
    assert!(
        edited.contains(&format!(
            "explicit input changed: $WORKSPACE{0}src{0}lib.rs",
            std::path::MAIN_SEPARATOR
        )),
        "{edited}"
    );
    f.clean();
    let flags = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("RUSTFLAGS", "--cfg private_value_123"),
    );
    assert!(stderr(&flags).contains("environment changed: RUSTFLAGS"));
    let latest = f.explain(&["--latest", "--summary"]);
    assert!(!latest.to_string().contains("private_value_123"));
    // Restore the unflagged identity, then damage its artifact.
    f.clean();
    f.build();
    let store =
        bellows_core::Store::open(f.cache.join(format!("store-v{PROTOCOL_VERSION}"))).unwrap();
    let events: Vec<bellows_core::Event> = fs::read_to_string(f.cache.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let key = events
        .iter()
        .rev()
        .find(|e| e.kind == "l1_hit" && e.detail.contains("library output"))
        .unwrap()
        .static_key
        .as_ref()
        .unwrap();
    let index = store.read_candidates(key).unwrap();
    let artifact = index.candidates[0]
        .artifacts
        .iter()
        .find(|a| a.file_name.ends_with(".rlib"))
        .unwrap();
    fs::write(store.blob_path(&artifact.digest).unwrap(), "broken").unwrap();
    f.clean();
    let rebuilt = f.build();
    assert!(stderr(&rebuilt).contains("corrupt blob"));
    assert!(
        f.explain(&["--latest", "--summary"])["decisions"]["corrupt"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn explain_finds_default_local_log_and_honors_event_log_override() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    checked(
        f.command(BELLOWS)
            .args(["cargo", "build", "--release", "--offline"])
            .env("XDG_CACHE_HOME", f.temp.path().join("xdg")),
    );
    let result = checked(
        f.command(BELLOWS)
            .args(["explain", "--latest", "--json"])
            .env("XDG_CACHE_HOME", f.temp.path().join("xdg")),
    );
    assert!(
        !serde_json::from_slice::<Vec<Value>>(&result.stdout)
            .unwrap()
            .is_empty()
    );
    let path = f.temp.path().join("custom.jsonl");
    checked(
        f.local()
            .args(["cargo", "check", "--release", "--offline"])
            .env("BELLOWS_EVENT_LOG", &path),
    );
    let result = checked(
        f.command(BELLOWS)
            .args(["stats", "--local", "--cache-dir"])
            .arg(&f.cache)
            .args(["--latest", "--json"])
            .env("BELLOWS_EVENT_LOG", &path),
    );
    assert!(
        !serde_json::from_slice::<Value>(&result.stdout).unwrap()["events"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(unix, windows))]
#[test]
fn restore_refuses_symlink_destinations_without_touching_other_outputs() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    f.lock();
    let build = || {
        f.declared()
            .args([
                "--",
                "cargo",
                "build",
                "--release",
                "--locked",
                "--offline",
                "--target-dir",
                "output",
            ])
            .output()
            .unwrap()
    };
    assert!(build().status.success());
    fs::remove_dir_all(f.workspace.join("output")).unwrap();
    let elsewhere = f.temp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep"), "untouched").unwrap();
    link_directory(&f, &elsewhere, &f.workspace.join("output"));
    let rejected = build();
    assert!(!rejected.status.success());
    assert!(!stderr(&rejected).contains("stale cached result will be rebuilt"));
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep")).unwrap(),
        "untouched"
    );
    assert!(!elsewhere.join("release").exists());
}

#[test]
fn cfg_literals_containing_checkout_paths_are_hashed_exactly() {
    let mut f = Fixture::new("", "fn main() { println!(\"{}\", fixture::value()); }");
    let first = f.workspace.clone();
    fs::write(
        first.join("src/lib.rs"),
        format!(
            "pub fn value() -> bool {{ cfg!(audit_path={:?}) }}",
            first.to_string_lossy()
        ),
    )
    .unwrap();
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                format!("--cfg\u{1f}audit_path={:?}", first.to_string_lossy()),
            ),
    );
    assert_eq!(f.value("target"), "true");
    let other = f.temp.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    for path in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs"] {
        fs::copy(first.join(path), other.join(path)).unwrap();
    }
    f.workspace = other;
    let result = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                format!("--cfg\u{1f}audit_path={:?}", f.workspace.to_string_lossy()),
            ),
    );
    assert!(!stderr(&result).contains("LOCAL HIT"));
    assert_eq!(f.value("target"), "false");
    assert!(stderr(&result).contains("argument --cfg"));
}

#[test]
fn native_archives_on_unqualified_and_all_search_paths_never_hit() {
    let f = Fixture::new(
        "#[link(name=\"audit_native\", kind=\"static\")] unsafe extern \"C\" { fn native_value() -> i32; } pub fn value() -> i32 { unsafe { native_value() } }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    fs::create_dir(f.workspace.join("native")).unwrap();
    for search in ["native", "all=native"] {
        for value in [1, 2] {
            fs::write(
                f.workspace.join("native/value.c"),
                format!("int native_value(void) {{ return {value}; }}"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                checked(
                    f.command("cc")
                        .args(["-c", "native/value.c", "-o", "native/value.o"]),
                );
                checked(f.command("ar").args([
                    "rcs",
                    "native/libaudit_native.a",
                    "native/value.o",
                ]));
            }
            #[cfg(windows)]
            {
                let finder = PathBuf::from(std::env::var_os("ProgramFiles(x86)").unwrap())
                    .join("Microsoft Visual Studio/Installer/vswhere.exe");
                let found = checked(f.command(finder).args([
                    "-latest",
                    "-products",
                    "*",
                    "-requires",
                    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                    "-property",
                    "installationPath",
                ]));
                let installation = PathBuf::from(String::from_utf8(found.stdout).unwrap().trim());
                let version = fs::read_to_string(
                    installation.join("VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"),
                )
                .unwrap();
                let tools = installation
                    .join("VC/Tools/MSVC")
                    .join(version.trim())
                    .join("bin/Hostx64/x64");
                checked(f.command(tools.join("cl.exe")).args([
                    "/nologo",
                    "/c",
                    "native/value.c",
                    "/Fonative/value.obj",
                ]));
                checked(f.command(tools.join("lib.exe")).args([
                    "/nologo",
                    "/OUT:native/audit_native.lib",
                    "native/value.obj",
                ]));
            }
            fs::create_dir_all(f.workspace.join("out")).unwrap();
            let result = checked(f.local().arg(BELLOWS).args([
                "rustc",
                "src/lib.rs",
                "--edition=2024",
                "--crate-name=fixture",
                "--crate-type=rlib",
                "--emit=dep-info,link",
                "-Cextra-filename=-audit",
                "--out-dir=out",
                "-L",
                search,
            ]));
            assert!(stderr(&result).contains("native linker or external codegen inputs"));
            assert!(!stderr(&result).contains("LOCAL HIT"));
            checked(f.command("rustc").args([
                "src/main.rs",
                "--edition=2024",
                "--extern",
                "fixture=out/libfixture-audit.rlib",
                "-o",
                if cfg!(windows) { "app.exe" } else { "app" },
            ]));
            let result = checked(
                &mut f.command(
                    f.workspace
                        .join(format!("app{}", std::env::consts::EXE_SUFFIX)),
                ),
            );
            assert_eq!(
                String::from_utf8(result.stdout).unwrap().trim(),
                value.to_string()
            );
            fs::remove_dir_all(f.workspace.join("out")).unwrap();
        }
    }
}

#[test]
fn waiting_build_reacquires_released_lease_without_timing_out() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let lease_requests = Arc::new(AtomicUsize::new(0));
    let stop = stopped.clone();
    let requests = lease_requests.clone();
    let server = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<u64>().unwrap();
                }
            }
            std::io::copy(&mut reader.take(length), &mut std::io::sink()).unwrap();
            let (status, body) = if request.starts_with("POST /v1/leases/") {
                let attempt = requests.fetch_add(1, Ordering::SeqCst);
                let response = if attempt == 0 {
                    serde_json::json!({"status":"wait","retry_after_ms":50,"expires_ms":bellows_core::now_ms()+60_000})
                } else {
                    serde_json::json!({"status":"owned","token":"test-lease","expires_ms":bellows_core::now_ms()+60_000})
                };
                ("200 OK", response.to_string())
            } else if request.starts_with("GET /v1/actions/") {
                ("200 OK", "{\"candidates\":[]}".to_owned())
            } else if request.starts_with("HEAD ") {
                ("404 Not Found", String::new())
            } else {
                ("200 OK", "{}".to_owned())
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let events = f.temp.path().join("events.jsonl");
    let output = f
        .command(BELLOWS)
        .env("BELLOWS_L1", "0")
        .env("BELLOWS_STATE_DIR", f.temp.path().join("state"))
        .env("BELLOWS_EVENT_LOG", &events)
        .env("BELLOWS_MAX_WAIT_MS", "1000")
        .args([
            "run",
            "--server",
            &server_url,
            "--",
            "cargo",
            "build",
            "--release",
            "--offline",
        ])
        .output()
        .unwrap();
    stopped.store(true, Ordering::SeqCst);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    // Library: wait, then acquire the released lease. Binary: acquire.
    assert_eq!(lease_requests.load(Ordering::SeqCst), 3);
    let recorded = fs::read_to_string(events).unwrap();
    assert!(
        recorded.contains("\"kind\":\"lease_acquired\""),
        "{recorded}"
    );
    assert!(!recorded.contains("\"kind\":\"fallback\""), "{recorded}");
    assert_eq!(f.value("target"), "42");
}

fn write_files(root: &std::path::Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

/// Copy the fixture's sources (not its target) into a sibling checkout.
/// Let inputs written just now age past the slack within which Bellows treats
/// a file as possibly changed during the compile (`changed_during_compile`).
/// Tests write a source and build within milliseconds, which no edit does;
/// every `Fixture::local` invocation waits this long first.
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(150));
}

fn second_checkout(f: &mut Fixture, name: &str) {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "target" || name == ".bellows" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &to.join(&name));
            } else {
                fs::copy(entry.path(), to.join(&name)).unwrap();
            }
        }
    }
    let other = f.temp.path().join(name);
    copy(&f.workspace, &other);
    f.workspace = other;
}

/// Cache decisions for one crate from wrapper status lines, e.g. "LOCAL HIT".
fn decisions(output: &Output, crate_name: &str) -> Vec<String> {
    stderr(output)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (label, rest) = line.split_once(&format!(" {crate_name} "))?;
            let label = label.trim();
            (!rest.is_empty() && label.chars().all(|c| c.is_ascii_uppercase() || c == ' '))
                .then(|| label.to_owned())
        })
        .collect()
}

fn events(f: &Fixture) -> Vec<bellows_core::Event> {
    fs::read_to_string(f.cache.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn test_harnesses_share_across_checkouts_unless_they_embed_a_checkout_path() {
    let mut f = Fixture::new(
        "pub fn value() -> u32 { 42 }\n#[cfg(test)] mod tests { #[test] fn unit() { assert_eq!(super::value(), 42); } }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            (
                "tests/pure.rs",
                "#[test] fn pure() { assert_eq!(fixture::value(), 42); assert!(file!().ends_with(\"pure.rs\")); }",
            ),
            (
                "tests/baked.rs",
                "#[test] fn baked() { let manifest = concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/Cargo.toml\"); assert!(std::fs::read_to_string(manifest).unwrap().contains(\"fixture\")); println!(\"BAKED={}\", env!(\"CARGO_MANIFEST_DIR\")); }",
            ),
        ],
    );
    f.lock();
    let test = |f: &Fixture| {
        checked(
            f.local()
                .args(["cargo", "test", "--offline", "--", "--nocapture"]),
        )
    };
    let first = test(&f);
    assert_eq!(
        decisions(&first, "pure"),
        ["CACHE MISS"],
        "{}",
        stderr(&first)
    );
    second_checkout(&mut f, "second checkout");
    let second = test(&f);
    let log = stderr(&second);
    assert_eq!(decisions(&second, "pure"), ["LOCAL HIT"], "{log}");
    assert_eq!(decisions(&second, "baked"), ["CACHE MISS"], "{log}");
    assert!(
        log.contains("environment changed: CARGO_MANIFEST_DIR"),
        "{log}"
    );
    // The recompiled path-baking test reads this checkout's files.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains(&format!("BAKED={}", f.workspace.display())),
        "{stdout}"
    );
    // Identical results: every harness passes in both checkouts.
    let results = |output: &Output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("test ") && line.contains(" ... "))
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(results(&first), results(&second));
    assert_eq!(results(&second).len(), 3);
    assert!(results(&second).iter().all(|line| line.ends_with("... ok")));

    // A clean target in the same checkout restores even the pinned harness.
    f.clean();
    let warm = test(&f);
    assert_eq!(
        decisions(&warm, "baked"),
        ["LOCAL HIT"],
        "{}",
        stderr(&warm)
    );
    if cfg!(windows) {
        let deps = f.workspace.join("target/debug/deps");
        let names = fs::read_dir(&deps)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for suffix in [".exe", ".pdb"] {
            assert!(
                names
                    .iter()
                    .any(|name| name.starts_with("pure-") && name.ends_with(suffix)),
                "restored {suffix} missing: {names:?}"
            );
        }
    }
}

fn native_archive(f: &Fixture, value: i32) {
    fs::create_dir_all(f.workspace.join("native")).unwrap();
    fs::write(
        f.workspace.join("native/value.c"),
        format!("int native_value(void) {{ return {value}; }}"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        checked(
            f.command("cc")
                .args(["-c", "native/value.c", "-o", "native/value.o"]),
        );
        let _ = fs::remove_file(f.workspace.join("native/libaudit_native.a"));
        checked(
            f.command("ar")
                .args(["rcs", "native/libaudit_native.a", "native/value.o"]),
        );
    }
    #[cfg(windows)]
    {
        let finder = PathBuf::from(std::env::var_os("ProgramFiles(x86)").unwrap())
            .join("Microsoft Visual Studio/Installer/vswhere.exe");
        let found = checked(f.command(finder).args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ]));
        let installation = PathBuf::from(String::from_utf8(found.stdout).unwrap().trim());
        let version = fs::read_to_string(
            installation.join("VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"),
        )
        .unwrap();
        let tools = installation
            .join("VC/Tools/MSVC")
            .join(version.trim())
            .join("bin/Hostx64/x64");
        checked(f.command(tools.join("cl.exe")).args([
            "/nologo",
            "/c",
            "native/value.c",
            "/Fonative/value.obj",
        ]));
        checked(f.command(tools.join("lib.exe")).args([
            "/nologo",
            "/OUT:native/audit_native.lib",
            "native/value.obj",
        ]));
    }
}

#[test]
fn native_library_changes_invalidate_a_cached_link() {
    let f = Fixture::new("", "");
    fs::write(
        f.workspace.join("src/app.rs"),
        "unsafe extern \"C\" { fn native_value() -> i32; } fn main() { println!(\"{}\", unsafe { native_value() }); }",
    )
    .unwrap();
    let link = || {
        checked(f.local().arg(BELLOWS).args([
            "rustc",
            "src/app.rs",
            "--edition=2024",
            "--crate-name=app",
            "--crate-type=bin",
            "--emit=dep-info,link",
            "-Cextra-filename=-audit",
            "--out-dir=out",
            "-L",
            "native=native",
            "-l",
            "static=audit_native",
        ]))
    };
    let run = || {
        let output = checked(
            &mut f.command(
                f.workspace
                    .join(format!("out/app-audit{}", std::env::consts::EXE_SUFFIX)),
            ),
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    native_archive(&f, 1);
    assert_eq!(decisions(&link(), "app"), ["CACHE MISS"]);
    assert_eq!(run(), "1");
    fs::remove_dir_all(f.workspace.join("out")).unwrap();
    let restored = link();
    assert_eq!(
        decisions(&restored, "app"),
        ["LOCAL HIT"],
        "{}",
        stderr(&restored)
    );
    assert_eq!(run(), "1");
    native_archive(&f, 2);
    fs::remove_dir_all(f.workspace.join("out")).unwrap();
    let relinked = link();
    assert_eq!(
        decisions(&relinked, "app"),
        ["CACHE MISS"],
        "{}",
        stderr(&relinked)
    );
    assert!(
        stderr(&relinked).contains("input changed"),
        "{}",
        stderr(&relinked)
    );
    assert_eq!(run(), "2");
}

#[test]
fn incremental_compiles_publish_only_from_scratch_sessions() {
    let mut f = Fixture::new(
        "pub fn value() -> u32 { 1 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    f.lock();
    let build = |f: &Fixture| {
        checked(
            f.local()
                .args(["cargo", "build", "--offline"])
                .env("CARGO_INCREMENTAL", "1"),
        )
    };
    let stored = |f: &Fixture| {
        events(f)
            .iter()
            .filter(|e| e.kind == "store" && e.detail.contains("library output"))
            .count()
    };
    build(&f);
    assert_eq!(stored(&f), 1);
    second_checkout(&mut f, "second checkout");
    let restored = build(&f);
    assert_eq!(decisions(&restored, "fixture"), ["LOCAL HIT", "LOCAL HIT"]);
    // No session existed here: the first edit compiles from scratch.
    fs::write(
        f.workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 2 }",
    )
    .unwrap();
    build(&f);
    assert_eq!(stored(&f), 2);
    // The next edit reuses that session and stays in this checkout.
    fs::write(
        f.workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 3 }",
    )
    .unwrap();
    build(&f);
    assert_eq!(stored(&f), 2);
    assert!(
        events(&f)
            .iter()
            .any(|e| e.kind == "not_stored" && e.detail.contains("reused an existing session"))
    );
    let binary = f.workspace.join(format!(
        "target/debug/fixture{}",
        std::env::consts::EXE_SUFFIX
    ));
    let output = checked(&mut f.command(binary));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}

#[test]
fn proc_macro_consumers_share_across_checkouts() {
    let mut f = Fixture::new(
        "pub fn value() -> u32 { derive::answer!() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nderive={path=\"derive\"}\n[workspace]\nmembers=[\"derive\"]\n",
            ),
            (
                "derive/Cargo.toml",
                "[package]\nname=\"derive\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[lib]\nproc-macro=true\n",
            ),
            // Like wit-bindgen: export the (empty) OUT_DIR and bake it in.
            (
                "derive/build.rs",
                "fn main() { println!(\"cargo:rustc-env=MACRO_DEBUG_DIR={}\", std::env::var(\"OUT_DIR\").unwrap()); }",
            ),
            (
                "derive/src/lib.rs",
                "use proc_macro::TokenStream;\nconst DEBUG: &str = env!(\"MACRO_DEBUG_DIR\");\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    if std::env::var_os(\"FIXTURE_MACRO_DEBUG\").is_some() { std::fs::write(std::path::Path::new(DEBUG).join(\"expanded.rs\"), \"42\").unwrap(); }\n    \"42u32\".parse().unwrap()\n}\n",
            ),
        ],
    );
    f.lock();
    f.build();
    second_checkout(&mut f, "second checkout");
    let restored = f.build();
    let log = stderr(&restored);
    assert_eq!(decisions(&restored, "derive"), ["LOCAL HIT"], "{log}");
    assert_eq!(
        decisions(&restored, "fixture"),
        ["LOCAL HIT", "LOCAL HIT"],
        "{log}"
    );
    assert!(log.contains("proc macros: derive"), "{log}");
    assert_eq!(f.value("target"), "42");
    // The substituted directory is real and writable for the macro.
    f.clean();
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("FIXTURE_MACRO_DEBUG", "1")
            .env("CARGO_INCREMENTAL", "0"),
    );
    assert_eq!(f.value("target"), "42");
}

#[test]
fn restore_replaces_an_executable_that_is_still_running() {
    let f = Fixture::new("", "");
    fs::write(
        f.workspace.join("src/app.rs"),
        "fn main() { if std::env::args().nth(1).is_some() { std::thread::sleep(std::time::Duration::from_secs(20)); } println!(\"ready\"); }",
    )
    .unwrap();
    let link = || {
        checked(f.local().arg(BELLOWS).args([
            "rustc",
            "src/app.rs",
            "--edition=2024",
            "--crate-name=app",
            "--crate-type=bin",
            "--emit=dep-info,link",
            "-Cextra-filename=-running",
            "--out-dir=out",
        ]))
    };
    assert_eq!(decisions(&link(), "app"), ["CACHE MISS"]);
    let binary = f
        .workspace
        .join(format!("out/app-running{}", std::env::consts::EXE_SUFFIX));
    // A test binary still executing (Windows locks its image) while Cargo
    // asks for the same unit again.
    let mut running = f.command(&binary).arg("wait").spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let restored = link();
    assert_eq!(
        decisions(&restored, "app"),
        ["LOCAL HIT"],
        "{}",
        stderr(&restored)
    );
    let output = checked(&mut f.command(&binary));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ready");
    running.kill().unwrap();
    let _ = running.wait();
}

/// PATH with `first` in front, and the inherited entries in reverse order:
/// the same programs, spelled as a different shell would.
fn shell_path(first: &Path) -> std::ffi::OsString {
    let mut entries: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect();
    entries.reverse();
    entries.insert(0, first.to_owned());
    std::env::join_paths(entries).unwrap()
}

#[test]
fn the_same_compile_from_another_shell_is_a_hit() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let empty = f.temp.path().join("empty bin");
    fs::create_dir_all(&empty).unwrap();
    // rustup finds its toolchains through HOME outside Windows: the same
    // directory, spelled differently, is still another value.
    let home = match std::env::var_os("HOME") {
        Some(home) if !cfg!(windows) => Path::new(&home).join("."),
        _ => f.temp.path().to_owned(),
    };
    // A POSIX shell: HOME, LANG and LC_ALL set, its own PATH in front.
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("HOME", &home)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("TZ", "UTC")
            .env("PATH", shell_path(&empty)),
    );
    f.clean();
    // Another shell: none of them (HOME as inherited outside Windows), the
    // inherited PATH as it is.
    let mut other = f.local();
    if cfg!(windows) {
        other.env_remove("HOME");
    }
    let other = checked(
        other
            .args(["cargo", "build", "--release", "--offline"])
            .env_remove("LANG")
            .env_remove("LC_ALL")
            .env_remove("TZ"),
    );
    let log = stderr(&other);
    // A C compiler driver (the default on Linux and macOS) may find its own
    // tools through PATH, so there the linked binary keeps PATH in its key.
    let expected: &[&str] = if cfg!(windows) {
        &["LOCAL HIT", "LOCAL HIT"]
    } else {
        &["LOCAL HIT", "CACHE MISS"]
    };
    assert_eq!(decisions(&other, "fixture"), expected, "{log}");
    assert!(!log.contains("environment changed: HOME"), "{log}");
    assert_eq!(f.value("target"), "42");
}

/// Keys still follow the linker that PATH selects: the same lld-link found
/// through another directory shares results, a different one misses.
#[cfg(windows)]
#[test]
fn a_different_lld_link_on_path_misses() {
    let Ok(found) = Command::new("where").arg("lld-link").output() else {
        return;
    };
    let Some(lld) = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .map(|line| PathBuf::from(line.trim()))
        .filter(|path| path.is_file())
    else {
        eprintln!("skipped: no lld-link on PATH");
        return;
    };
    let f = Fixture::new(
        "pub fn value() -> u32 { 7 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let build = |dir: &Path| {
        checked(
            f.local()
                .args(["cargo", "build", "--release", "--offline"])
                .env("RUSTFLAGS", "-Clinker-flavor=lld-link")
                .env("PATH", shell_path(dir)),
        )
    };
    let same = f.temp.path().join("same lld");
    let other = f.temp.path().join("other lld");
    for dir in [&same, &other] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::copy(&lld, same.join("lld-link.exe")).unwrap();
    // The same program with trailing bytes: it still links, but it is not
    // the linker the cached binary was made with.
    let mut bytes = fs::read(&lld).unwrap();
    bytes.extend_from_slice(b"another build");
    fs::write(other.join("lld-link.exe"), bytes).unwrap();

    build(&same);
    f.clean();
    let empty = f.temp.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let again = build(&empty);
    assert_eq!(
        decisions(&again, "fixture"),
        ["LOCAL HIT", "LOCAL HIT"],
        "{}",
        stderr(&again)
    );
    f.clean();
    let changed = build(&other);
    let log = stderr(&changed);
    assert_eq!(
        decisions(&changed, "fixture"),
        ["LOCAL HIT", "CACHE MISS"],
        "{log}"
    );
    assert!(log.contains("linker on PATH changed: lld-link"), "{log}");
    assert_eq!(f.value("target"), "7");
}

#[test]
fn profiles_with_identical_compiler_arguments_share_results() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let manifest = fs::read_to_string(f.workspace.join("Cargo.toml")).unwrap();
    fs::write(
        f.workspace.join("Cargo.toml"),
        format!("{manifest}[profile.fast]\ninherits=\"release\"\n"),
    )
    .unwrap();
    f.build();
    let fast = checked(
        f.local()
            .args(["cargo", "build", "--profile", "fast", "--offline"]),
    );
    assert_eq!(
        decisions(&fast, "fixture"),
        ["LOCAL HIT", "LOCAL HIT"],
        "{}",
        stderr(&fast)
    );
    let output = checked(&mut f.command(f.workspace.join(format!(
        "target/fast/fixture{}",
        std::env::consts::EXE_SUFFIX
    ))));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn read_only_clients_restore_but_never_publish() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let read_only = || {
        checked(
            f.local()
                .args(["cargo", "build", "--release", "--offline"])
                .env("BELLOWS_READ_ONLY", "1"),
        )
    };
    read_only();
    assert!(!events(&f).iter().any(|e| e.kind == "store"));
    f.clean();
    let still_cold = read_only();
    assert!(
        !stderr(&still_cold).contains("HIT"),
        "{}",
        stderr(&still_cold)
    );
    f.clean();
    f.build();
    f.clean();
    let restored = read_only();
    assert_eq!(decisions(&restored, "fixture"), ["LOCAL HIT", "LOCAL HIT"]);
    assert_eq!(f.value("target"), "42");
}

#[test]
fn build_script_runs_are_restored_and_track_declared_inputs() {
    let mut f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
            ),
            ("data.txt", "7"),
            (
                "build.rs",
                "fn main() {\n    println!(\"cargo:rerun-if-changed=data.txt\");\n    let value = std::fs::read_to_string(\"data.txt\").unwrap();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/generated.rs\"), format!(\"pub fn value() -> u32 {{ {} }}\", value.trim())).unwrap();\n    println!(\"cargo:rustc-cfg=generated\");\n    // Restored directives become later rustc arguments; spelled differently,\n    // they would split every dependent's cache key (Windows verbatim paths).\n    println!(\"cargo:rustc-link-search=native={out}\");\n    println!(\"cargo:rustc-link-search=native={}\", std::env::var(\"CARGO_MANIFEST_DIR\").unwrap());\n}\n",
            ),
        ],
    );
    f.lock();
    let script = |f: &Fixture| {
        events(f)
            .into_iter()
            .filter(|e| e.crate_name == "build-script:fixture")
            .map(|e| e.kind)
            .collect::<Vec<_>>()
    };
    f.build();
    assert_eq!(script(&f), ["miss", "store"]);
    assert_eq!(f.value("target"), "7");
    second_checkout(&mut f, "second checkout");
    let restored = f.build();
    let log = stderr(&restored);
    assert_eq!(script(&f), ["miss", "store", "l1_hit"], "{log}");
    assert_eq!(
        decisions(&restored, "fixture"),
        ["LOCAL HIT", "LOCAL HIT"],
        "{log}"
    );
    assert_eq!(f.value("target"), "7");
    // A declared input changes: the script runs again and so does the crate.
    fs::write(f.workspace.join("data.txt"), "9").unwrap();
    f.build();
    // The rerun starts over the restored OUT_DIR, so it stays local.
    assert_eq!(
        script(&f),
        ["miss", "store", "l1_hit", "miss", "not_stored"]
    );
    assert_eq!(f.value("target"), "9");
    // Plain Cargo in the same target directory still runs the real script.
    fs::write(f.workspace.join("data.txt"), "11").unwrap();
    checked(f.command("cargo").args(["build", "--release", "--offline"]));
    assert_eq!(f.value("target"), "11");
}

/// A fixture whose build script reports how many entries `notes/` holds,
/// declaring the directory (or nothing at all, which makes the whole package
/// its input).
fn notes_fixture(declare: bool) -> Fixture {
    let f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let rerun = if declare {
        "    println!(\"cargo:rerun-if-changed=notes\");\n"
    } else {
        ""
    };
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
            ),
            (
                "build.rs",
                &format!(
                    "fn main() {{\n{rerun}    let count = std::fs::read_dir(\"notes\").unwrap().count();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{{out}}/generated.rs\"), format!(\"pub fn value() -> usize {{{{ {{count}} }}}}\")).unwrap();\n}}\n"
                ),
            ),
        ],
    );
    fs::create_dir_all(f.workspace.join("notes")).unwrap();
    f.lock();
    f
}

/// Build a fresh checkout after `change`; the script's value and its cache
/// decision there.
fn notes_checkout(f: &mut Fixture, name: &str, change: impl FnOnce(&Path)) -> (String, String) {
    second_checkout(f, name);
    change(&f.workspace.join("notes"));
    let before = events(f).len();
    f.build();
    let decision = events(f)[before..]
        .iter()
        .filter(|e| e.crate_name == "build-script:fixture")
        .map(|e| e.kind.clone())
        .next()
        .expect("a build-script decision");
    (f.value("target"), decision)
}

#[test]
fn build_script_directory_inputs_track_added_removed_and_renamed_files() {
    let mut f = notes_fixture(true);
    // Recorded with an empty watched directory, as manifold-net-protocol's
    // protocol-bumps/ once was.
    f.build();
    assert_eq!(f.value("target"), "0");
    let add = |notes: &Path| fs::write(notes.join("a.toml"), "").unwrap();
    assert_eq!(
        notes_checkout(&mut f, "added", add),
        ("1".into(), "miss".into())
    );
    let rename = |notes: &Path| fs::rename(notes.join("a.toml"), notes.join("b.toml")).unwrap();
    assert_eq!(
        notes_checkout(&mut f, "renamed", rename),
        ("1".into(), "miss".into())
    );
    let add_c = |notes: &Path| fs::write(notes.join("c.toml"), "").unwrap();
    assert_eq!(
        notes_checkout(&mut f, "two", add_c),
        ("2".into(), "miss".into())
    );
    let remove = |notes: &Path| fs::remove_file(notes.join("b.toml")).unwrap();
    assert_eq!(
        notes_checkout(&mut f, "removed", remove),
        ("1".into(), "miss".into())
    );
    let nested = |notes: &Path| fs::create_dir(notes.join("drafts")).unwrap();
    assert_eq!(
        notes_checkout(&mut f, "nested", nested),
        ("2".into(), "miss".into())
    );
    // An unchanged directory is still a hit.
    assert_eq!(
        notes_checkout(&mut f, "same", |_| ()),
        ("2".into(), "l1_hit".into())
    );
}

#[test]
fn build_scripts_without_declared_inputs_track_new_package_files() {
    let mut f = notes_fixture(false);
    f.build();
    assert_eq!(f.value("target"), "0");
    let add = |notes: &Path| fs::write(notes.join("a.toml"), "").unwrap();
    assert_eq!(
        notes_checkout(&mut f, "added", add),
        ("1".into(), "miss".into())
    );
    assert_eq!(
        notes_checkout(&mut f, "same", |_| ()),
        ("1".into(), "l1_hit".into())
    );
}

#[test]
fn repeated_builds_stay_cargo_no_ops_after_compiles_and_restores() {
    let mut f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            // A build-dependency compiled in the same build: Cargo compares
            // its rlib's mtime with the build-script executable's.
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[build-dependencies]\nhelper={path=\"helper\"}\n[workspace]\nmembers=[\"helper\"]\n",
            ),
            (
                "helper/Cargo.toml",
                "[package]\nname=\"helper\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
            ),
            ("helper/src/lib.rs", "pub fn value() -> u32 { 5 }"),
            (
                "build.rs",
                "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); let out = std::env::var(\"OUT_DIR\").unwrap(); std::fs::write(format!(\"{out}/generated.rs\"), format!(\"pub fn value() -> u32 {{ {} }}\", helper::value())).unwrap(); }",
            ),
        ],
    );
    f.lock();
    // An installed Bellows is usually older than anything it builds; a
    // launcher that inherited its mtime made Cargo rebuild every dependent.
    let aged = f
        .temp
        .path()
        .join(format!("bellows-aged{}", std::env::consts::EXE_SUFFIX));
    fs::copy(BELLOWS, &aged).unwrap();
    fs::File::options()
        .write(true)
        .open(&aged)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    let build = |f: &Fixture| {
        checked(
            f.command(&aged)
                .args(["local", "--cache-dir"])
                .arg(&f.cache)
                .args(["--", "cargo", "build", "--release", "--offline"]),
        )
    };
    let decisions_so_far = |f: &Fixture| {
        events(f)
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "hit" | "l1_hit" | "miss" | "bypass"))
            .count()
    };
    build(&f);
    let after_compile = decisions_so_far(&f);
    let noop = build(&f);
    assert!(
        stderr(&noop).contains("0 reused · 0 rebuilt"),
        "{}",
        stderr(&noop)
    );
    assert_eq!(decisions_so_far(&f), after_compile);
    // The same holds after every unit was restored in another checkout.
    second_checkout(&mut f, "second checkout");
    build(&f);
    let after_restore = decisions_so_far(&f);
    let noop = build(&f);
    assert!(
        stderr(&noop).contains("0 reused · 0 rebuilt"),
        "{}",
        stderr(&noop)
    );
    assert_eq!(decisions_so_far(&f), after_restore);
    assert_eq!(f.value("target"), "5");
}

#[test]
fn diagnostics_show_real_paths_for_snapshot_tests() {
    let f = Fixture::new("", "");
    let source = f.workspace.join("ui/warn.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "fn main() { let unused = 1; }").unwrap();
    let compile = |path: &std::path::Path| {
        f.local()
            .arg(BELLOWS)
            .arg("rustc")
            .arg(path)
            .args([
                "--edition=2024",
                "--crate-name=warn",
                "--crate-type=bin",
                "--emit=dep-info,link",
                "-Cextra-filename=-ui",
                "--out-dir=out",
            ])
            .output()
            .unwrap()
    };
    // trybuild compiles files by absolute path and snapshots the messages.
    let expected = format!("{}", source.display());
    for attempt in ["miss", "hit"] {
        let output = compile(&source);
        let log = stderr(&output);
        assert!(output.status.success(), "{log}");
        assert!(log.contains(&expected), "{attempt}: {log}");
        assert!(!log.contains("/bellows/"), "{attempt}: {log}");
        fs::remove_dir_all(f.workspace.join("out")).unwrap();
    }
    let failing = f.workspace.join("ui/fail.rs");
    fs::write(&failing, "fn main() { let x: u32 = \"no\"; }").unwrap();
    let output = compile(&failing);
    assert!(!output.status.success());
    let log = stderr(&output);
    assert!(log.contains(&format!("{}", failing.display())), "{log}");
    assert!(!log.contains("/bellows/"), "{log}");
}

#[test]
fn repeated_clippy_runs_stay_no_ops_with_intact_dependency_info() {
    let f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
            ),
            (
                "build.rs",
                "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); let out = std::env::var(\"OUT_DIR\").unwrap(); std::fs::write(format!(\"{out}/generated.rs\"), \"pub fn value() -> u32 { 5 }\").unwrap(); }",
            ),
        ],
    );
    f.lock();
    let clippy = || {
        checked(
            f.local()
                .args(["cargo", "clippy", "--all-targets", "--offline"]),
        )
    };
    clippy();
    let noop = clippy();
    assert!(
        stderr(&noop).contains("0 reused · 0 rebuilt"),
        "{}",
        stderr(&noop)
    );
    let build = f.workspace.join("target/debug/build");
    for entry in fs::read_dir(build).unwrap() {
        for file in fs::read_dir(entry.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "d") {
                let text = fs::read_to_string(&path).unwrap();
                for line in text.lines().filter(|l| l.starts_with("# env-dep:")) {
                    assert!(
                        line.starts_with("# env-dep:CLIPPY")
                            || line.starts_with("# env-dep:OUT_DIR"),
                        "{}: {line}",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn diagnostics_quote_restored_dependency_sources_like_plain_cargo() {
    let f = Fixture::new("", "");
    // Like trybuild: a separate package (compiled from its own directory as
    // a path dependency) whose bound is quoted in a consumer's error.
    write_files(
        &f.workspace,
        &[
            (
                "engine/Cargo.toml",
                "[package]\nname=\"engine\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
            ),
            (
                "engine/src/lib.rs",
                "pub struct Server;\npub trait Side {}\nimpl Side for Server {}\npub fn register<S: Side>() {}\n",
            ),
            (
                "ui/Cargo.toml",
                "[package]\nname=\"ui\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nengine={path=\"../engine\"}\n[workspace]\n",
            ),
            (
                "ui/src/lib.rs",
                "pub fn wrong() { engine::register::<u8>(); }\n",
            ),
        ],
    );
    let manifest = |f: &Fixture| f.workspace.join("ui/Cargo.toml");
    let bellows = |f: &Fixture| {
        f.local()
            .args(["cargo", "build", "--offline", "--manifest-path"])
            .arg(manifest(f))
            .env("CARGO_TARGET_DIR", f.workspace.join("target"))
            .output()
            .unwrap()
    };
    let diagnostic = |output: &Output| {
        let text = stderr(output);
        let start = text.find("error[E0277]").expect(&text);
        let end = text[start..]
            .find("\n\n")
            .map_or(text.len(), |end| start + end);
        text[start..end].to_owned()
    };
    assert!(!bellows(&f).status.success());
    // Restore the dependency into a clean target. (Across checkouts Cargo
    // itself gives an out-of-workspace path dependency a different
    // `-C metadata`, so it is rebuilt there.)
    f.clean();
    let cached = bellows(&f);
    let log = stderr(&cached);
    assert_eq!(decisions(&cached, "engine"), ["LOCAL HIT"], "{log}");
    let plain = f
        .command("cargo")
        .args(["build", "--offline", "--manifest-path"])
        .arg(manifest(&f))
        .env("CARGO_TARGET_DIR", f.workspace.join("plain-target"))
        .output()
        .unwrap();
    assert_eq!(diagnostic(&cached), diagnostic(&plain));
    assert!(
        diagnostic(&cached).contains("pub fn register<S: Side>()"),
        "{log}"
    );
    assert!(!log.contains("/bellows/"), "{log}");
}

#[test]
fn diagnostics_quote_checkout_sources_from_a_project_outside_the_checkout() {
    // Like trybuild with a target directory outside the checkout: Cargo runs
    // the consumer from a generated project there, but its crate root and the
    // path dependency it quotes live in the checkout.
    let f = Fixture::new("", "");
    write_files(
        &f.workspace,
        &[
            (
                "engine/Cargo.toml",
                "[package]\nname=\"engine\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
            ),
            (
                "engine/src/lib.rs",
                "pub struct Server;\npub trait Side {}\nimpl Side for Server {}\npub fn register<S: Side>() {}\n",
            ),
            (
                "ui/wrong.rs",
                "pub fn wrong() { engine::register::<u8>(); }\n",
            ),
        ],
    );
    let outside = f.temp.path().join("target outside/tests/trybuild/ui");
    let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
    write_files(
        &outside,
        &[(
            "Cargo.toml",
            &format!(
                "[package]\nname=\"ui\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[lib]\npath=\"{}\"\n[dependencies]\nengine={{path=\"{}\"}}\n[workspace]\n",
                path(&f.workspace.join("ui/wrong.rs")),
                path(&f.workspace.join("engine")),
            ),
        )],
    );
    let build = |command: &mut Command, target: &str| {
        command
            .args(["build", "--offline", "--manifest-path"])
            .arg(outside.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", f.temp.path().join(target))
            .output()
            .unwrap()
    };
    let diagnostic = |output: &Output| {
        let text = stderr(output);
        let start = text.find("error[E0277]").expect(&text);
        let end = text[start..]
            .find("\n\n")
            .map_or(text.len(), |end| start + end);
        text[start..end].to_owned()
    };
    let bellows = build(f.local().arg("cargo"), "target outside");
    let plain = build(&mut f.command("cargo"), "plain target");
    let log = stderr(&bellows);
    assert_eq!(diagnostic(&bellows), diagnostic(&plain), "{log}");
    assert!(
        diagnostic(&bellows).contains("pub fn register<S: Side>()"),
        "{log}"
    );
    assert!(
        !log.contains("/bellows/") && !log.contains("trybuild/ui/engine"),
        "{log}"
    );
}

#[test]
fn sessions_launched_from_subdirectories_never_share_another_checkouts_inputs() {
    // Like manifold-mod-macros: a build script bakes the absolute path of a
    // sibling data tree (its WIT files), and the macro reads that path while
    // expanding. A session launched from a crate subdirectory must still
    // treat the whole git checkout as this checkout.
    let mut f = Fixture::new(
        "pub fn value() -> &'static str { macros::data!() }",
        "fn main() { println!(\"{}\", fixture::value().trim()); }",
    );
    write_files(
        &f.workspace,
        &[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nmacros={path=\"macros\"}\n[workspace]\nmembers=[\"macros\"]\n",
            ),
            (
                "macros/Cargo.toml",
                "[package]\nname=\"macros\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[lib]\nproc-macro=true\n",
            ),
            (
                "macros/build.rs",
                "fn main() {\n    let data = std::path::Path::new(\"../data\").canonicalize().unwrap().join(\"value.txt\");\n    println!(\"cargo:rerun-if-changed=../data\");\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/paths.rs\"), format!(\"const DATA: &str = {:?};\", data.display().to_string())).unwrap();\n}\n",
            ),
            (
                "macros/src/lib.rs",
                "use proc_macro::TokenStream;\ninclude!(concat!(env!(\"OUT_DIR\"), \"/paths.rs\"));\n#[proc_macro]\npub fn data(_: TokenStream) -> TokenStream { format!(\"include_str!({DATA:?})\").parse().unwrap() }\n",
            ),
            ("data/value.txt", "one"),
            ("sub/README", "launch directory"),
        ],
    );
    f.lock();
    let build = |f: &Fixture| {
        let mut command = f.local();
        command
            .args([
                "cargo",
                "build",
                "--release",
                "--offline",
                "--manifest-path",
            ])
            .arg(f.workspace.join("Cargo.toml"))
            .current_dir(f.workspace.join("sub"));
        checked(&mut command)
    };
    build(&f);
    assert_eq!(f.value("target"), "one");
    second_checkout(&mut f, "second checkout");
    fs::write(f.workspace.join("data/value.txt"), "two").unwrap();
    build(&f);
    assert_eq!(f.value("target"), "two");
    // A changed input in this checkout invalidates every dependent result.
    fs::write(f.workspace.join("data/value.txt"), "three").unwrap();
    build(&f);
    assert_eq!(f.value("target"), "three");
}

#[test]
fn collection_dry_runs_change_nothing_and_reuse_is_journaled() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let gc = |flags: &[&str]| -> Value {
        let output = checked(
            f.command(BELLOWS)
                .args(["gc", "--local", "--cache-dir"])
                .arg(&f.cache)
                .args(flags)
                .arg("--json"),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    f.build();
    let published = gc(&["--max-mb", "0", "--dry-run"]);
    assert_eq!(published["dry_run"], true);
    assert!(published["records"].as_u64().unwrap() >= 2, "{published}");
    assert_eq!(
        published["records_evicted"], published["records"],
        "{published}"
    );
    let publications = published["journal_entries"].as_u64().unwrap();
    assert!(publications >= 2, "{published}");

    // The dry run removed nothing: a clean build is restored, and each reuse
    // is recorded for least-recently-used collection.
    f.clean();
    let restored = f.build();
    assert_eq!(decisions(&restored, "fixture"), ["LOCAL HIT", "LOCAL HIT"]);
    let reused = gc(&["--max-mb", "0", "--dry-run"]);
    assert_eq!(
        reused["journal_entries"].as_u64().unwrap(),
        publications,
        "a reuse refreshes an existing entry: {reused}"
    );
    assert!(
        reused["breakdown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| bucket["group"] == "kind" && bucket["label"] == "executable"),
        "{reused}"
    );

    // An executable budget of zero plans to evict the binary alone, however
    // generous the overall budget, and keeps the library it links.
    let executables = gc(&[
        "--max-mb",
        "100000",
        "--max-executable-mb",
        "0",
        "--dry-run",
    ]);
    assert_eq!(executables["records_evicted"], 1, "{executables}");
    assert_eq!(
        executables["max_executable_bytes"], 0,
        "the collection applied the executable budget: {executables}"
    );
    assert!(
        executables["breakdown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| {
                bucket["group"] == "kind"
                    && bucket["label"] == "executable"
                    && bucket["evicted_records"] == 1
            }),
        "{executables}"
    );

    // Within budget nothing is evicted; at zero every record goes, and the
    // next build recompiles correctly.
    let kept = gc(&["--max-mb", "100000"]);
    assert_eq!(kept["records_evicted"], 0, "{kept}");
    let collected = gc(&["--max-mb", "0"]);
    assert_eq!(
        collected["records_evicted"], collected["records"],
        "{collected}"
    );
    f.clean();
    let rebuilt = f.build();
    assert!(!stderr(&rebuilt).contains("HIT"), "{}", stderr(&rebuilt));
    assert_eq!(f.value("target"), "42");
}

#[test]
fn a_server_lost_during_a_build_is_reported_once() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    // A server that answers the session's health check, then goes away (a
    // bellowsd restart mid-build).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let output = checked(f.command(BELLOWS).args([
        "run",
        "--server",
        &format!("http://127.0.0.1:{port}"),
        "--",
        "cargo",
        "build",
        "--release",
        "--offline",
    ]));
    server.join().unwrap();
    let text = stderr(&output);
    assert_eq!(
        text.matches("stopped answering during this build").count(),
        1,
        "one warning for the session: {text}"
    );
    assert_eq!(f.value("target"), "42");
}

#[test]
fn an_unreachable_server_is_reported_loudly_and_can_be_required() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let run = || {
        let mut command = f.command(BELLOWS);
        command.args([
            "run",
            "--server",
            "http://127.0.0.1:1",
            "--",
            "cargo",
            "build",
            "--release",
            "--offline",
        ]);
        command
    };
    let fallback = checked(&mut run());
    let text = stderr(&fallback);
    assert_eq!(
        text.matches("server unreachable").count(),
        2,
        "warned before and after the build: {text}"
    );
    assert_eq!(f.value("target"), "42");
    let required = run().env("BELLOWS_REQUIRE_SERVER", "1").output().unwrap();
    assert!(!required.status.success());
    assert!(stderr(&required).contains("BELLOWS_REQUIRE_SERVER"));
}

/// A compile whose input `src/value.rs` is rewritten from 42 to 43 by `edit`
/// (a shell command run once, after the real rustc has read the old content
/// and before Bellows hashes the inputs). Neither the edit nor any later build
/// may restore the artifact compiled from 42.
#[cfg(unix)]
fn assert_input_edited_during_the_compile_is_never_stored(edit: &str) {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(
        "mod value; pub fn value() -> u32 { value::value() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let value = f.workspace.join("src/value.rs");
    fs::write(&value, "pub fn value() -> u32 { 42 }").unwrap();
    let marker = f.temp.path().join("edit-once");
    fs::write(&marker, "").unwrap();
    let staged = f.temp.path().join("value-43.rs");
    fs::write(&staged, "pub fn value() -> u32 { 43 }").unwrap();
    let shim = f.temp.path().join("rustc-shim");
    let real = String::from_utf8(checked(Command::new("rustup").args(["which", "rustc"])).stdout)
        .unwrap()
        .trim()
        .to_owned();
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\"{real}\" \"$@\"\nstatus=$?\n\
             case \" $* \" in *\" --crate-name fixture \"*\"--crate-type lib\"*)\n\
             if [ -e \"{marker}\" ]; then rm \"{marker}\"; sleep 1; \
             STAGED=\"{staged}\" VALUE=\"{value}\"; {edit}; fi ;;\nesac\nexit $status\n",
            marker = marker.display(),
            staged = staged.display(),
            value = value.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let build = || {
        checked(
            f.local()
                .env("RUSTC", &shim)
                .args(["cargo", "build", "--release", "--offline"]),
        )
    };
    let first = build();
    assert!(!marker.exists(), "the shim never edited value.rs");
    assert_eq!(
        fs::read_to_string(&value).unwrap(),
        "pub fn value() -> u32 { 43 }"
    );
    assert!(
        stderr(&first).contains("changed during the compile"),
        "{}",
        stderr(&first)
    );
    // Rebuild until cargo has compiled the library again (an edit that keeps
    // an old modification time does not make cargo's own record stale).
    fs::remove_dir_all(f.workspace.join("target")).unwrap();
    let second = build();
    assert!(
        !stderr(&second).contains("LOCAL HIT fixture"),
        "{}",
        stderr(&second)
    );
    assert_eq!(f.value("target"), "43");
}

/// An input that changes while rustc runs (an editor save, or a `git checkout`
/// into the same worktree) must never be recorded with the artifact compiled
/// from its earlier content: the next build of the new content would restore
/// the old code (Manifold #722 bisect, a stale `manifold_mod_types`).
#[cfg(unix)]
#[test]
fn an_input_edited_during_the_compile_is_never_stored() {
    assert_input_edited_during_the_compile_is_never_stored("cat \"$STAGED\" > \"$VALUE\"");
}

/// The same when the replacement keeps an older modification time (`cp -p`,
/// `rsync -t`, archive extraction): the change time still moves.
#[cfg(unix)]
#[test]
fn an_input_replaced_with_an_old_modification_time_is_never_stored() {
    assert_input_edited_during_the_compile_is_never_stored(
        "touch -d '2001-01-01' \"$STAGED\"; cp -p \"$STAGED\" \"$VALUE\"",
    );
}

/// The build-script counterpart of `an_input_edited_during_the_compile_is_never_stored`:
/// a declared input rewritten while the script runs must not be recorded with
/// the output generated from its earlier content.
#[test]
fn a_build_script_input_edited_while_it_runs_is_never_stored() {
    let f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let marker = f.temp.path().join("edit-once");
    fs::write(&marker, "").unwrap();
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
            ),
            ("data.txt", "7"),
            (
                "build.rs",
                &format!(
                    "fn main() {{\n    println!(\"cargo:rerun-if-changed=data.txt\");\n    let value = std::fs::read_to_string(\"data.txt\").unwrap();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{{out}}/generated.rs\"), format!(\"pub fn value() -> u32 {{{{ {{}} }}}}\", value.trim())).unwrap();\n    // Once: the input changes after the script read it.\n    if std::fs::remove_file({marker:?}).is_ok() {{\n        std::thread::sleep(std::time::Duration::from_millis(200));\n        std::fs::write(\"data.txt\", \"9\").unwrap();\n    }}\n}}\n",
                    marker = marker.to_string_lossy()
                ),
            ),
        ],
    );
    f.lock();
    let first = f.build();
    assert!(!marker.exists(), "the build script never edited data.txt");
    assert!(
        stderr(&first).contains("changed while the build script ran"),
        "{}",
        stderr(&first)
    );
    assert_eq!(f.value("target"), "7");
    // data.txt is newer than cargo's record: the script reruns, and it reads 9.
    f.build();
    assert_eq!(f.value("target"), "9");
}

/// A build script that declares no inputs is recorded against its whole
/// package directory, judged by the directory's listing from before the run
/// (Cargo creates `target` there just before, in a fresh checkout): a change
/// that leaves the listing as it was keeps the result storable, and an entry
/// removed meanwhile does not.
#[test]
fn undeclared_build_script_inputs_are_judged_by_the_package_listing() {
    let run = |during: &str| {
        let f = Fixture::new(
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
            "fn main() { println!(\"{}\", fixture::value()); }",
        );
        write_files(
            &f.workspace,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
                ),
                ("spare.txt", "spare"),
                (
                    "build.rs",
                    &format!(
                        "fn main() {{\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{{out}}/generated.rs\"), \"pub fn value() -> u32 {{ 1 }}\").unwrap();\n    {during}\n}}\n"
                    ),
                ),
            ],
        );
        f.lock();
        f.build();
        events(&f)
            .into_iter()
            .filter(|e| e.crate_name == "build-script:fixture")
            .map(|e| (e.kind, e.detail))
            .collect::<Vec<_>>()
    };
    // The package directory's timestamp moves, its listing does not.
    let unchanged = run(
        "std::thread::sleep(std::time::Duration::from_millis(200)); std::fs::write(\"scratch.tmp\", \"\").unwrap(); std::fs::remove_file(\"scratch.tmp\").unwrap();",
    );
    assert!(
        unchanged.iter().any(|(kind, _)| kind == "store"),
        "{unchanged:?}"
    );
    // A package file disappears while the script runs.
    let removed = run(
        "std::thread::sleep(std::time::Duration::from_millis(200)); std::fs::remove_file(\"spare.txt\").unwrap();",
    );
    assert!(
        removed.iter().any(|(kind, detail)| kind == "not_stored"
            && detail.contains("changed while the build script ran")),
        "{removed:?}"
    );
}

/// A build script that declares no inputs is keyed by every file in its
/// package directory, by content, not only by the directory's listing: an
/// in-place edit with an unchanged listing reruns it.
#[test]
fn undeclared_build_script_inputs_track_content_edits() {
    let mut f = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    write_files(
        &f.workspace,
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nbuild=\"build.rs\"\n[workspace]\n",
            ),
            ("data.txt", "7"),
            (
                "build.rs",
                "fn main() {\n    let value = std::fs::read_to_string(\"data.txt\").unwrap();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/generated.rs\"), format!(\"pub fn value() -> u32 {{ {} }}\", value.trim())).unwrap();\n}\n",
            ),
        ],
    );
    f.lock();
    let script = |f: &Fixture, before: usize| {
        events(f)[before..]
            .iter()
            .filter(|e| e.crate_name == "build-script:fixture")
            .map(|e| e.kind.clone())
            .next()
            .expect("a build-script decision")
    };
    f.build();
    assert_eq!(f.value("target"), "7");
    // Same listing, new content.
    second_checkout(&mut f, "edited");
    fs::write(f.workspace.join("data.txt"), "9").unwrap();
    let before = events(&f).len();
    f.build();
    assert_eq!(script(&f, before), "miss");
    assert_eq!(f.value("target"), "9");
    // The edited content is itself reusable.
    second_checkout(&mut f, "edited again");
    fs::write(f.workspace.join("data.txt"), "9").unwrap();
    let before = events(&f).len();
    f.build();
    assert_eq!(script(&f, before), "l1_hit");
    assert_eq!(f.value("target"), "9");
}
