use anyhow::{Context, Result};
use bellows_core::{Event, atomic_write, digest_bytes, now_ms};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LOG_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Args, Debug, Default)]
pub struct Selection {
    /// Read the daemonless local cache's diagnostics.
    #[arg(long)]
    pub local: bool,
    #[arg(long, requires = "local")]
    pub cache_dir: Option<PathBuf>,
    /// Select the latest wrapped build in this workspace.
    #[arg(long, conflicts_with = "session")]
    pub latest: bool,
    /// Select a build ID printed by bellows cargo/run/local.
    #[arg(long)]
    pub session: Option<String>,
    /// Show decisions for one Rust crate.
    #[arg(long = "crate")]
    pub crate_name: Option<String>,
}

pub fn log_path(selection: &Selection) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BELLOWS_EVENT_LOG") {
        return Ok(path.into());
    }
    if selection.local {
        return Ok(super::local_state_dir(selection.cache_dir.as_deref())?.join("events.jsonl"));
    }
    let workspace_log = super::event_log_path();
    if std::env::var_os("BELLOWS_STATE_DIR").is_some() {
        return Ok(workspace_log);
    }
    let local_log = super::local_state_dir(None)?.join("events.jsonl");
    let modified = |path: &Path| fs::metadata(path).and_then(|m| m.modified()).ok();
    if modified(&workspace_log) > modified(&local_log) {
        Ok(workspace_log)
    } else {
        Ok(local_log)
    }
}

pub fn select(events: Vec<Event>, selection: &Selection, workspace: &str) -> Vec<Event> {
    let session = selection.session.clone().or_else(|| {
        selection
            .latest
            .then(|| {
                events
                    .iter()
                    .rev()
                    .find(|e| e.kind == "build_start" && e.workspace.as_deref() == Some(workspace))
                    .and_then(|e| e.session_id.clone())
            })
            .flatten()
    });
    events
        .into_iter()
        .filter(|event| {
            (!selection.latest || session.is_some())
                && session
                    .as_ref()
                    .is_none_or(|id| event.session_id.as_ref() == Some(id))
                && selection
                    .crate_name
                    .as_ref()
                    .is_none_or(|name| &event.crate_name == name)
        })
        .collect()
}

pub fn read_selected(selection: &Selection) -> Result<Vec<Event>> {
    let events = read_log(&log_path(selection)?)?;
    let workspace = std::env::current_dir()?
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    Ok(select(events, selection, &workspace))
}

fn lock_log(path: &Path, exclusive: bool) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(name))?;
    if exclusive {
        fs2::FileExt::lock_exclusive(&file)?;
    } else {
        fs2::FileExt::lock_shared(&file)?;
    }
    Ok(file)
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    name.into()
}

pub fn append(path: &Path, event: &Event) -> Result<()> {
    append_with_limit(path, event, LOG_LIMIT)
}

fn append_with_limit(path: &Path, event: &Event, limit: u64) -> Result<()> {
    let _lock = lock_log(path, true)?;
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    if fs::metadata(path).is_ok_and(|m| m.len() + bytes.len() as u64 > limit) {
        let oldest = rotated(path, 2);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        let previous = rotated(path, 1);
        if previous.exists() {
            fs::rename(&previous, &oldest)?;
        }
        fs::rename(path, previous)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&bytes)?;
    Ok(())
}

pub fn read_log(path: &Path) -> Result<Vec<Event>> {
    if !path.exists() && !rotated(path, 1).exists() {
        return Ok(Vec::new());
    }
    let _lock = lock_log(path, false)?;
    let mut events = Vec::new();
    let mut corrupt = 0;
    for file in [rotated(path, 2), rotated(path, 1), path.to_path_buf()] {
        let contents = match fs::read_to_string(file) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str(line) {
                Ok(event) => events.push(event),
                Err(_) => corrupt += 1,
            }
        }
    }
    if corrupt > 0 {
        eprintln!("Bellows diagnostics: ignored {corrupt} malformed event(s)");
    }
    Ok(events)
}

