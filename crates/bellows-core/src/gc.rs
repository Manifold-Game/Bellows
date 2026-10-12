//! Size-bounded, least-recently-used collection of a store.
//!
//! One read-only pass snapshots every record and blob, counts references and
//! plans evictions in order of last use: the later of a candidate's
//! publication and its newest hit in the access journal. A dry run stops
//! there and reports the plan. A real run then applies it in small batches,
//! each under the store's mutation lock, so publications interleave with the
//! collection instead of waiting for all of it.
//!
//! A blob is deleted only when no record references it. Three things protect
//! blobs that the snapshot cannot see being referenced:
//! - a blob written or re-offered since the snapshot began (uploads and
//!   existence checks refresh its modification time);
//! - a blob that was unreferenced at the snapshot and is younger than the
//!   upload grace period, because its record may still be on the way;
//! - a blob referenced by any record published or used after the snapshot
//!   began, found by re-reading the journal under the lock before each batch.
use crate::{
    ArchiveManifest, CandidateIndex, DeclaredActionRecord, GcBucket, GcReport, PIN_PREFIX, Store,
    UNPUBLISHED_BLOB_GRACE, atomic_write, collect_paths, now_ms, validate_content_key,
};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const JOURNAL_DIR: &str = "access";
const JOURNAL_FILE: &str = "journal.log";
const BATCH: usize = 256;
const TOP_CRATES: usize = 15;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const COARSE_CLOCK_SLACK_MS: u64 = 50;

#[derive(Clone, Debug, Default)]
pub struct GcOptions {
    /// Keep the blob directory at or below this many bytes.
    pub max_bytes: u64,
    /// Plan and report without changing the store.
    pub dry_run: bool,
    /// Evict every compiler candidate older than this protocol, whatever the
    /// budget. Such candidates are unreachable from current clients.
    pub min_protocol: Option<u32>,
    /// A separate budget for final executables (binaries, test harnesses,
    /// examples). The least recently used are evicted until the bytes only
    /// they reference fit, before the overall budget is applied to every
    /// record. Libraries, procedural macros and build scripts are never
    /// evicted to make room for executables, which matters when one build
    /// links far more than it compiles: by recency alone its newest outputs,
    /// the links, would push out the libraries the next build needs most.
    pub max_executable_bytes: Option<u64>,
}

/// Runs each batch of mutations; a server wraps them in its own write lock.
pub type BatchRunner<'a> = &'a dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()>;

fn unserialized(batch: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    batch()
}

impl Store {
    /// Records that a compiler candidate was published or reused. The GC
    /// treats the newest such entry as the candidate's last use.
    pub fn record_use(&self, static_key: &str, action_key: &str) -> Result<()> {
        validate_content_key(static_key)?;
        validate_content_key(action_key)?;
        let directory = self.root.join(JOURNAL_DIR);
        fs::create_dir_all(&directory)?;
        // One short append per line: concurrent writers never interleave
        // within a line, and a torn final line is skipped when read.
        let line = format!("{static_key} {action_key} {}\n", now_ms());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(JOURNAL_FILE))
            .context("open access journal")?;
        file.write_all(line.as_bytes())
            .context("append access journal")
    }

    /// Marks an existing blob as just offered, so a collection running now
    /// keeps it for the record about to reference it.
    pub fn touch_blob(&self, digest: &str) -> Result<()> {
        let path = self.blob_path(digest)?;
        let file = fs::OpenOptions::new().append(true).open(&path)?;
        file.set_modified(SystemTime::now())?;
        Ok(())
    }

    pub fn gc(&self, max_bytes: u64) -> Result<GcReport> {
        self.collect_locally(&GcOptions {
            max_bytes,
            ..GcOptions::default()
        })
    }

    /// Collects a store no server is serving; batches need no outer lock.
    pub fn collect_locally(&self, options: &GcOptions) -> Result<GcReport> {
        self.collect(options, &unserialized)
    }

    pub fn collect(&self, options: &GcOptions, serialize: BatchRunner<'_>) -> Result<GcReport> {
        let started = now_ms();
        let started_time = SystemTime::now();
        // Rotate before reading so every use from here on lands in a fresh
        // journal that each batch re-reads under the lock.
        if !options.dry_run {
            rotate_journal(&self.root)?;
        }
        let snapshot = Snapshot::read(self, started_time)?;
        let plan = Plan::new(&snapshot, options, started);
        let mut report = plan.report(&snapshot, options);
        if !options.dry_run {
            plan.apply(self, &snapshot, serialize, &mut report)?;
            compact_journal(&self.root, &snapshot, &plan)?;
            report.bytes_after = crate::directory_bytes(&self.root.join("blobs"))?;
        }
        report.duration_ms = now_ms().saturating_sub(started);
        Ok(report)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Library,
    /// A binary, test harness or example: a final link nothing else consumes.
    Executable,
    /// Procedural macros, dylibs and build-script programs: linked, but inputs
    /// to other compilations.
    Linked,
    BuildScript,
    Declared,
    Archive,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Library => "library",
            Kind::Executable => "executable",
            Kind::Linked => "linked output",
            Kind::BuildScript => "build-script run",
            Kind::Declared => "declared action",
            Kind::Archive => "archive",
        }
    }
}

