//! Keeps rustc's incremental sessions coherent with the metadata Cargo reads.
//!
//! rustc saves a crate's metadata (`metadata.rmeta`) in its incremental
//! session. When a later compile finds the metadata unchanged, rustc
//! hard-links that saved file back to the output (`deps/lib….rmeta`) and the
//! output keeps the saved file's modification time. Cargo decides whether a
//! later build's dependents are fresh by comparing those times.
//!
//! A unit compiled in two sessions breaks that. Bellows compiles a miss in
//! its own per-unit session (`{crate}-bellows{extra}`) with remapped paths,
//! and every other compile of the unit (a plain `cargo` build, a Bellows
//! passthrough) uses Cargo's session without them: the two produce different
//! metadata. When the unit moves between sessions, or a restore replaces the
//! output, rustc can put back a session's older metadata under its older
//! time, and dependents built against the newer metadata stay "fresh"
//! against a crate hash that no longer exists (E0460, E0463).
//!
//! The rule kept here: **a session may hold only metadata that is the file
//! in the output directory now, or the same bytes.** A session that holds
//! anything else is removed, and rustc starts that unit from scratch.

use anyhow::{Context, Result};
use bellows_core::digest_bytes;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The file rustc saves in a session and hard-links to the output.
const SESSION_METADATA: &str = "metadata.rmeta";

/// Identifies a file across its hard links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some(FileId {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Hard-link identity is not portable; elsewhere files compare by content.
#[cfg(not(unix))]
fn file_id(_metadata: &fs::Metadata) -> Option<FileId> {
    None
}

/// Other names the file still has. Unknown (`None`) where the platform
/// does not say; callers then search without a bound.
#[cfg(unix)]
fn link_count(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.nlink())
}