// Miss explanations. `reason_code` classifies misses by these phrases, so the
// wording and the classification change together. Retained logs may still hold
// the earlier phrasing, which remains recognised.
const NOT_CACHED_YET: &str = "not cached yet: first build of this crate seen here";
const BUILD_VARIANT: &str = "not cached: differs from an earlier build of this crate:";
const ENTRY_GONE: &str = "not cached: built before, but its entry was evicted or never saved";
const LEGACY_NOT_CACHED_YET: &str = "first observed identity";
const LEGACY_BUILD_VARIANT: &str = "static identity changed:";
const LEGACY_ENTRY_GONE: &str = "previously observed identity";

pub fn reason_code(kind: &str, detail: &str) -> &'static str {
    match kind {
        "hit" | "l1_hit" | "single_flight" => {
            return if detail.contains("build-script run") {
                "build_script"
            } else if detail.contains("linked output") {
                "linked"
            } else if detail.contains("library output") {
                "library"
            } else {
                "unspecified"
            };
        }
        "store" => {
            return if detail.contains("pinned to this checkout") {
                "checkout_pinned"
            } else if detail.contains("shareable") {
                "shareable"
            } else {
                "unspecified"
            };
        }
        "not_stored" => {
            return if detail.contains("incremental") {
                "incremental_session"
            } else if detail.contains("blob size limit") {
                "too_large_for_server"
            } else if detail.contains("native member") {
                "native_inputs"
            } else if detail.contains("cardinality") {
                "too_many_outputs"
            } else {
                "unsupported_outputs"
            };
        }
        _ => {}
    }
    let variant = detail.contains(BUILD_VARIANT) || detail.contains(LEGACY_BUILD_VARIANT);
    if variant && detail.contains("compiler version changed") {
        "compiler_changed"
    } else if variant && detail.contains("environment changed:") {
        "environment_changed"
    } else if detail.contains("input disappeared:") {
        "input_missing"
    } else if detail.contains("input changed:") {
        "input_changed"
    } else if detail.contains("environment changed:") {
        "environment_changed"
    } else if variant {
        "identity_changed"
    } else if detail.contains(NOT_CACHED_YET) || detail.contains(LEGACY_NOT_CACHED_YET) {
        "cold_identity"
    } else if detail.contains(ENTRY_GONE) || detail.contains(LEGACY_ENTRY_GONE) {
        "entry_missing"
    } else if detail.contains("remote unavailable") {
        "remote_unavailable"
    } else if detail.contains("symlinked compiler input") {
        "symlink_input"
    } else if detail.contains("unstable compiler flag") {
        "unstable_flags"
    } else if detail.contains("host-native CPU") {
        "host_cpu"
    } else if detail.contains("custom sysroot") || detail.contains("custom target specification") {
        "custom_toolchain"
    } else if detail.contains("unsupported emit")
        || detail.contains("temporary outputs")
        || detail.contains("test harness outputs")
    {
        "unsupported_outputs"
    } else if detail.contains("incremental") {
        "incremental"
    } else if detail.contains("procedural macro") || detail.contains("linked crate type proc-macro")
    {
        "proc_macro"
    } else if detail.contains("native linker") {
        "native_inputs"
    } else if detail.contains("linked crate type") {
        "linked_output"
    } else if detail.contains("compiler probe") {
        "compiler_probe"
    } else if detail.contains("timed out") {
        "wait_timeout"
    } else if kind == "corrupt"
        || detail.contains("absent or corrupt")
        || detail.contains("artifacts unavailable")
    {
        "artifact_unavailable"
    } else if detail.contains("protocol") {
        "protocol_mismatch"
    } else if kind == "fallback" {
        "cache_error"
    } else if kind == "bypass" {
        "unsupported_invocation"
    } else {
        "unspecified"
    }
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub decisions: BTreeMap<String, u64>,
    pub reasons: Vec<ReasonSummary>,
    pub session_ids: BTreeSet<String>,
    pub elapsed_ms: Option<u64>,
    pub compiler_process_ms: u64,
}