/// One evictable unit: a compiler candidate, or a whole declared/archive record.
struct Unit {
    path: PathBuf,
    /// For candidates: (action key, creation time) identifies it in its index.
    candidate: Option<(String, u64)>,
    static_key: Option<String>,
    protocol: Option<u32>,
    crate_name: String,
    kind: Kind,
    pinned: bool,
    last_used_ms: u64,
    blobs: Vec<u32>,
}

struct Blob {
    digest: String,
    bytes: u64,
    modified_ms: u64,
}

struct Snapshot {
    units: Vec<Unit>,
    blobs: Vec<Blob>,
    /// Blob index -> units referencing it.
    referrers: Vec<Vec<u32>>,
    journal_entries: u64,
    total_bytes: u64,
    started_ms: u64,
}

impl Snapshot {
    fn read(store: &Store, started: SystemTime) -> Result<Self> {
        let started_ms = millis(started);
        let journal = read_journal(&store.root)?;
        let mut blob_files = Vec::new();
        collect_paths(&store.root.join("blobs"), &mut blob_files)?;
        let mut blobs = Vec::with_capacity(blob_files.len());
        let mut by_digest = HashMap::with_capacity(blob_files.len());
        let mut total_bytes = 0u64;
        for path in blob_files {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            total_bytes = total_bytes.saturating_add(metadata.len());
            by_digest.insert(name.to_owned(), blobs.len() as u32);
            blobs.push(Blob {
                digest: name.to_owned(),
                bytes: metadata.len(),
                modified_ms: metadata.modified().map(millis).unwrap_or(0),
            });
        }
        let mut units = Vec::new();
        let mut referrers = vec![Vec::new(); blobs.len()];
        let mut add = |mut unit: Unit, digests: Vec<String>| {
            let index = units.len() as u32;
            let mut seen = BTreeSet::new();
            for digest in digests {
                if let Some(&blob) = by_digest.get(&digest)
                    && seen.insert(blob)
                {
                    unit.blobs.push(blob);
                    referrers[blob as usize].push(index);
                }
            }
            units.push(unit);
        };

        let mut files = Vec::new();
        collect_paths(&store.root.join("actions"), &mut files)?;
        for (path, index) in read_parallel::<CandidateIndex>(files, "action")? {
            for candidate in index.candidates {
                let used = journal
                    .get(&(candidate.static_key.clone(), candidate.action_key.clone()))
                    .copied()
                    .unwrap_or(0);
                let kind = candidate_kind(&candidate.crate_name, &candidate.artifacts);
                let mut digests = candidate
                    .artifacts
                    .into_iter()
                    .map(|artifact| artifact.digest)
                    .collect::<Vec<_>>();
                digests.push(candidate.stdout.digest);
                digests.push(candidate.stderr.digest);
                add(
                    Unit {
                        path: path.clone(),
                        candidate: Some((candidate.action_key, candidate.created_ms)),
                        static_key: Some(candidate.static_key),
                        protocol: Some(candidate.protocol),
                        crate_name: candidate.crate_name,
                        kind,
                        pinned: candidate
                            .env
                            .iter()
                            .any(|input| input.name.starts_with(PIN_PREFIX)),
                        last_used_ms: used.max(candidate.created_ms),
                        blobs: Vec::new(),
                    },
                    digests,
                );
            }
        }
        files = Vec::new();
        collect_paths(&store.root.join("declared"), &mut files)?;
        for (path, record) in read_parallel::<DeclaredActionRecord>(files, "declared")? {
            let modified = modified_ms(&path);
            let mut digests = record
                .inputs
                .into_iter()
                .chain(record.outputs)
                .map(|artifact| artifact.digest)
                .collect::<Vec<_>>();
            digests.push(record.stdout.digest);
            digests.push(record.stderr.digest);
            add(
                Unit {
                    path,
                    candidate: None,
                    static_key: None,
                    protocol: None,
                    crate_name: record.name,
                    kind: Kind::Declared,
                    pinned: false,
                    last_used_ms: modified,
                    blobs: Vec::new(),
                },
                digests,
            );
        }
        files = Vec::new();
        collect_paths(&store.root.join("archives"), &mut files)?;
        for (path, manifest) in read_parallel::<ArchiveManifest>(files, "archive")? {
            let modified = modified_ms(&path);
            add(
                Unit {
                    path,
                    candidate: None,
                    static_key: None,
                    protocol: Some(manifest.protocol),
                    crate_name: manifest.name,
                    kind: Kind::Archive,
                    pinned: false,
                    last_used_ms: modified.max(manifest.created_ms),
                    blobs: Vec::new(),
                },
                manifest
                    .files
                    .into_iter()
                    .map(|artifact| artifact.digest)
                    .collect(),
            );
        }
        Ok(Self {
            units,
            blobs,
            referrers,
            journal_entries: journal.len() as u64,
            total_bytes,
            started_ms,
        })
    }
}