#[cfg(not(unix))]
fn link_count(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

fn read_all(mut file: &File) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// One unit's metadata output and the incremental directories that can
/// hold copies of it.
pub struct Coherence {
    /// The unit's metadata output (`deps/lib{crate}{extra}.rmeta`).
    rmeta: PathBuf,
    /// Cargo's incremental directory for the profile (`-C incremental=`).
    incremental: PathBuf,
    crate_name: String,
    /// Bellows' own session directory for this unit, if it compiles here.
    unit_dir: Option<PathBuf>,
    /// The output as it was before this compile or restore, held open so its
    /// bytes and remaining links stay readable after rustc or a restore
    /// unlinks the path.
    before: Option<(File, fs::Metadata)>,
}

impl Coherence {
    /// Records the unit's metadata output before anything replaces it.
    pub fn record(
        rmeta: PathBuf,
        incremental: PathBuf,
        crate_name: &str,
        unit_dir: Option<PathBuf>,
    ) -> Self {
        let before = File::open(&rmeta)
            .ok()
            .and_then(|file| file.metadata().ok().map(|metadata| (file, metadata)));
        Self {
            rmeta,
            incremental,
            crate_name: crate_name.to_owned(),
            unit_dir,
            before,
        }
    }

    fn before_id(&self) -> Option<FileId> {
        self.before
            .as_ref()
            .and_then(|(_, metadata)| file_id(metadata))
    }

    fn before_digest(&self) -> Result<Option<String>> {
        self.before
            .as_ref()
            .map(|(file, _)| read_all(file).map(|bytes| digest_bytes(&bytes)))
            .transpose()
    }

    /// Before a compile in Bellows' unit session: removes the session when
    /// it holds metadata other than the current output, so rustc cannot put
    /// that back. Returns why it was removed.
    pub fn reset_unit_session_if_stale(&self) -> Result<Option<String>> {
        let Some(unit_dir) = &self.unit_dir else {
            return Ok(None);
        };
        let held = reused_metadata(&crate_dirs_in(unit_dir, &self.crate_name)?)?;
        if held.is_empty() {
            return Ok(None);
        }
        let before_id = self.before_id();
        let mut before_digest: Option<String> = None;
        for path in &held {
            let metadata = fs::metadata(path)?;
            if before_id.is_some() && file_id(&metadata) == before_id {
                continue;
            }
            if self.before.is_none() {
                return remove_unit_dir(
                    unit_dir,
                    "its session holds metadata but the output has none",
                );
            }
            if before_digest.is_none() {
                before_digest = Some(self.before_digest()?.unwrap_or_default());
            }
            if bellows_core::digest_file(path)? != before_digest.as_deref().unwrap_or_default() {
                return remove_unit_dir(
                    unit_dir,
                    "its session holds other metadata than the output Cargo reads",
                );
            }
        }
        Ok(None)
    }

    /// After a restore wrote metadata with digest `restored`: removes
    /// Bellows' unit session unless every metadata it holds has those bytes.
    pub fn reset_unit_session_unless(&self, restored: &str) -> Result<Option<String>> {
        let Some(unit_dir) = &self.unit_dir else {
            return Ok(None);
        };
        for path in reused_metadata(&crate_dirs_in(unit_dir, &self.crate_name)?)? {
            if bellows_core::digest_file(&path)? != restored {
                return remove_unit_dir(
                    unit_dir,
                    "a restore replaced the metadata its session holds",
                );
            }
        }
        Ok(None)
    }

    /// After the output was replaced (by a compile in any session, or a
    /// restore): removes every session that still holds the replaced file,
    /// unless the new output has the same bytes. Returns what was removed.
    pub fn release_replaced(&self) -> Result<Vec<PathBuf>> {
        let Some((file, metadata)) = &self.before else {
            return Ok(Vec::new());
        };
        let Some(before_id) = file_id(metadata) else {
            return Ok(Vec::new());
        };
        let current = fs::metadata(&self.rmeta).ok();
        let current_id = current.as_ref().and_then(file_id);
        if current_id == Some(before_id) {
            return Ok(Vec::new());
        }
        // The output's own name is gone; any link left is in a session.
        let remaining = link_count(&file.metadata()?).unwrap_or(u64::MAX);
        if remaining == 0 {
            return Ok(Vec::new());
        }
        if current.is_some() {
            let now = bellows_core::digest_file(&self.rmeta)?;
            if self.before_digest()?.as_deref() == Some(now.as_str()) {
                return Ok(Vec::new());
            }
        }
        let crate_dirs = all_crate_dirs(&self.incremental, &self.crate_name)?;
        let mut found = 0u64;
        let mut removed = Vec::new();
        for crate_dir in crate_dirs {
            if found >= remaining {
                break;
            }
            let latest = latest_finished_session(&crate_dir)?;
            let mut latest_is_stale = false;
            for session in sessions_in(&crate_dir)? {
                let Ok(metadata) = fs::metadata(session.join(SESSION_METADATA)) else {
                    continue;
                };
                if file_id(&metadata) == Some(before_id) {
                    found += 1;
                    latest_is_stale |= latest.as_ref() == Some(&session);
                }
            }
            // Only the latest finished session is reused; rustc collects
            // the older ones (and unfinished ones) itself. Removing the
            // whole directory keeps rustc from falling back to an even
            // older session.
            if latest_is_stale {
                fs::remove_dir_all(&crate_dir)
                    .with_context(|| format!("remove {}", crate_dir.display()))?;
                removed.push(crate_dir);
            }
        }
        Ok(removed)
    }

    /// After a successful compile: if rustc left the output with a time from
    /// before the compile and different bytes from what dependents last read,
    /// gives it the current time, so Cargo rebuilds those dependents.
    pub fn repair_mtime(&self, started: SystemTime) -> Result<bool> {
        let Ok(metadata) = fs::metadata(&self.rmeta) else {
            return Ok(false);
        };
        if metadata.modified()? >= started {
            return Ok(false);
        }
        if let Some(before_id) = self.before_id()
            && file_id(&metadata) == Some(before_id)
        {
            return Ok(false);
        }
        let now = bellows_core::digest_file(&self.rmeta)?;
        if self.before_digest()?.as_deref() == Some(now.as_str()) {
            return Ok(false);
        }
        File::options()
            .write(true)
            .open(&self.rmeta)
            .and_then(|file| file.set_modified(SystemTime::now()))
            .with_context(|| format!("set the time of {}", self.rmeta.display()))?;
        Ok(true)
    }
}

fn remove_unit_dir(unit_dir: &Path, reason: &str) -> Result<Option<String>> {
    fs::remove_dir_all(unit_dir).with_context(|| format!("remove {}", unit_dir.display()))?;
    Ok(Some(format!(
        "reset incremental session {}: {reason}",
        unit_dir.display()
    )))
}

fn entries(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in read {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            found.push((name.to_owned(), entry.path()));
        }
    }
    found.sort();
    Ok(found)
}