#[derive(Serialize)]
pub struct ReasonSummary {
    pub kind: String,
    pub reason: String,
    pub count: u64,
    pub crates: BTreeSet<String>,
    pub examples: BTreeSet<String>,
}

pub fn summarize(events: &[Event]) -> Summary {
    let mut summary = Summary::default();
    let mut groups = BTreeMap::<(String, String), ReasonSummary>::new();
    for event in events {
        if let Some(id) = &event.session_id {
            summary.session_ids.insert(id.clone());
        }
        if event.kind == "build_end" {
            summary.elapsed_ms = event.duration_ms;
            continue;
        }
        if event.kind == "compiler_timing" {
            summary.compiler_process_ms += event.duration_ms.unwrap_or(0);
            continue;
        }
        if event.kind == "build_start" {
            continue;
        }
        *summary.decisions.entry(event.kind.clone()).or_default() += 1;
        if !matches!(
            event.kind.as_str(),
            "miss"
                | "bypass"
                | "fallback"
                | "corrupt"
                | "not_stored"
                | "hit"
                | "l1_hit"
                | "single_flight"
                | "store"
        ) {
            continue;
        }
        // Retained events recorded before a category existed carry
        // "unspecified"; derive the current category from their detail.
        let reason = event
            .reason
            .clone()
            .filter(|reason| reason != "unspecified")
            .unwrap_or_else(|| reason_code(&event.kind, &event.detail).into());
        let group = groups
            .entry((event.kind.clone(), reason.clone()))
            .or_insert_with(|| ReasonSummary {
                kind: event.kind.clone(),
                reason,
                count: 0,
                crates: BTreeSet::new(),
                examples: BTreeSet::new(),
            });
        group.count += 1;
        group.crates.insert(event.crate_name.clone());
        if group.examples.len() < 3 {
            group.examples.insert(event.detail.clone());
        }
    }
    if summary.session_ids.len() != 1 {
        summary.elapsed_ms = None;
    }
    summary.reasons = groups.into_values().collect();
    summary
        .reasons
        .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    summary
}

pub fn print_summary(summary: &Summary, limit: usize) {
    print_reasons(summary.reasons.iter(), limit);
}

/// Examples for rebuilt work and problems. Bypasses are by design and, like
/// reuse and storage, already counted by [`print_grouped`].
pub fn print_details(summary: &Summary, limit: usize) {
    print_reasons(
        summary.reasons.iter().filter(|reason| {
            REBUILT
                .iter()
                .chain(PROBLEMS)
                .chain(NOT_STORED)
                .any(|kind| *kind == reason.kind)
        }),
        limit,
    );
}

const REUSED: &[&str] = &["hit", "l1_hit", "single_flight"];
const REBUILT: &[&str] = &["miss"];
const NOT_CACHEABLE: &[&str] = &["bypass"];
const PROBLEMS: &[&str] = &["fallback", "corrupt"];
const STORED: &[&str] = &["store"];
const NOT_STORED: &[&str] = &["not_stored"];