struct Plan {
    evicted: Vec<bool>,
    /// Blobs to delete, each attributed to the unit whose eviction freed it.
    freed: Vec<(u32, u32)>,
    unreferenced: Vec<u32>,
    protected_unreferenced: u64,
    bytes_after: u64,
}

impl Plan {
    fn new(snapshot: &Snapshot, options: &GcOptions, now: u64) -> Self {
        let grace = UNPUBLISHED_BLOB_GRACE.as_millis() as u64;
        let mut unreferenced = Vec::new();
        let mut protected_unreferenced = 0;
        let mut bytes_after = snapshot.total_bytes;
        for (index, blob) in snapshot.blobs.iter().enumerate() {
            if !snapshot.referrers[index].is_empty() {
                continue;
            }
            if now.saturating_sub(blob.modified_ms) < grace {
                protected_unreferenced += 1;
            } else {
                unreferenced.push(index as u32);
                bytes_after = bytes_after.saturating_sub(blob.bytes);
            }
        }
        let mut order = (0..snapshot.units.len() as u32).collect::<Vec<_>>();
        let obsolete = |unit: &Unit| {
            unit.candidate.is_some()
                && options
                    .min_protocol
                    .zip(unit.protocol)
                    .is_some_and(|(minimum, protocol)| protocol < minimum)
        };
        order.sort_by_key(|&index| {
            let unit = &snapshot.units[index as usize];
            (!obsolete(unit), unit.last_used_ms, index)
        });
        let mut remaining = snapshot
            .referrers
            .iter()
            .map(|referrers| referrers.len() as u32)
            .collect::<Vec<_>>();
        let mut evicted = vec![false; snapshot.units.len()];
        let mut freed = Vec::new();
        let mut evict = |index: u32, bytes_after: &mut u64, executable_bytes: &mut u64| {
            if evicted[index as usize] {
                return;
            }
            let unit = &snapshot.units[index as usize];
            evicted[index as usize] = true;
            for &blob in &unit.blobs {
                remaining[blob as usize] -= 1;
                if remaining[blob as usize] == 0 {
                    freed.push((blob, index));
                    let bytes = snapshot.blobs[blob as usize].bytes;
                    *bytes_after = bytes_after.saturating_sub(bytes);
                    if unit.kind == Kind::Executable {
                        *executable_bytes = executable_bytes.saturating_sub(bytes);
                    }
                }
            }
        };
        // Bytes that only executables reference: what evicting all of them
        // would free.
        let mut executable_bytes = snapshot
            .referrers
            .iter()
            .enumerate()
            .filter(|(_, referrers)| {
                !referrers.is_empty()
                    && referrers
                        .iter()
                        .all(|&unit| snapshot.units[unit as usize].kind == Kind::Executable)
            })
            .map(|(blob, _)| snapshot.blobs[blob].bytes)
            .sum::<u64>();
        if let Some(budget) = options.max_executable_bytes {
            for &index in &order {
                if executable_bytes <= budget {
                    break;
                }
                if snapshot.units[index as usize].kind == Kind::Executable {
                    evict(index, &mut bytes_after, &mut executable_bytes);
                }
            }
        }
        for index in order {
            let unit = &snapshot.units[index as usize];
            if bytes_after <= options.max_bytes && !obsolete(unit) {
                break;
            }
            evict(index, &mut bytes_after, &mut executable_bytes);
        }
        Self {
            evicted,
            freed,
            unreferenced,
            protected_unreferenced,
            bytes_after,
        }
    }

    fn report(&self, snapshot: &Snapshot, options: &GcOptions) -> GcReport {
        let units = &snapshot.units;
        let evicted_count = self.evicted.iter().filter(|evicted| **evicted).count() as u64;
        let candidates = units.iter().filter(|unit| unit.candidate.is_some());
        let unreferenced_bytes: u64 = self
            .unreferenced
            .iter()
            .map(|&blob| snapshot.blobs[blob as usize].bytes)
            .sum();
        let freed_bytes: u64 = self
            .freed
            .iter()
            .map(|&(blob, _)| snapshot.blobs[blob as usize].bytes)
            .sum();
        let oldest_kept = units
            .iter()
            .zip(&self.evicted)
            .filter(|(_, evicted)| !**evicted)
            .map(|(unit, _)| unit.last_used_ms)
            .min();
        let newest_evicted = units
            .iter()
            .zip(&self.evicted)
            .filter(|(_, evicted)| **evicted)
            .map(|(unit, _)| unit.last_used_ms)
            .max();
        let now = snapshot.started_ms;
        let age = |unit: &Unit| {
            let days = now.saturating_sub(unit.last_used_ms) / DAY_MS;
            match days {
                0 => "used within 1 day",
                1..=2 => "used 1-3 days ago",
                3..=6 => "used 3-7 days ago",
                _ => "used over 7 days ago",
            }
            .to_owned()
        };
        let mut breakdown = Vec::new();
        breakdown.extend(
            self.buckets(snapshot, "protocol", |unit| match unit.protocol {
                Some(protocol) => format!("protocol {protocol}"),
                None => "unversioned".into(),
            }),
        );
        breakdown.extend(self.buckets(snapshot, "kind", |unit| unit.kind.label().into()));
        breakdown.extend(self.buckets(snapshot, "scope", |unit| {
            if unit.pinned {
                "pinned to a checkout".into()
            } else {
                "shareable".into()
            }
        }));
        breakdown.extend(self.buckets(snapshot, "last use", age));
        let mut crates = self.buckets(snapshot, "crate", |unit| unit.crate_name.clone());
        crates.sort_by(|a, b| {
            (b.evicted_bytes, b.exclusive_bytes).cmp(&(a.evicted_bytes, a.exclusive_bytes))
        });
        crates.truncate(TOP_CRATES);
        breakdown.extend(crates);
        GcReport {
            bytes_before: snapshot.total_bytes,
            bytes_after: self.bytes_after,
            records_evicted: evicted_count,
            blobs_evicted: (self.freed.len() + self.unreferenced.len()) as u64,
            dry_run: options.dry_run,
            max_bytes: options.max_bytes,
            min_protocol: options.min_protocol,
            max_executable_bytes: options.max_executable_bytes,
            records: units.len() as u64,
            candidates: candidates.count() as u64,
            blobs: snapshot.blobs.len() as u64,
            evicted_bytes: freed_bytes,
            unreferenced_blobs: self.unreferenced.len() as u64,
            unreferenced_bytes,
            protected_blobs: self.protected_unreferenced,
            journal_entries: snapshot.journal_entries,
            oldest_kept_ms: oldest_kept,
            newest_evicted_ms: newest_evicted,
            duration_ms: 0,
            skipped_blobs: 0,
            breakdown,
        }
    }

