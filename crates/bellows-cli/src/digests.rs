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
    pub fn remember(&self, path: &Path, digest: &str) {
        let Some(root) = &self.root else {
            return;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return;
        };
        if metadata.len() < SMALL || validate_content_key(digest).is_err() {
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
    digest_bytes(fields.as_bytes())
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
}