/// Decisions grouped by meaning: reused work, rebuilt work and why, work that
/// Bellows never caches by design, and actual problems. Intentional
/// configuration and by-design bypasses must not read like failures.
pub fn print_grouped(summary: &Summary, color: bool) {
    let count = |kinds: &[&str]| {
        kinds
            .iter()
            .map(|kind| summary.decisions.get(*kind).copied().unwrap_or(0))
            .sum::<u64>()
    };
    let breakdown = |kinds: &[&str]| {
        summary
            .reasons
            .iter()
            .filter(|reason| kinds.contains(&reason.kind.as_str()))
            .map(|reason| format!("{} {}", reason_label(&reason.reason), reason.count))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let line = |key: &str, value: u64, detail: String| {
        let row = crate::terminal::key_value(color, key, value);
        if detail.is_empty() {
            println!("{row}");
        } else {
            println!("{row}  {detail}");
        }
    };
    line("reused", count(REUSED), breakdown(REUSED));
    line("rebuilt", count(REBUILT), breakdown(REBUILT));
    let bypassed = breakdown(NOT_CACHEABLE);
    line(
        "not cacheable",
        count(NOT_CACHEABLE),
        if bypassed.is_empty() {
            bypassed
        } else {
            format!("by design: {bypassed}")
        },
    );
    let problems = count(PROBLEMS);
    line(
        "problems",
        problems,
        if problems == 0 {
            "none".into()
        } else {
            breakdown(PROBLEMS)
        },
    );
    if count(STORED) > 0 {
        line("stored", count(STORED), breakdown(STORED));
    }
    if count(NOT_STORED) > 0 {
        line(
            "not stored",
            count(NOT_STORED),
            format!("compiled, kept local: {}", breakdown(NOT_STORED)),
        );
    }
    for (kind, value) in &summary.decisions {
        if ![REUSED, REBUILT, NOT_CACHEABLE, PROBLEMS, STORED, NOT_STORED]
            .iter()
            .any(|group| group.contains(&kind.as_str()))
        {
            line(&kind.replace('_', " "), *value, String::new());
        }
    }
}

/// Plain-language names for reason codes in grouped statistics.
fn reason_label(reason: &str) -> String {
    match reason {
        "cold_identity" => "not cached yet",
        "identity_changed" => "build variant",
        "environment_changed" => "environment differs",
        "compiler_changed" => "compiler changed",
        "input_changed" => "inputs changed",
        "input_missing" => "input missing",
        "entry_missing" => "evicted or never saved",
        "artifact_unavailable" => "artifact unavailable",
        "proc_macro" => "proc macros",
        "linked_output" => "linked outputs",
        "unsupported_outputs" => "unmodeled outputs",
        "native_inputs" => "native inputs",
        "compiler_probe" => "compiler probes",
        "library" => "libraries",
        "linked" => "linked outputs",
        "build_script" => "build-script runs",
        "shareable" => "shareable",
        "checkout_pinned" => "pinned to checkout",
        "incremental_session" => "incremental session reuse",
        "too_many_outputs" => "too many outputs",
        "too_large_for_server" => "too large for server",
        other => return other.replace('_', " "),
    }
    .into()
}

fn print_reasons<'a>(reasons: impl Iterator<Item = &'a ReasonSummary>, limit: usize) {
    for reason in reasons.take(limit) {
        println!(
            "{:>6} {} / {} · {} crate(s): {}{}",
            reason.count,
            reason.kind,
            reason.reason,
            reason.crates.len(),
            reason
                .crates
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if reason.crates.len() > 5 {
                ", … (use --json for all crates)"
            } else {
                ""
            }
        );
        for example in &reason.examples {
            println!("       {example}");
        }
    }
}