    /// Per-label totals for one grouping. `exclusive_bytes` counts blobs whose
    /// every referrer carries the label: what removing the label would free.
    fn buckets(
        &self,
        snapshot: &Snapshot,
        group: &str,
        label: impl Fn(&Unit) -> String,
    ) -> Vec<GcBucket> {
        let labels = snapshot.units.iter().map(&label).collect::<Vec<_>>();
        let mut buckets: BTreeMap<String, GcBucket> = BTreeMap::new();
        for (index, name) in labels.iter().enumerate() {
            let entry = buckets.entry(name.clone()).or_insert_with(|| GcBucket {
                group: group.to_owned(),
                label: name.clone(),
                ..GcBucket::default()
            });
            entry.records += 1;
            if self.evicted[index] {
                entry.evicted_records += 1;
            }
        }
        for (blob, referrers) in snapshot.referrers.iter().enumerate() {
            let Some(first) = referrers.first() else {
                continue;
            };
            let name = &labels[*first as usize];
            if referrers
                .iter()
                .all(|referrer| labels[*referrer as usize] == *name)
                && let Some(entry) = buckets.get_mut(name)
            {
                entry.exclusive_bytes += snapshot.blobs[blob].bytes;
            }
        }
        for &(blob, unit) in &self.freed {
            if let Some(entry) = buckets.get_mut(&labels[unit as usize]) {
                entry.evicted_bytes += snapshot.blobs[blob as usize].bytes;
            }
        }
        buckets.into_values().collect()
    }

    fn apply(
        &self,
        store: &Store,
        snapshot: &Snapshot,
        serialize: BatchRunner<'_>,
        report: &mut GcReport,
    ) -> Result<()> {
        let mut by_path: BTreeMap<&Path, Vec<&Unit>> = BTreeMap::new();
        for (unit, evicted) in snapshot.units.iter().zip(&self.evicted) {
            if *evicted {
                by_path.entry(unit.path.as_path()).or_default().push(unit);
            }
        }
        let paths = by_path.into_iter().collect::<Vec<_>>();
        let mut records_evicted = 0u64;
        let mut recent = Recent::default();
        for batch in paths.chunks(BATCH) {
            serialize(&mut || {
                store.with_mutation_lock(|| {
                    recent.refresh(store)?;
                    for (path, units) in batch {
                        records_evicted += evict_from(path, units, &recent.uses)?;
                    }
                    Ok(())
                })
            })?;
        }
        let blobs = self
            .freed
            .iter()
            .map(|&(blob, _)| blob)
            .chain(self.unreferenced.iter().copied())
            .collect::<Vec<_>>();
        let mut deleted = 0u64;
        let mut skipped = 0u64;
        for batch in blobs.chunks(BATCH) {
            serialize(&mut || {
                store.with_mutation_lock(|| {
                    recent.refresh(store)?;
                    for &blob in batch {
                        let blob = &snapshot.blobs[blob as usize];
                        let path = store.blob_path(&blob.digest)?;
                        let modified = match fs::metadata(&path) {
                            Ok(metadata) => metadata.modified().map(millis).unwrap_or(u64::MAX),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error.into()),
                        };
                        // File times come from the kernel's coarse clock,
                        // which trails the snapshot's clock by at most a tick.
                        if modified.saturating_add(COARSE_CLOCK_SLACK_MS) >= snapshot.started_ms
                            || recent.blobs.contains(&blob.digest)
                        {
                            skipped += 1;
                            continue;
                        }
                        match fs::remove_file(&path) {
                            Ok(()) => deleted += 1,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    Ok(())
                })
            })?;
        }
        report.records_evicted = records_evicted;
        report.blobs_evicted = deleted;
        report.skipped_blobs = skipped;
        Ok(())
    }
}

/// Removes the planned units from one record file, unless they were used or
/// replaced since the snapshot. Returns how many units were removed.
fn evict_from(path: &Path, units: &[&Unit], recent: &HashSet<(String, String)>) -> Result<u64> {
    if units.iter().all(|unit| unit.candidate.is_none()) {
        return Ok(match fs::remove_file(path) {
            Ok(()) => 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        });
    }
    // Decoded directly: validation would quarantine an older protocol's
    // index instead of letting it be evicted.
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let Ok(mut index) = serde_json::from_slice::<CandidateIndex>(&bytes) else {
        // Rewritten into something undecodable since the snapshot: leave it.
        return Ok(0);
    };
    let before = index.candidates.len();
    index.candidates.retain(|candidate| {
        !units.iter().any(|unit| {
            unit.candidate
                .as_ref()
                .is_some_and(|(action_key, created_ms)| {
                    *action_key == candidate.action_key
                        && *created_ms == candidate.created_ms
                        && !recent.contains(&(candidate.static_key.clone(), action_key.clone()))
                })
        })
    });
    let removed = (before - index.candidates.len()) as u64;
    if removed == 0 {
        return Ok(0);
    }
    if index.candidates.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        atomic_write(path, &serde_json::to_vec_pretty(&index)?)?;
    }
    Ok(removed)
}

