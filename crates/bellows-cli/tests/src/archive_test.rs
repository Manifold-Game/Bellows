use super::*;

fn member(name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
        0,
        0,
        0,
        644,
        data.len()
    )
    .into_bytes();
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(b'\n');
    }
    out
}

#[test]
fn reads_gnu_long_names_and_skips_symbol_tables() {
    let mut archive = MAGIC.to_vec();
    archive.extend(member("/", b"symtab"));
    archive.extend(member("//", b"a_very_long_member_name.rcgu.o/\n"));
    archive.extend(member("lib.rmeta/", b"meta"));
    archive.extend(member("/0", b"obj"));
    archive.extend(member("v.o/", b"native"));
    assert_eq!(
        member_names(&archive).unwrap(),
        ["lib.rmeta", "a_very_long_member_name.rcgu.o", "v.o"]
    );
    assert!(member_names(b"not an archive").is_err());
}

#[test]
fn reads_bsd_names() {
    let mut archive = MAGIC.to_vec();
    archive.extend(member("#1/20", b"__.SYMDEF SORTED\0\0\0\0"));
    archive.extend(member("#1/12", b"long_name.oXdata"));
    assert_eq!(member_names(&archive).unwrap(), ["long_name.oX"]);
}