/// Prints a build session's decisions until the build ends.
pub struct Follower {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Follower {
    /// After the wrapped command exits: prints what is left, then stops.
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads events appended to a log from where it was opened, across
/// rotations (the log is renamed to `.1` and a new one started).
struct LogTail {
    path: PathBuf,
    file: File,
    partial: Vec<u8>,
}

impl LogTail {
    fn open(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::End(0)).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            file,
            partial: Vec::new(),
        })
    }

    fn read(&mut self, sink: &dyn Fn(&Event)) {
        self.drain(sink);
        if self.rotated()
            && let Ok(file) = File::open(&self.path)
        {
            // The rotated file was read to its end above.
            self.file = file;
            self.partial.clear();
            self.drain(sink);
        }
    }

    fn drain(&mut self, sink: &dyn Fn(&Event)) {
        let mut appended = Vec::new();
        if self.file.read_to_end(&mut appended).is_err() {
            return;
        }
        self.partial.extend_from_slice(&appended);
        // A line still being written stays until its newline arrives.
        while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line = self.partial.drain(..=end).collect::<Vec<_>>();
            if let Ok(event) = serde_json::from_slice::<Event>(&line) {
                sink(&event);
            }
        }
    }

    fn rotated(&self) -> bool {
        let (Ok(on_disk), Ok(held)) = (fs::metadata(&self.path), self.file.metadata()) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (on_disk.dev(), on_disk.ino()) != (held.dev(), held.ino())
        }
        // A new log starts empty, shorter than the one held.
        #[cfg(not(unix))]
        {
            on_disk.len() < held.len()
        }
    }
}

pub struct BuildSession {
    id: String,
    workspace: String,
    log: PathBuf,
    started: Instant,
}