/// What has been published or reused since the journal was rotated, read
/// incrementally: each refresh parses only lines appended since the last one
/// and re-reads only the indexes they name.
#[derive(Default)]
struct Recent {
    offset: u64,
    uses: HashSet<(String, String)>,
    blobs: HashSet<String>,
}

impl Recent {
    fn refresh(&mut self, store: &Store) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let path = store.root.join(JOURNAL_DIR).join(JOURNAL_FILE);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        // Only whole lines; a line still being appended is read next time.
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(());
        };
        self.offset += end as u64 + 1;
        let mut keys = BTreeSet::new();
        parse_lines(&bytes[..=end], &mut |key, action, _| {
            self.uses.insert((key.to_owned(), action.to_owned()));
            keys.insert(key.to_owned());
        });
        for key in keys {
            let Ok(index) = store.read_candidates(&key) else {
                continue;
            };
            for candidate in index.candidates {
                self.blobs.extend(
                    candidate
                        .artifacts
                        .into_iter()
                        .map(|artifact| artifact.digest),
                );
                self.blobs.insert(candidate.stdout.digest);
                self.blobs.insert(candidate.stderr.digest);
            }
        }
        Ok(())
    }
}

fn candidate_kind(crate_name: &str, artifacts: &[crate::Artifact]) -> Kind {
    if crate_name.starts_with("build-script:")
        || artifacts
            .iter()
            .any(|artifact| artifact.file_name == "out-dir.tree")
    {
        Kind::BuildScript
    } else if artifacts.iter().any(|artifact| {
        let name = artifact.file_name.as_str();
        !(name.ends_with(".rlib") || name.ends_with(".rmeta") || name.ends_with(".d"))
    }) {
        // A build script's program and any shared object (a procedural
        // macro or dylib) feed other compilations; everything else linked is
        // a final executable.
        let shared_object = artifacts.iter().any(|artifact| {
            let name = artifact.file_name.as_str();
            name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
        });
        if shared_object || crate_name.starts_with("build_script_") {
            Kind::Linked
        } else {
            Kind::Executable
        }
    } else {
        Kind::Library
    }
}

fn rotate_journal(root: &Path) -> Result<()> {
    let directory = root.join(JOURNAL_DIR);
    let current = directory.join(JOURNAL_FILE);
    if !current.exists() {
        return Ok(());
    }
    let rotated = directory.join(format!("journal-{}-{}.log", now_ms(), std::process::id()));
    fs::rename(&current, &rotated).context("rotate access journal")
}

/// Replaces the journals the snapshot read with one entry per surviving
/// candidate whose last use came from the journal.
fn compact_journal(root: &Path, snapshot: &Snapshot, plan: &Plan) -> Result<()> {
    let directory = root.join(JOURNAL_DIR);
    let mut read = Vec::new();
    if directory.exists() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if name != JOURNAL_FILE && name.ends_with(".log") {
                read.push(path);
            }
        }
    }
    let mut lines = String::new();
    for (unit, evicted) in snapshot.units.iter().zip(&plan.evicted) {
        let (Some(key), Some((action, created))) = (&unit.static_key, &unit.candidate) else {
            continue;
        };
        if !evicted && unit.last_used_ms > *created {
            lines.push_str(&format!("{key} {action} {}\n", unit.last_used_ms));
        }
    }
    if !lines.is_empty() {
        fs::create_dir_all(&directory)?;
        atomic_write(
            &directory.join(format!("compact-{}.log", snapshot.started_ms)),
            lines.as_bytes(),
        )?;
    }
    let keep = format!("compact-{}.log", snapshot.started_ms);
    for path in read {
        if path.file_name().and_then(|name| name.to_str()) != Some(keep.as_str()) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

/// Newest use per (static key, action key) across every journal file.
fn read_journal(root: &Path) -> Result<HashMap<(String, String), u64>> {
    let mut uses: HashMap<(String, String), u64> = HashMap::new();
    let directory = root.join(JOURNAL_DIR);
    if !directory.exists() {
        return Ok(uses);
    }
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("log") {
            continue;
        }
        parse_journal(&path, &mut |key, action, ms| {
            let slot = uses.entry((key.to_owned(), action.to_owned())).or_default();
            *slot = (*slot).max(ms);
        })?;
    }
    Ok(uses)
}

