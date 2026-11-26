use super::*;

#[test]
fn finds_embedded_roots_and_ignores_remapped_paths() {
    let scanner = Scanner::new(&[
        "/home/dev/checkout-a".into(),
        "/home/dev/checkout-a/target".into(),
    ]);
    assert!(scanner.leaks(b"\0\x01/home/dev/checkout-a/crates/x/fixtures\0"));
    assert!(!scanner.leaks(b"/bellows/workspace/crates/x/src/lib.rs"));
    assert!(!scanner.leaks(b"/home/dev/checkout-b/crates/x"));
    assert!(Scanner::new(&["/w".into()]).leaks(b"anything"));
}

#[test]
fn program_databases_may_name_linker_modules_but_not_sources() {
    let root = if cfg!(windows) {
        r"C:\work\checkout-a"
    } else {
        "/work/checkout-a"
    };
    let scanner = Scanner::new(&[root.into()]);
    let sep = std::path::MAIN_SEPARATOR;
    let record = |text: String| [b"\0\x02".as_slice(), text.as_bytes(), b"\0\x01"].concat();
    for text in [
        root.to_owned(),
        format!("{root}{sep}target{sep}deps{sep}m.m.abc-cgu.0.rcgu.o"),
        format!("{root}{sep}target{sep}deps{sep}libdemo-abc.rlib"),
        format!("{root}{sep}target{sep}deps{sep}m.pdb"),
        format!(" /NOLOGO /OUT:{root}{sep}target{sep}m.exe /DEBUG"),
    ] {
        assert!(
            !scanner.leaks_in_program_database(&record(text.clone())),
            "{text}"
        );
        assert!(scanner.leaks(&record(text)));
    }
    let source = format!("{root}{sep}src{sep}lib.rs");
    assert!(scanner.leaks_in_program_database(&record(source)));
    let fixture = format!("{root}{sep}fixtures");
    assert!(scanner.leaks_in_program_database(&record(fixture)));
}
