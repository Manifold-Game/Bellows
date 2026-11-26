use super::*;

#[test]
fn self_contained_linkers_resolve_from_the_sysroot() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("lib/rustlib/x86_64-host/bin");
    std::fs::create_dir_all(bin.join("gcc-ld")).unwrap();
    let name = if cfg!(windows) {
        "rust-lld.exe"
    } else {
        "rust-lld"
    };
    std::fs::write(bin.join(name), b"lld").unwrap();
    std::fs::write(
        bin.join("gcc-ld").join(if cfg!(windows) {
            "ld.lld.exe"
        } else {
            "ld.lld"
        }),
        b"lld",
    )
    .unwrap();
    assert_eq!(
        sysroot_tool("rust-lld", temp.path()),
        Some(bin.join(name).canonicalize().unwrap())
    );
    assert!(sysroot_tool("ld.lld", temp.path()).is_some());
    assert_eq!(sysroot_tool("missing-linker", temp.path()), None);
    assert_eq!(sysroot_tool("bin/rust-lld", temp.path()), None);
}

#[test]
fn parses_rustc_linker_command_rendering() {
    let text = r#"LC_ALL="C" PATH="/a:/b" VSLANG="1033" "cc" "-m64" "/tmp/x y/a.o" "-Wl,--as-needed" "-lgcc_s" "-L" "/opt/lib" "quote\"d" "back\\slash" "\u{e9}""#;
    let command = parse_link_command(text).unwrap();
    assert_eq!(
        command.env,
        vec![
            ("LC_ALL".into(), "C".into()),
            ("PATH".into(), "/a:/b".into()),
            ("VSLANG".into(), "1033".into())
        ]
    );
    assert_eq!(command.program, "cc");
    assert_eq!(
        command.args,
        [
            "-m64",
            "/tmp/x y/a.o",
            "-Wl,--as-needed",
            "-lgcc_s",
            "-L",
            "/opt/lib",
            "quote\"d",
            "back\\slash",
            "é"
        ]
    );
    assert!(parse_link_command(r#"cd "/x" && "cc""#).is_err());
    assert!(parse_link_command(r#""cc" "unterminated"#).is_err());
}

#[test]
fn library_requests_cover_every_resolvable_file_name() {
    assert_eq!(
        LibraryRequest::unix("z").file_names(),
        ["libz.so", "libz.a", "libz.dylib", "libz.tbd"]
    );
    assert_eq!(LibraryRequest::unix(":libq.a").file_names(), ["libq.a"]);
    assert_eq!(
        LibraryRequest::msvc("kernel32").file_names(),
        ["kernel32.lib", "kernel32"]
    );
    assert_eq!(
        LibraryRequest::msvc("ws2_32.lib").file_names(),
        ["ws2_32.lib"]
    );
}

#[test]
fn linker_arguments_expose_embedded_file_names() {
    assert_eq!(
        split_linker_argument("-Wl,--version-script=/x/v.map,-z,now"),
        ["--version-script=/x/v.map", "-z", "now"]
    );
    assert_eq!(path_like_values("--version-script=/x/v.map"), ["/x/v.map"]);
    assert_eq!(
        path_like_values("/DEF:C:\\x\\a.def"),
        ["/DEF:C:\\x\\a.def", "C:\\x\\a.def"]
    );
    assert!(path_like_values("-lz").is_empty());
}