fn parse_journal(path: &Path, visit: &mut dyn FnMut(&str, &str, u64)) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    parse_lines(&bytes, visit);
    Ok(())
}

fn parse_lines(bytes: &[u8], visit: &mut dyn FnMut(&str, &str, u64)) {
    for line in String::from_utf8_lossy(bytes).lines() {
        let mut fields = line.split(' ');
        let (Some(key), Some(action), Some(ms), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if validate_content_key(key).is_err() || validate_content_key(action).is_err() {
            continue;
        }
        if let Ok(ms) = ms.parse() {
            visit(key, action, ms);
        }
    }
}

/// Decodes records on all cores. Any undecodable record aborts the whole
/// collection before anything is deleted.
fn read_parallel<T: serde::de::DeserializeOwned + Send>(
    files: Vec<PathBuf>,
    what: &str,
) -> Result<Vec<(PathBuf, T)>> {
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(16);
    let chunk = files.len().div_ceil(workers).max(1);
    let results = std::thread::scope(|scope| {
        let handles = files
            .chunks(chunk)
            .map(|paths| {
                scope.spawn(move || {
                    let mut decoded = Vec::with_capacity(paths.len());
                    for path in paths {
                        let bytes = match fs::read(path) {
                            Ok(bytes) => bytes,
                            // Removed by a concurrent publication's rewrite.
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                continue;
                            }
                            Err(error) => return Err(anyhow::Error::from(error)),
                        };
                        let record = serde_json::from_slice(&bytes).with_context(|| {
                            format!("decode {what} record {} during GC", path.display())
                        })?;
                        decoded.push((path.clone(), record));
                    }
                    Ok(decoded)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("GC reader panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    Ok(results.into_iter().flatten().collect())
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(millis)
        .unwrap_or(0)
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActionCandidate, Artifact, FileInput, PROTOCOL_VERSION, StreamArtifact,
        compiler_action_key, digest_bytes,
    };
    use std::time::Duration;

    struct Fixture {
        root: PathBuf,
        store: Store,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "bellows-lru-{name}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            let store = Store::open(&root).unwrap();
            Self { root, store }
        }

        /// Publishes a candidate whose only artifact is 100 bytes of `fill`.
        fn publish(&self, name: &str, fill: u8, created_ms: u64, protocol: u32) -> ActionCandidate {
            self.publish_output(name, &format!("lib{name}.rlib"), fill, created_ms, protocol)
        }

        /// Publishes a candidate whose only artifact, `file_name`, is 100
        /// bytes of `fill`.
        fn publish_output(
            &self,
            name: &str,
            file_name: &str,
            fill: u8,
            created_ms: u64,
            protocol: u32,
        ) -> ActionCandidate {
            let payload = vec![fill; 100];
            let digest = digest_bytes(&payload);
            self.store.put_blob(&digest, &payload).unwrap();
            let empty = digest_bytes(b"");
            self.store.put_blob(&empty, b"").unwrap();
            let static_key = digest_bytes(name.as_bytes());
            let files = vec![FileInput {
                path: "$WORKSPACE/src/lib.rs".into(),
                digest: digest_bytes(name.as_bytes()),
            }];
            let candidate = ActionCandidate {
                protocol,
                action_key: compiler_action_key(&static_key, &files, &[], &[]),
                static_key,
                crate_name: name.into(),
                created_ms,
                files,
                host_files: vec![],
                env: vec![],
                artifacts: vec![Artifact {
                    file_name: file_name.into(),
                    digest,
                    executable: !file_name.ends_with(".rlib"),
                }],
                stdout: StreamArtifact {
                    digest: empty.clone(),
                    len: 0,
                },
                stderr: StreamArtifact {
                    digest: empty,
                    len: 0,
                },
                proc_macros: vec![],
            };
            if protocol == PROTOCOL_VERSION {
                self.store.put_candidate(candidate.clone(), 8).unwrap();
            } else {
                // Stores keep older protocols' records, which current
                // validation rejects.
                let path = self.store.action_path(&candidate.static_key).unwrap();
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let index = CandidateIndex {
                    candidates: vec![candidate.clone()],
                };
                atomic_write(&path, &serde_json::to_vec(&index).unwrap()).unwrap();
            }
            candidate
        }

        /// Replaces the journal with uses at chosen times.
        fn uses(&self, uses: &[(&ActionCandidate, u64)]) {
            let directory = self.root.join(JOURNAL_DIR);
            let _ = fs::remove_dir_all(&directory);
            fs::create_dir_all(&directory).unwrap();
            let lines = uses
                .iter()
                .map(|(candidate, ms)| {
                    format!("{} {} {ms}\n", candidate.static_key, candidate.action_key)
                })
                .collect::<String>();
            fs::write(directory.join(JOURNAL_FILE), lines).unwrap();
        }

        /// Ages every blob past the windows that protect racing publications.
        fn age_blobs(&self) {
            let mut blobs = Vec::new();
            collect_paths(&self.root.join("blobs"), &mut blobs).unwrap();
            for blob in blobs {
                fs::OpenOptions::new()
                    .append(true)
                    .open(blob)
                    .unwrap()
                    .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
                    .unwrap();
            }
        }

        fn present(&self, candidate: &ActionCandidate) -> bool {
            self.store
                .action_path(&candidate.static_key)
                .unwrap()
                .exists()
        }

        fn blob(&self, candidate: &ActionCandidate) -> bool {
            self.store.read_blob(&candidate.artifacts[0].digest).is_ok()
        }

        fn options(&self, max_bytes: u64) -> GcOptions {
            GcOptions {
                max_bytes,
                ..GcOptions::default()
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn evicts_the_least_recently_used_candidate_first() {
        let fixture = Fixture::new("lru");
        let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
        let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
        // The older publication was reused after the newer one was written.
        fixture.uses(&[(&older, 3_000)]);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(&fixture.options(100), &unserialized)
            .unwrap();
        assert_eq!(report.records_evicted, 1);
        assert_eq!(report.bytes_after, 100);
        assert!(fixture.present(&older) && fixture.blob(&older));
        assert!(!fixture.present(&newer) && !fixture.blob(&newer));
    }

    #[test]
    fn executables_are_classified_apart_from_inputs_to_other_compilations() {
        let output = |file_name: &str| Artifact {
            file_name: file_name.into(),
            digest: String::new(),
            executable: true,
        };
        let kind = |crate_name: &str, file_name: &str| {
            candidate_kind(crate_name, &[output(file_name), output("dep.d")])
        };
        assert!(kind("suite", "suite-0123abcd") == Kind::Executable);
        assert!(kind("flagship", "flagship.exe") == Kind::Executable);
        assert!(kind("serde_derive", "libserde_derive-0123abcd.so") == Kind::Linked);
        assert!(kind("serde_derive", "serde_derive-0123abcd.dll") == Kind::Linked);
        assert!(kind("build_script_build", "build_script_build-0123abcd") == Kind::Linked);
        assert!(kind("serde", "libserde-0123abcd.rlib") == Kind::Library);
    }

    #[test]
    fn executables_never_push_out_libraries_under_their_own_budget() {
        let fixture = Fixture::new("executables");
        // Oldest first, as one build publishes them: a procedural macro, the
        // library using it, then two test harnesses linking both.
        let proc_macro =
            fixture.publish_output("derive", "libderive-1.so", 1, 500, PROTOCOL_VERSION);
        let library = fixture.publish("library", 2, 1_000, PROTOCOL_VERSION);
        let older_test = fixture.publish_output("suite", "suite-1", 3, 2_000, PROTOCOL_VERSION);
        let newer_test = fixture.publish_output("other", "other-1", 4, 3_000, PROTOCOL_VERSION);
        // Publication times alone order them.
        fixture.uses(&[]);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(
                &GcOptions {
                    max_executable_bytes: Some(100),
                    ..fixture.options(300)
                },
                &unserialized,
            )
            .unwrap();
        assert_eq!(report.records_evicted, 1);
        assert_eq!(report.bytes_after, 300);
        assert_eq!(report.max_executable_bytes, Some(100));
        assert!(fixture.present(&proc_macro) && fixture.blob(&proc_macro));
        assert!(fixture.present(&library) && fixture.blob(&library));
        assert!(!fixture.present(&older_test) && !fixture.blob(&older_test));
        assert!(fixture.present(&newer_test) && fixture.blob(&newer_test));
        let executables = report
            .breakdown
            .iter()
            .find(|bucket| bucket.group == "kind" && bucket.label == "executable")
            .unwrap();
        assert_eq!((executables.records, executables.evicted_records), (2, 1));
    }

    #[test]
    fn without_an_executable_budget_recency_alone_decides() {
        let fixture = Fixture::new("recency");
        let library = fixture.publish("library", 1, 1_000, PROTOCOL_VERSION);
        let test = fixture.publish_output("suite", "suite-1", 2, 2_000, PROTOCOL_VERSION);
        // Publication times alone order them.
        fixture.uses(&[]);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(&fixture.options(100), &unserialized)
            .unwrap();
        assert_eq!(report.records_evicted, 1);
        assert!(!fixture.present(&library));
        assert!(fixture.present(&test));
    }

    #[test]
    fn the_overall_budget_still_applies_after_the_executable_budget() {
        let fixture = Fixture::new("both-budgets");
        let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
        let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
        let test = fixture.publish_output("suite", "suite-1", 3, 3_000, PROTOCOL_VERSION);
        // Publication times alone order them.
        fixture.uses(&[]);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(
                &GcOptions {
                    max_executable_bytes: Some(0),
                    ..fixture.options(100)
                },
                &unserialized,
            )
            .unwrap();
        assert_eq!(report.records_evicted, 2);
        assert!(!fixture.present(&test));
        assert!(!fixture.present(&older));
        assert!(fixture.present(&newer));
    }

    #[test]
    fn a_dry_run_reports_the_plan_and_changes_nothing() {
        let fixture = Fixture::new("dry");
        let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
        let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
        fixture.uses(&[(&older, 3_000)]);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(
                &GcOptions {
                    dry_run: true,
                    ..fixture.options(100)
                },
                &unserialized,
            )
            .unwrap();
        assert!(report.dry_run);
        assert_eq!((report.records_evicted, report.evicted_bytes), (1, 100));
        assert_eq!((report.bytes_before, report.bytes_after), (200, 100));
        assert_eq!(report.journal_entries, 1);
        let crates = report
            .breakdown
            .iter()
            .filter(|bucket| bucket.group == "crate" && bucket.evicted_records == 1)
            .map(|bucket| bucket.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(crates, ["newer"]);
        assert!(fixture.present(&older) && fixture.blob(&older));
        assert!(fixture.present(&newer) && fixture.blob(&newer));
        assert!(fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE).exists());
    }

    #[test]
    fn older_protocols_are_evicted_whatever_the_budget() {
        let fixture = Fixture::new("protocol");
        let old = fixture.publish("old", 1, now_ms(), PROTOCOL_VERSION - 1);
        let current = fixture.publish("current", 2, 1_000, PROTOCOL_VERSION);
        fixture.age_blobs();
        let report = fixture
            .store
            .collect(
                &GcOptions {
                    min_protocol: Some(PROTOCOL_VERSION),
                    ..fixture.options(u64::MAX)
                },
                &unserialized,
            )
            .unwrap();
        assert_eq!(report.records_evicted, 1);
        assert!(!fixture.present(&old) && !fixture.blob(&old));
        assert!(fixture.present(&current) && fixture.blob(&current));
    }

    #[test]
    fn a_blob_offered_again_during_collection_is_kept() {
        let fixture = Fixture::new("offered");
        let candidate = fixture.publish("only", 1, 1_000, PROTOCOL_VERSION);
        // Touched after the collection starts: a client re-offering the blob
        // for a new record while the collection runs.
        fixture.age_blobs();
        fs::OpenOptions::new()
            .append(true)
            .open(
                fixture
                    .store
                    .blob_path(&candidate.artifacts[0].digest)
                    .unwrap(),
            )
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        let report = fixture
            .store
            .collect(&fixture.options(0), &unserialized)
            .unwrap();
        assert_eq!(report.records_evicted, 1);
        assert!(report.skipped_blobs >= 1);
        assert!(!fixture.present(&candidate));
        assert!(fixture.blob(&candidate));
    }

    #[test]
    fn a_candidate_used_during_collection_is_kept_with_its_blobs() {
        let fixture = Fixture::new("racing");
        let candidate = fixture.publish("hot", 1, 1_000, PROTOCOL_VERSION);
        fixture.uses(&[]);
        fixture.age_blobs();
        rotate_journal(&fixture.root).unwrap();
        let snapshot = Snapshot::read(&fixture.store, SystemTime::now()).unwrap();
        let options = fixture.options(0);
        let plan = Plan::new(&snapshot, &options, now_ms());
        let mut report = plan.report(&snapshot, &options);
        assert_eq!(report.records_evicted, 1);
        // A hit lands between the snapshot and the eviction.
        fixture
            .store
            .record_use(&candidate.static_key, &candidate.action_key)
            .unwrap();
        plan.apply(&fixture.store, &snapshot, &unserialized, &mut report)
            .unwrap();
        assert_eq!(report.records_evicted, 0);
        assert!(fixture.present(&candidate) && fixture.blob(&candidate));
    }

    #[test]
    fn compaction_keeps_last_use_for_the_next_collection() {
        let fixture = Fixture::new("compact");
        let older = fixture.publish("older", 1, 1_000, PROTOCOL_VERSION);
        let newer = fixture.publish("newer", 2, 2_000, PROTOCOL_VERSION);
        fixture.uses(&[(&older, 3_000)]);
        fixture.age_blobs();
        let first = fixture
            .store
            .collect(&fixture.options(u64::MAX), &unserialized)
            .unwrap();
        assert_eq!(first.records_evicted, 0);
        let journals = fs::read_dir(fixture.root.join(JOURNAL_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(journals.len(), 1, "{journals:?}");
        assert!(journals[0].starts_with("compact-"));
        let second = fixture
            .store
            .collect(&fixture.options(100), &unserialized)
            .unwrap();
        assert_eq!(second.records_evicted, 1);
        assert!(fixture.present(&older));
        assert!(!fixture.present(&newer));
    }

    #[test]
    fn a_torn_journal_line_is_ignored() {
        let fixture = Fixture::new("torn");
        let candidate = fixture.publish("torn", 1, 1_000, PROTOCOL_VERSION);
        let path = fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&candidate.static_key.as_bytes()[..20])
            .unwrap();
        let uses = read_journal(&fixture.root).unwrap();
        assert_eq!(uses.len(), 1);
    }
}
