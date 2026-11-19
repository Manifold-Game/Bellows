//! Content digests for large compiler inputs, memoized by file identity.
//!
//! Final links and their validation rehash every transitive rlib. The
//! memo key covers the path, length, modification time and, on Unix, the
//! inode and status-change time, which a writer cannot preserve. Files that
//! changed within the last two seconds are never memoized, because a second
//! write in the same timestamp tick could keep the same length and mtime.
use anyhow::{Context, Result};
use bellows_core::{atomic_write_state, digest_bytes, digest_file, validate_content_key};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SMALL: u64 = 256 * 1024;
const RACY: Duration = Duration::from_secs(2);
pub const DIRECTORY: &str = "digests-v1";

pub struct Digests {
    root: Option<PathBuf>,
}

impl Digests {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: Some(state_dir.join(DIRECTORY)),
        }
    }

    #[cfg(test)]
    pub fn uncached() -> Self {
        Self { root: None }
    }

    pub fn file(&self, path: &Path) -> Result<String> {
        let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let Some(root) = &self.root else {
            return digest_file(path);
        };
        if metadata.len() < SMALL {
            return digest_file(path);
        }
        let key = identity(path, &metadata);
        let memo = root.join(&key[..2]).join(&key);
        if let Ok(cached) = fs::read_to_string(&memo)
            && validate_content_key(cached.trim()).is_ok()
        {
            return Ok(cached.trim().to_owned());
        }
        let digest = digest_file(path)?;
        let settled = SystemTime::now()
            .duration_since(changed(&metadata))
            .is_ok_and(|age| age > RACY);
        // After hashing, the file must still have the identity we keyed on.
        let unchanged = fs::metadata(path)
            .map(|after| identity(path, &after) == key)
            .unwrap_or(false);
        if settled && unchanged {
            let _ = atomic_write_state(&memo, digest.as_bytes());
        }
        Ok(digest)
    }

    /// Record the digest of a large file Bellows itself just wrote from bytes
    /// it verified, so downstream link validation in a fresh checkout does
    /// not rehash every restored rlib. The memo is keyed on the identity
    /// observed after the write; any later rewrite changes it.
    ///
    /// Unlike [`Digests::file`], this memoizes a file written moments ago, with
    /// no settle window, so a rewrite of the same length that keeps every
    /// identity field would be served this digest. Two things rule that out:
    /// - Nothing else writes the file within its timestamp granularity. Every
    ///   caller runs inside the wrapper process of the unit that owns the path,
    ///   while Cargo waits for that wrapper: Bellows wrote the file itself (a
    ///   restore) or its rustc child has exited and Bellows just read the bytes
    ///   (a capture). The next writer of the path is a later rustc for the same
    ///   unit, which starts only after this wrapper exits and Cargo schedules
    ///   the unit again, and writes its outputs at the end of a compilation:
    ///   far longer than a Windows clock tick (at most 15.6 ms), and with a
    ///   new change time and file id on Unix and Windows alike.
    /// - A filesystem whose timestamps are coarser than that (whole seconds:
    ///   FAT and exFAT, some FUSE, NFS and SMB mounts) could still keep every
    ///   field across a later rewrite within its granularity, and may report
    ///   reused inode numbers. On such a file this records nothing
    ///   ([`fine_grained`]), and `file` memoizes it once it has settled.
    pub fn remember(&self, path: &Path, digest: &str) {
        let Some(root) = &self.root else {
            return;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return;
        };
        if metadata.len() < SMALL
            || validate_content_key(digest).is_err()
            || !fine_grained(&metadata)
        {
            return;
        }
        let key = identity(path, &metadata);
        let _ = atomic_write_state(&root.join(&key[..2]).join(&key), digest.as_bytes());
    }
}

fn changed(metadata: &fs::Metadata) -> SystemTime {
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ctime = UNIX_EPOCH
            + Duration::new(metadata.ctime().max(0) as u64, metadata.ctime_nsec() as u32);
        modified.max(ctime)
    }
    #[cfg(not(unix))]
    modified
}

fn identity(path: &Path, metadata: &fs::Metadata) -> String {
    digest_bytes(identity_fields(path, metadata).as_bytes())
}

/// Whether `metadata`'s timestamps carry sub-second precision: a modification
/// time with a fractional second, and on Unix a change time with one too. A
/// filesystem that stamps whole seconds (FAT, exFAT, some network and FUSE
/// mounts) gives neither; a fine-grained one gives a whole second about once
/// in a billion writes, which only skips that memo.
fn fine_grained(metadata: &fs::Metadata) -> bool {
    let fractional = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|since| since.subsec_nanos() != 0);
    #[cfg(unix)]
    let fractional = fractional && {
        use std::os::unix::fs::MetadataExt;
        metadata.ctime_nsec() != 0
    };
    fractional
}

