use super::*;

#[test]
fn trees_round_trip_and_reject_escapes() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("out");
    fs::create_dir_all(out.join("nested/deeper")).unwrap();
    fs::create_dir_all(out.join("nested-target")).unwrap();
    fs::write(out.join("nested-target/CACHEDIR.TAG"), "Signature").unwrap();
    fs::write(out.join("nested-target/huge.rlib"), "scratch").unwrap();
    fs::write(out.join("gen.rs"), "pub const A: u8 = 1;").unwrap();
    fs::write(out.join("nested/deeper/lib.a"), [0u8, 1, 2]).unwrap();
    let archive = pack_tree(&out).unwrap();
    let files = unpack_tree(&archive).unwrap();
    let names = files
        .iter()
        .map(|(path, _, _)| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    assert_eq!(names, ["gen.rs", "nested/deeper/lib.a"]);
    assert!(tree_is_empty(&temp.path().join("missing")).unwrap());
    assert!(!tree_is_empty(&out).unwrap());
    fs::remove_file(out.join("gen.rs")).unwrap();
    fs::remove_dir_all(out.join("nested")).unwrap();
    assert!(tree_is_empty(&out).unwrap());

    let mut forged = TREE_MAGIC.to_vec();
    forged.extend(br#"{"path":"../escape","len":1,"executable":false}"#);
    forged.extend(b"\nx");
    assert!(unpack_tree(&forged).is_err());
}

#[test]
fn launchers_are_private_copies_and_removed_before_recompiling() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("build_script_build-abc");
    fs::write(&script, b"real script").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&script)
        .unwrap()
        .set_modified(old)
        .unwrap();
    install_launcher(temp.path(), "build_script_build-abc").unwrap();
    assert_eq!(fs::metadata(&script).unwrap().modified().unwrap(), old);
    // Writing through the installed launcher must not touch this
    // process's executable (an in-place linker write).
    fs::write(&script, b"relinked in place").unwrap();
    assert_ne!(
        fs::read(env::current_exe().unwrap()).unwrap(),
        b"relinked in place"
    );
    assert_eq!(
        fs::read(temp.path().join(".bellows-real-build_script_build-abc")).unwrap(),
        b"real script"
    );
    remove_launcher(temp.path(), "build_script_build-abc");
    assert!(!script.exists());
    assert!(!temp.path().join(RECORD).exists());
    assert!(
        !temp
            .path()
            .join(".bellows-real-build_script_build-abc")
            .exists()
    );
}

#[test]
fn directives_follow_cargos_rerun_model() {
    let (files, env) = declared_inputs(
        b"cargo:rerun-if-changed=build.rs\ncargo::rerun-if-changed=src\ncargo:rerun-if-env-changed=CC\ncargo:rustc-link-lib=z\n",
    );
    assert_eq!(files, ["build.rs", "src"]);
    assert_eq!(env, ["CC"]);
}

#[test]
fn launchers_are_recognized_only_with_their_record() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("build-script-build");
    assert!(launched_as(&[script.clone().into_os_string()]).is_none());
    fs::write(
        temp.path().join(RECORD),
        br#"{"real":".bellows-real-build_script_build-abc"}"#,
    )
    .unwrap();
    assert_eq!(
        launched_as(&[script.into_os_string()]),
        Some(temp.path().join(".bellows-real-build_script_build-abc"))
    );
    fs::write(temp.path().join(RECORD), br#"{"real":"../escape"}"#).unwrap();
    assert!(launched_as(&[temp.path().join("build-script-build").into_os_string()]).is_none());
    assert!(launched_as(&[temp.path().join("bellows").into_os_string()]).is_none());
    assert!(is_build_script_compile(
        "build_script_build",
        Path::new("/t/debug/build/zstd-sys-abc")
    ));
    assert!(!is_build_script_compile(
        "build_script_build",
        Path::new("/t/debug/deps")
    ));
}
