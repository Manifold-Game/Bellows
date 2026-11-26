use super::*;

#[test]
fn memo_follows_file_identity_not_path() {
    let temp = tempfile::tempdir().unwrap();
    let digests = Digests::new(temp.path());
    let file = temp.path().join("large.rlib");
    let first = vec![1u8; SMALL as usize + 1];
    fs::write(&file, &first).unwrap();
    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(digests.file(&file).unwrap(), digest_bytes(&first));
    // Same length, restored mtime: inode/ctime (or creation time) differ.
    let second = vec![2u8; SMALL as usize + 1];
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, &second).unwrap();
    fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(old)
        .unwrap();
    fs::rename(&replacement, &file).unwrap();
    assert_eq!(digests.file(&file).unwrap(), digest_bytes(&second));
}

#[test]
fn remembered_digests_apply_only_to_the_written_file() {
    let temp = tempfile::tempdir().unwrap();
    let digests = Digests::new(temp.path());
    let file = temp.path().join("restored.rlib");
    let bytes = vec![7u8; SMALL as usize + 1];
    fs::write(&file, &bytes).unwrap();
    digests.remember(&file, &digest_bytes(&bytes));
    assert_eq!(digests.file(&file).unwrap(), digest_bytes(&bytes));
    let other = vec![8u8; SMALL as usize + 1];
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, &other).unwrap();
    fs::rename(&replacement, &file).unwrap();
    assert_eq!(digests.file(&file).unwrap(), digest_bytes(&other));
}