impl BuildSession {
    pub fn start(state: &Path, workspace: &Path) -> Self {
        let session = Self {
            id: format!("{}-{}", now_ms(), std::process::id()),
            workspace: workspace.to_string_lossy().into_owned(),
            log: std::env::var_os("BELLOWS_EVENT_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| state.join("events.jsonl")),
            started: Instant::now(),
        };
        session.event("build_start", None, "wrapped build started");
        session
    }
    fn event(&self, kind: &str, duration_ms: Option<u64>, detail: &str) {
        let event = Event {
            timestamp_ms: now_ms(),
            kind: kind.into(),
            crate_name: "build".into(),
            static_key: None,
            action_key: None,
            detail: detail.into(),
            reason: None,
            session_id: Some(self.id.clone()),
            workspace: Some(self.workspace.clone()),
            duration_ms,
        };
        if let Err(error) = append(&self.log, &event) {
            eprintln!("Bellows diagnostics unavailable: {error:#}");
        }
    }
    pub fn configure(&self, child: &mut Command) {
        child
            .env("BELLOWS_SESSION_ID", &self.id)
            .env("BELLOWS_EVENT_LOG", &self.log);
    }

    /// Prints this session's decisions as its wrappers record them. The
    /// wrappers print nothing: their stderr is the compiler's, which Cargo
    /// saves with each unit and replays whenever the unit is fresh. This
    /// process's stderr is the terminal's. `None` when the output level
    /// prints no decisions, or the log cannot be read.
    pub fn follow(&self) -> Option<Follower> {
        let output = crate::terminal::Output::from_env();
        if output == crate::terminal::Output::Quiet {
            return None;
        }
        let mut tail = LogTail::open(&self.log)?;
        let id = self.id.clone();
        let color = crate::terminal::stderr_color();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let print = |event: &Event| {
                if event.session_id.as_deref() == Some(id.as_str()) && output.prints(&event.kind) {
                    eprintln!(
                        "{}",
                        crate::terminal::status(
                            color,
                            &event.kind,
                            &event.crate_name,
                            &event.detail
                        )
                    );
                }
            };
            loop {
                // Read once more after the build ends: by then every
                // wrapper has written its last event.
                let last = stopping.load(Ordering::SeqCst);
                tail.read(&print);
                if last {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        Some(Follower {
            stop,
            handle: Some(handle),
        })
    }
    /// Compiles in this session that fell back to plain rustc because the
    /// server stopped answering after the session started.
    pub fn remote_fallbacks(&self) -> usize {
        read_log(&self.log)
            .map(|events| {
                events
                    .iter()
                    .filter(|e| {
                        e.session_id.as_ref() == Some(&self.id)
                            && e.kind == "fallback"
                            && e.detail.starts_with("remote unavailable")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn finish(&self, code: i32) {
        self.event(
            "build_end",
            Some(self.started.elapsed().as_millis() as u64),
            &format!("wrapped command exited {code}"),
        );
        if !crate::terminal::Output::from_env().prints_build_summary() {
            return;
        }
        if let Ok(events) = read_log(&self.log) {
            let events = events
                .into_iter()
                .filter(|e| e.session_id.as_ref() == Some(&self.id))
                .collect::<Vec<_>>();
            let summary = summarize(&events);
            let count = |kinds: &[&str]| {
                kinds
                    .iter()
                    .map(|kind| summary.decisions.get(*kind).copied().unwrap_or(0))
                    .sum::<u64>()
            };
            eprintln!(
                "Bellows build {} · {:.2}s · {} reused · {} rebuilt · {} not cacheable · {} problems",
                self.id,
                self.started.elapsed().as_secs_f64(),
                count(REUSED),
                count(REBUILT),
                count(NOT_CACHEABLE),
                count(PROBLEMS)
            );
            for group in summary
                .reasons
                .iter()
                .filter(|group| group.kind != "bypass")
                .take(6)
            {
                eprintln!(
                    "  {} {} / {} · {}",
                    group.count,
                    group.kind,
                    group.reason,
                    group
                        .examples
                        .first()
                        .map(String::as_str)
                        .unwrap_or_default()
                );
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Fingerprint {
    pub key: String,
    pub components: BTreeMap<String, String>,
}

pub fn fingerprint_changes(before: &Fingerprint, after: &Fingerprint) -> Vec<String> {
    let names = before
        .components
        .keys()
        .chain(after.components.keys())
        .collect::<BTreeSet<_>>();
    let argument_groups_changed = names.iter().any(|name| {
        name.starts_with("argument ")
            && name.as_str() != "argument order"
            && before.components.get(*name) != after.components.get(*name)
    });
    names
        .into_iter()
        .filter(|name| name.as_str() != "argument order" || !argument_groups_changed)
        .filter_map(|name| {
            let previous = before.components.get(name);
            let current = after.components.get(name);
            (previous != current).then(|| {
                format!(
                    "{name} ({} → {})",
                    previous.map(|s| &s[..s.len().min(12)]).unwrap_or("absent"),
                    current.map(|s| &s[..s.len().min(12)]).unwrap_or("absent")
                )
            })
        })
        .collect()
}

pub fn remember_identity(state: &Path, group: &str, current: &Fingerprint) -> Result<String> {
    let directory = state.join("diagnostics");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.json", digest_bytes(group.as_bytes())));
    let _lock = lock_log(&path, true)?;
    let mut history: Vec<Fingerprint> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("read diagnostic identity history")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    // Only this workspace's own history is compared, so a fresh CI runner calls
    // every miss "not cached yet" rather than guessing at a cause.
    let explanation = if history.iter().any(|old| old.key == current.key) {
        ENTRY_GONE.into()
    } else if let Some((old, changes)) = history
        .iter()
        .map(|old| (old, fingerprint_changes(old, current)))
        .min_by_key(|(_, changes)| changes.len())
    {
        format!(
            "{BUILD_VARIANT} {} (compared with {})",
            changes.join("; "),
            &old.key[..old.key.len().min(12)]
        )
    } else {
        NOT_CACHED_YET.into()
    };
    history.retain(|old| old.key != current.key);
    history.insert(0, current.clone());
    history.truncate(8);
    atomic_write(&path, &serde_json::to_vec(&history)?)?;
    Ok(explanation)
}

/// Hash flag values, retaining names only. Never persist environment values or
/// command argument values (which can contain credentials) in diagnostics.
pub fn argument_components(
    args: &[String],
    normalize: impl Fn(&str) -> String,
) -> BTreeMap<String, String> {
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (label, value) = if arg == "-C" || arg == "-L" {
            i += 1;
            let value = args.get(i).map(String::as_str).unwrap_or("");
            (
                format!("argument {arg} {}", value.split('=').next().unwrap_or("")),
                value.to_owned(),
            )
        } else if arg.starts_with("-C") || arg.starts_with("-L") {
            (
                format!(
                    "argument {} {}",
                    &arg[..2],
                    arg[2..].split('=').next().unwrap_or("")
                ),
                arg[2..].to_owned(),
            )
        } else if arg.starts_with('-') {
            if let Some((name, value)) = arg.split_once('=') {
                (format!("argument {name}"), value.to_owned())
            } else if args
                .get(i + 1)
                .is_some_and(|next| !next.starts_with('-') && !next.ends_with(".rs"))
            {
                i += 1;
                (format!("argument {arg}"), args[i].clone())
            } else {
                (format!("argument {arg}"), String::new())
            }
        } else {
            ("source argument".into(), arg.clone())
        };
        groups.entry(label).or_default().push(normalize(&value));
        i += 1;
    }
    groups
        .into_iter()
        .map(|(name, values)| {
            (
                name,
                digest_bytes(&serde_json::to_vec(&values).unwrap_or_default()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_reads_appended_events_across_a_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let roomy = 1 << 20;
        append_with_limit(&path, &event("miss", "s", "before"), roomy).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |event: &Event| seen.borrow_mut().push(event.detail.clone());
        append_with_limit(&path, &event("miss", "s", "one"), roomy).unwrap();
        tail.read(&sink);
        assert_eq!(*seen.borrow(), ["one"]);
        // Written to the old log after the last read, then the log rotates.
        append_with_limit(&path, &event("miss", "s", "late"), roomy).unwrap();
        append_with_limit(&path, &event("miss", "s", "two"), 1).unwrap();
        assert!(rotated(&path, 1).exists());
        tail.read(&sink);
        assert_eq!(*seen.borrow(), ["one", "late", "two"]);
        // A line still being written waits for its newline.
        let line = serde_json::to_vec(&event("miss", "s", "three")).unwrap();
        let (head, rest) = line.split_at(10);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(head).unwrap();
        tail.read(&sink);
        assert_eq!(seen.borrow().len(), 3);
        file.write_all(rest).unwrap();
        file.write_all(b"\n").unwrap();
        tail.read(&sink);
        assert_eq!(*seen.borrow(), ["one", "late", "two", "three"]);
    }

    fn event(kind: &str, session: &str, detail: &str) -> Event {
        Event {
            timestamp_ms: now_ms(),
            kind: kind.into(),
            crate_name: "fixture".into(),
            static_key: None,
            action_key: None,
            detail: detail.into(),
            reason: None,
            session_id: Some(session.into()),
            workspace: Some("/workspace".into()),
            duration_ms: None,
        }
    }

    #[test]
    fn rotation_is_bounded_and_readers_include_retained_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        for i in 0..7 {
            append_with_limit(&path, &event("miss", &i.to_string(), "test"), 1).unwrap();
        }
        let events = read_log(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].session_id.as_deref(), Some("4"));
        assert_eq!(events[2].session_id.as_deref(), Some("6"));
        assert!(!rotated(&path, 3).exists());
    }

    #[test]
    fn concurrent_writers_and_rotation_keep_complete_records() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let mut threads = Vec::new();
        for index in 0..4 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..30 {
                    append_with_limit(&path, &event("miss", &index.to_string(), "test"), 2048)
                        .unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        for file in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
            let contents = fs::read_to_string(file).unwrap();
            assert!(
                contents
                    .lines()
                    .all(|line| serde_json::from_str::<Event>(line).is_ok())
            );
        }
        assert!(!read_log(&path).unwrap().is_empty());
    }

    #[test]
    fn latest_session_uses_start_order_even_when_an_older_build_finishes_last() {
        let events = vec![
            event("build_start", "old", ""),
            event("build_start", "new", ""),
            event("miss", "new", "new"),
            event("build_end", "new", ""),
            event("miss", "old", "old"),
            event("build_end", "old", ""),
        ];
        let selection = Selection {
            latest: true,
            ..Default::default()
        };
        let selected = select(events, &selection, "/workspace");
        assert_eq!(selected.len(), 3);
        assert!(
            selected
                .iter()
                .all(|e| e.session_id.as_deref() == Some("new"))
        );
    }

    #[test]
    fn identity_history_names_changes_without_recording_values() {
        let temp = tempfile::tempdir().unwrap();
        let first = Fingerprint {
            key: digest_bytes(b"first"),
            components: BTreeMap::from([(
                "environment changed: RUSTFLAGS".into(),
                digest_bytes(b"--cfg a_secret_value"),
            )]),
        };
        assert!(
            remember_identity(temp.path(), "fixture", &first)
                .unwrap()
                .contains(NOT_CACHED_YET)
        );
        let next = Fingerprint {
            key: digest_bytes(b"next"),
            components: BTreeMap::from([(
                "environment changed: RUSTFLAGS".into(),
                digest_bytes(b"--cfg a_new_secret"),
            )]),
        };
        let detail = remember_identity(temp.path(), "fixture", &next).unwrap();
        assert!(detail.contains("environment changed: RUSTFLAGS"));
        assert_eq!(reason_code("miss", &detail), "environment_changed");
        assert!(!detail.contains("a_secret_value"));
        assert!(
            remember_identity(temp.path(), "fixture", &first)
                .unwrap()
                .contains(ENTRY_GONE)
        );
        for i in 0..20 {
            let mut fp = next.clone();
            fp.key = digest_bytes(i.to_string().as_bytes());
            remember_identity(temp.path(), "fixture", &fp).unwrap();
        }
        let bytes = fs::read(
            temp.path()
                .join("diagnostics")
                .join(format!("{}.json", digest_bytes(b"fixture"))),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<Fingerprint>>(&bytes)
                .unwrap()
                .len(),
            8
        );
        assert!(!String::from_utf8(bytes).unwrap().contains("a_new_secret"));
    }

    #[test]
    fn miss_explanations_and_their_legacy_phrasing_share_reason_codes() {
        for (current, legacy, code) in [
            (
                NOT_CACHED_YET.to_owned(),
                "first observed identity; no earlier local identity to compare (cold cache or new crate)",
                "cold_identity",
            ),
            (
                ENTRY_GONE.to_owned(),
                "previously observed identity has no usable cache entry; it may have been evicted or never published",
                "entry_missing",
            ),
            (
                format!("{BUILD_VARIANT} argument --cfg (a1 → b2) (compared with c3)"),
                "static identity changed: argument --cfg (a1 → b2) (compared with c3)",
                "identity_changed",
            ),
            (
                format!("{BUILD_VARIANT} compiler version changed (compared with c3)"),
                "static identity changed: compiler version changed (compared with c3)",
                "compiler_changed",
            ),
        ] {
            assert_eq!(reason_code("miss", &current), code, "{current}");
            assert_eq!(reason_code("miss", legacy), code, "{legacy}");
        }
        // Explanations describe the miss; none names disabled configuration.
        for detail in [NOT_CACHED_YET, ENTRY_GONE, BUILD_VARIANT] {
            assert!(!detail.contains("disabled"));
        }
        assert_eq!(reason_label("cold_identity"), "not cached yet");
        assert_eq!(reason_label("linked_output"), "linked outputs");
        assert_eq!(reason_label("some_new_reason"), "some new reason");
    }

    #[test]
    fn legacy_events_and_malformed_lines_remain_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        fs::write(&path, "{\"timestamp_ms\":1,\"kind\":\"miss\",\"crate_name\":\"old\",\"static_key\":null,\"action_key\":null,\"detail\":\"input changed: file.rs\"}\n{broken\n").unwrap();
        let events = read_log(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(summarize(&events).reasons[0].reason, "input_changed");
    }
}