/// rustc's per-crate directories (`{crate}-{stable id}`) directly in `dir`.
fn crate_dirs_in(dir: &Path, crate_name: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{crate_name}-");
    let bellows = format!("{crate_name}-bellows");
    Ok(entries(dir)?
        .into_iter()
        .filter(|(name, _)| name.starts_with(&prefix) && !name.starts_with(&bellows))
        .map(|(_, path)| path)
        .collect())
}

/// Every per-crate directory for `crate_name`: Cargo's, directly in the
/// profile's incremental directory, and those inside each Bellows unit
/// directory. Bellows' first: the unit compiled most recently here is
/// usually the one holding a link.
fn all_crate_dirs(incremental: &Path, crate_name: &str) -> Result<Vec<PathBuf>> {
    let bellows = format!("{crate_name}-bellows");
    let mut dirs = Vec::new();
    for (name, path) in entries(incremental)? {
        if name.starts_with(&bellows) {
            dirs.extend(crate_dirs_in(&path, crate_name)?);
        }
    }
    dirs.extend(crate_dirs_in(incremental, crate_name)?);
    Ok(dirs)
}

/// Session directories in a per-crate directory, finished or not.
fn sessions_in(crate_dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(entries(crate_dir)?
        .into_iter()
        .filter(|(name, _)| name.starts_with("s-"))
        .map(|(_, path)| path)
        .collect())
}

/// The session rustc reuses from a per-crate directory: the finished one
/// with the latest timestamp (`s-{timestamp}-{random}-{svh}`, the timestamp
/// in base 36). rustc leaves the previous session beside it until its next
/// compile collects it.
fn latest_finished_session(crate_dir: &Path) -> Result<Option<PathBuf>> {
    let mut latest: Option<(u128, PathBuf)> = None;
    for session in sessions_in(crate_dir)? {
        let Some(name) = session.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with("-working") {
            continue;
        }
        let Some(stamp) = name
            .split('-')
            .nth(1)
            .and_then(|stamp| u128::from_str_radix(stamp, 36).ok())
        else {
            continue;
        };
        if latest.as_ref().is_none_or(|(best, _)| stamp > *best) {
            latest = Some((stamp, session));
        }
    }
    Ok(latest.map(|(_, session)| session))
}