/// What a memo's key is made of: the path, the length, the modification and creation times, then on Unix the
/// device, inode and change time, and on Windows the volume and file id. A file replaced in place must change at
/// least one of them. Times alone don't: Windows stamps files at clock-tick granularity, so a same-length file
/// written within one tick of the file it replaces has its times, and a rename into a recently used name can keep
/// the old creation time (Bellows #49's Windows CI printed identical fields before and after a replacement; main
/// passed only while the memo write's fsyncs slowed the tests past a tick).
fn identity_fields(path: &Path, metadata: &fs::Metadata) -> String {
    let nanos = |time: std::io::Result<SystemTime>| {
        time.ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos())
    };
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut fields = format!(
        "v1\0{}\0{}\0{}\0{}",
        path.display(),
        metadata.len(),
        nanos(metadata.modified()),
        nanos(metadata.created()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fields.push_str(&format!(
            "\0{}\0{}\0{}\0{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ));
    }
    #[cfg(windows)]
    fields.push_str(&windows_file_id(path));
    fields
}

/// Remove memo entries not used for `max_age`; called by parent commands.
pub fn prune(state_dir: &Path, max_age: Duration) {
    let root = state_dir.join(DIRECTORY);
    let marker = root.join(".pruned");
    let recent = fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| SystemTime::now().duration_since(time).ok())
        .is_some_and(|age| age < Duration::from_secs(24 * 60 * 60));
    if recent {
        return;
    }
    let _ = fs::create_dir_all(&root);
    let _ = fs::write(&marker, b"");
    let Ok(shards) = fs::read_dir(&root) else {
        return;
    };
    for shard in shards.flatten() {
        let Ok(entries) = fs::read_dir(shard.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|time| SystemTime::now().duration_since(time).ok())
                .is_some_and(|age| age > max_age);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// `\0<volume serial>\0<128-bit file id>` of `path` (GetFileInformationByHandleEx, FileIdInfo): the Windows
/// equivalent of the device and inode, so a file replaced by rename is told apart from the one it replaced. A file
/// whose id can't be read gets a field no later read repeats, so its memo is never served.
#[cfg(windows)]
fn windows_file_id(path: &Path) -> String {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_ID_INFO_CLASS: i32 = 0x12;
    let id = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .open(path)
        .ok()
        .and_then(|file| {
            let mut info = FileIdInfo::default();
            // SAFETY: `file` is an open handle and `info` is a FILE_ID_INFO-sized
            // buffer that outlives the call.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle().cast(),
                    FILE_ID_INFO_CLASS,
                    (&mut info as *mut FileIdInfo).cast(),
                    std::mem::size_of::<FileIdInfo>() as u32,
                )
            };
            (ok != 0).then(|| {
                format!(
                    "\0{}\0{}",
                    info.volume_serial_number,
                    u128::from_le_bytes(info.file_id)
                )
            })
        });
    id.unwrap_or_else(|| {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        format!("\0unidentified\0{unique}")
    })
}

#[cfg(test)]
mod tests {
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
        let before = identity_fields(&file, &fs::metadata(&file).unwrap());
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
        let after = identity_fields(&file, &fs::metadata(&file).unwrap());
        assert_eq!(
            digests.file(&file).unwrap(),
            digest_bytes(&second),
            "the memo served the replaced file's digest; identity before {before:?}, after {after:?}"
        );
    }

    /// The memo files under `root`, at any depth.
    fn memo_count(root: &Path) -> usize {
        let Ok(entries) = fs::read_dir(root) else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() { memo_count(&path) } else { 1 }
            })
            .sum()
    }

    /// A file stamped in whole seconds, as a coarse filesystem stamps every
    /// file, is never remembered: a rewrite within that second could keep its
    /// identity. A finely stamped one is.
    #[test]
    fn remember_skips_a_file_stamped_in_whole_seconds() {
        let temp = tempfile::tempdir().unwrap();
        let memos = temp.path().join("memos");
        let digests = Digests::new(&memos);
        let bytes = vec![3u8; SMALL as usize + 1];
        let coarse = temp.path().join("coarse.rlib");
        fs::write(&coarse, &bytes).unwrap();
        let whole = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&coarse)
            .unwrap()
            .set_modified(whole)
            .unwrap();
        assert!(!fine_grained(&fs::metadata(&coarse).unwrap()));
        digests.remember(&coarse, &digest_bytes(&bytes));
        assert_eq!(
            memo_count(&memos),
            0,
            "a whole-second stamp is not remembered"
        );
        let fine = temp.path().join("fine.rlib");
        fs::write(&fine, &bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(&fine)
            .unwrap()
            .set_modified(whole + Duration::from_nanos(123_400))
            .unwrap();
        assert!(fine_grained(&fs::metadata(&fine).unwrap()));
        digests.remember(&fine, &digest_bytes(&bytes));
        assert_eq!(memo_count(&memos), 1, "a fine stamp is remembered");
        assert_eq!(digests.file(&fine).unwrap(), digest_bytes(&bytes));
    }

    /// A same-length file renamed over another within the same timestamp tick has a different identity, on every
    /// platform: nothing here waits, so on Windows only the file id tells the two apart.
    #[test]
    fn a_replacement_within_one_tick_has_another_identity() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("same-tick.rlib");
        fs::write(&file, [1u8; 64]).unwrap();
        let before = identity_fields(&file, &fs::metadata(&file).unwrap());
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, [2u8; 64]).unwrap();
        fs::rename(&replacement, &file).unwrap();
        let after = identity_fields(&file, &fs::metadata(&file).unwrap());
        assert_ne!(before, after, "a replaced file kept its identity");
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
        let before = identity_fields(&file, &fs::metadata(&file).unwrap());
        let other = vec![8u8; SMALL as usize + 1];
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, &other).unwrap();
        fs::rename(&replacement, &file).unwrap();
        let after = identity_fields(&file, &fs::metadata(&file).unwrap());
        assert_eq!(
            digests.file(&file).unwrap(),
            digest_bytes(&other),
            "the remembered digest outlived its file; identity before {before:?}, after {after:?}"
        );
    }
}