/// The saved metadata rustc would reuse from each per-crate directory.
fn reused_metadata(crate_dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for crate_dir in crate_dirs {
        if let Some(session) = latest_finished_session(crate_dir)? {
            let metadata = session.join(SESSION_METADATA);
            if metadata.is_file() {
                found.push(metadata);
            }
        }
    }
    Ok(found)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Layout {
        _temp: tempfile::TempDir,
        deps: PathBuf,
        incremental: PathBuf,
        rmeta: PathBuf,
        unit_dir: PathBuf,
    }

    fn layout() -> Layout {
        let temp = tempfile::tempdir().unwrap();
        let deps = temp.path().join("deps");
        let incremental = temp.path().join("incremental");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(&incremental).unwrap();
        Layout {
            rmeta: deps.join("liba-1234.rmeta"),
            unit_dir: incremental.join("a-bellows-1234"),
            deps,
            incremental,
            _temp: temp,
        }
    }

    /// A finished session in `crate_dir` whose metadata is a hard link to
    /// `file` (as rustc saves it), or a copy of `bytes`.
    fn session(crate_dir: &Path, name: &str, link_to: Option<&Path>, bytes: &[u8]) -> PathBuf {
        let session = crate_dir.join(name);
        fs::create_dir_all(&session).unwrap();
        let metadata = session.join(SESSION_METADATA);
        match link_to {
            Some(file) => fs::hard_link(file, &metadata).unwrap(),
            None => fs::write(&metadata, bytes).unwrap(),
        }
        fs::write(session.join("dep-graph.bin"), b"graph").unwrap();
        session
    }

    fn coherence(l: &Layout) -> Coherence {
        Coherence::record(
            l.rmeta.clone(),
            l.incremental.clone(),
            "a",
            Some(l.unit_dir.clone()),
        )
    }

    /// Replaces the output the way rustc and restores do: a new file.
    fn replace(path: &Path, bytes: &[u8]) {
        let _ = fs::remove_file(path);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_unit_session_holding_the_output_is_kept() {
        let l = layout();
        fs::write(&l.rmeta, b"one").unwrap();
        session(&l.unit_dir.join("a-x1"), "s-1-a-svh", Some(&l.rmeta), b"");
        assert_eq!(coherence(&l).reset_unit_session_if_stale().unwrap(), None);
        assert!(l.unit_dir.exists());
    }

    #[test]
    fn a_unit_session_holding_the_same_bytes_is_kept() {
        let l = layout();
        fs::write(&l.rmeta, b"one").unwrap();
        session(&l.unit_dir.join("a-x1"), "s-1-a-svh", None, b"one");
        assert_eq!(coherence(&l).reset_unit_session_if_stale().unwrap(), None);
        assert!(l.unit_dir.exists());
    }

    #[test]
    fn a_unit_session_holding_other_metadata_is_reset() {
        let l = layout();
        fs::write(&l.rmeta, b"two").unwrap();
        session(&l.unit_dir.join("a-x1"), "s-1-a-svh", None, b"one");
        let reason = coherence(&l).reset_unit_session_if_stale().unwrap();
        assert!(reason.unwrap().contains("other metadata"));
        assert!(!l.unit_dir.exists());
    }

    #[test]
    fn a_unit_session_with_metadata_but_no_output_is_reset() {
        let l = layout();
        session(&l.unit_dir.join("a-x1"), "s-1-a-svh", None, b"one");
        assert!(
            coherence(&l)
                .reset_unit_session_if_stale()
                .unwrap()
                .is_some()
        );
        assert!(!l.unit_dir.exists());
    }

    #[test]
    fn unfinished_sessions_and_sessions_without_metadata_are_not_judged() {
        let l = layout();
        fs::write(&l.rmeta, b"two").unwrap();
        // A compile that was cut off: rustc never reuses it.
        session(&l.unit_dir.join("a-x1"), "s-1-a-working", None, b"one");
        // A check build that saved no metadata.
        fs::create_dir_all(l.unit_dir.join("a-x1/s-2-b-svh")).unwrap();
        assert_eq!(coherence(&l).reset_unit_session_if_stale().unwrap(), None);
    }

    #[test]
    fn a_restore_resets_a_unit_session_unless_it_holds_the_restored_bytes() {
        let l = layout();
        session(&l.unit_dir.join("a-x1"), "s-1-a-svh", None, b"one");
        let c = coherence(&l);
        assert_eq!(
            c.reset_unit_session_unless(&digest_bytes(b"one")).unwrap(),
            None
        );
        assert!(
            c.reset_unit_session_unless(&digest_bytes(b"two"))
                .unwrap()
                .is_some()
        );
        assert!(!l.unit_dir.exists());
    }

    #[test]
    fn replacing_the_output_removes_cargos_session_holding_the_old_file() {
        let l = layout();
        fs::write(&l.rmeta, b"cargo").unwrap();
        let cargo_dir = l.incremental.join("a-x1");
        session(&cargo_dir, "s-1-a-svh", Some(&l.rmeta), b"");
        // Another unit of the same crate, and another crate: untouched.
        let other_unit = l.incremental.join("a-x2");
        session(&other_unit, "s-1-b-svh", None, b"cargo");
        let other_crate = l.incremental.join("ab-x1");
        session(&other_crate, "s-1-c-svh", None, b"cargo");
        let c = coherence(&l);
        replace(&l.rmeta, b"bellows");
        let removed = c.release_replaced().unwrap();
        assert_eq!(removed, std::slice::from_ref(&cargo_dir));
        assert!(!cargo_dir.exists());
        assert!(other_unit.exists() && other_crate.exists());
    }

    #[test]
    fn an_older_session_beside_the_current_one_is_left_to_rustc() {
        let l = layout();
        fs::write(&l.rmeta, b"old").unwrap();
        let crate_dir = l.unit_dir.join("a-x1");
        let old = session(&crate_dir, "s-1-a-svh", Some(&l.rmeta), b"");
        let c = coherence(&l);
        replace(&l.rmeta, b"new");
        // This compile's session, the one rustc reuses next.
        let new = session(&crate_dir, "s-2-b-svh", Some(&l.rmeta), b"");
        assert!(c.release_replaced().unwrap().is_empty());
        assert!(old.exists() && new.exists());
        // Nor does the next compile reset the unit's session for it.
        assert_eq!(coherence(&l).reset_unit_session_if_stale().unwrap(), None);
    }

    #[test]
    fn the_latest_session_is_judged_by_its_timestamp() {
        let l = layout();
        fs::write(&l.rmeta, b"current").unwrap();
        let crate_dir = l.unit_dir.join("a-x1");
        // `z` is later than `10` in base 36 only by length: 35 < 36.
        session(&crate_dir, "s-z-a-svh", None, b"older");
        session(&crate_dir, "s-10-b-svh", Some(&l.rmeta), b"");
        session(&crate_dir, "s-11-c-working", None, b"cut off");
        assert_eq!(coherence(&l).reset_unit_session_if_stale().unwrap(), None);
    }

    #[test]
    fn replacing_the_output_with_the_same_bytes_removes_nothing() {
        let l = layout();
        fs::write(&l.rmeta, b"same").unwrap();
        let cargo_dir = l.incremental.join("a-x1");
        session(&cargo_dir, "s-1-a-svh", Some(&l.rmeta), b"");
        let c = coherence(&l);
        replace(&l.rmeta, b"same");
        assert!(c.release_replaced().unwrap().is_empty());
        assert!(cargo_dir.exists());
    }

    #[test]
    fn a_file_with_no_other_links_needs_no_search() {
        let l = layout();
        fs::write(&l.rmeta, b"alone").unwrap();
        // Holds the old bytes as a copy, not a link: not this file.
        let cargo_dir = l.incremental.join("a-x1");
        session(&cargo_dir, "s-1-a-svh", None, b"alone");
        let c = coherence(&l);
        replace(&l.rmeta, b"next");
        assert!(c.release_replaced().unwrap().is_empty());
        assert!(cargo_dir.exists());
    }

    #[test]
    fn the_search_also_finds_bellows_unit_sessions() {
        let l = layout();
        fs::write(&l.rmeta, b"bellows").unwrap();
        let inner = l.unit_dir.join("a-x1");
        session(&inner, "s-1-a-svh", Some(&l.rmeta), b"");
        let c = coherence(&l);
        replace(&l.rmeta, b"cargo");
        assert_eq!(c.release_replaced().unwrap(), std::slice::from_ref(&inner));
        assert!(!inner.exists());
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn an_old_time_on_different_bytes_is_repaired() {
        let l = layout();
        fs::write(&l.rmeta, b"newer").unwrap();
        let c = coherence(&l);
        let started = SystemTime::now();
        // rustc links in a session's older metadata under its old time.
        let saved = l.deps.join("saved.rmeta");
        fs::write(&saved, b"older").unwrap();
        set_mtime(&saved, started - Duration::from_secs(3600));
        fs::remove_file(&l.rmeta).unwrap();
        fs::hard_link(&saved, &l.rmeta).unwrap();
        assert!(c.repair_mtime(started).unwrap());
        assert!(fs::metadata(&l.rmeta).unwrap().modified().unwrap() >= started);
    }

    #[test]
    fn an_old_time_on_unchanged_bytes_is_kept() {
        let l = layout();
        fs::write(&l.rmeta, b"same").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        set_mtime(&l.rmeta, old);
        let c = coherence(&l);
        let started = SystemTime::now();
        // The same file kept, or a different file with the same bytes.
        assert!(!c.repair_mtime(started).unwrap());
        replace(&l.rmeta, b"same");
        set_mtime(&l.rmeta, old);
        assert!(!c.repair_mtime(started).unwrap());
        assert_eq!(fs::metadata(&l.rmeta).unwrap().modified().unwrap(), old);
    }
}
