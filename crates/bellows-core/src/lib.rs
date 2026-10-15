pub mod execution;
mod gc;

pub use gc::GcOptions;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 6;
pub const CONTENT_KEY_LEN: usize = 64;
pub const MAX_CANDIDATE_FILES: usize = 100_000;
pub const MAX_CANDIDATE_ENV: usize = 4_096;
pub const MAX_CANDIDATE_ARTIFACTS: usize = 16;
pub const MAX_RECORD_ARTIFACTS: usize = 100_000;
pub const MAX_RECORD_ENV: usize = 4_096;
const UNPUBLISHED_BLOB_GRACE: Duration = Duration::from_secs(60 * 60);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn digest_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn compiler_action_key(
    static_key: &str,
    files: &[FileInput],
    host_files: &[FileInput],
    environment: &[EnvInput],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hash_key_field(&mut hasher, "static", static_key.as_bytes());
    hash_key_field(
        &mut hasher,
        "files",
        &serde_json::to_vec(files).unwrap_or_default(),
    );
    hash_key_field(
        &mut hasher,
        "host-files",
        &serde_json::to_vec(host_files).unwrap_or_default(),
    );
    hash_key_field(
        &mut hasher,
        "env",
        &serde_json::to_vec(environment).unwrap_or_default(),
    );
    hasher.finalize().to_hex().to_string()
}

fn hash_key_field(hasher: &mut blake3::Hasher, name: &str, bytes: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(digest_bytes(&bytes))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInput {
    pub path: String,
    pub digest: String,
}

/// Prefix for synthetic environment inputs that pin a candidate to this
/// checkout's absolute roots (`@bellows:root:$WORKSPACE`). They are not process
/// environment variables and are computed by the client during validation.
pub const PIN_PREFIX: &str = "@bellows:root:";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvInput {
    pub name: String,
    pub value_digest: Option<String>,
    /// The digest covers the value after workspace/target path normalization.
    /// Only recorded when no captured output contains a checkout path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub normalized: bool,
}

impl EnvInput {
    pub fn capture(name: impl Into<String>, value: Option<&str>) -> Self {
        Self {
            name: name.into(),
            value_digest: value.map(|v| digest_bytes(v.as_bytes())),
            normalized: false,
        }
    }

    pub fn capture_normalized(name: impl Into<String>, normalized_value: &str) -> Self {
        Self {
            name: name.into(),
            value_digest: Some(digest_bytes(normalized_value.as_bytes())),
            normalized: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub file_name: String,
    pub digest: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamArtifact {
    pub digest: String,
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionCandidate {
    pub protocol: u32,
    pub static_key: String,
    pub action_key: String,
    pub crate_name: String,
    pub created_ms: u64,
    pub files: Vec<FileInput>,
    /// Absolute inputs outside every normalized root, such as system native
    /// libraries and C runtime objects read by the linker. They only match on
    /// a host whose files at those exact paths are byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_files: Vec<FileInput>,
    pub env: Vec<EnvInput>,
    pub artifacts: Vec<Artifact>,
    pub stdout: StreamArtifact,
    pub stderr: StreamArtifact,
    /// Crate names of procedural macros passed directly with `--extern`.
    /// Diagnostic only; their artifact digests are part of the static key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proc_macros: Vec<String>,
}

pub fn validate_content_key(key: &str) -> Result<()> {
    if key.len() != CONTENT_KEY_LEN
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("content key must be a lowercase {CONTENT_KEY_LEN}-character BLAKE3 digest")
    }
    Ok(())
}

pub fn validate_normalized_input_path(value: &str) -> Result<()> {
    const ROOTS: &[&str] = &[
        "$CHECKOUT",
        "$WORKSPACE",
        "$PROFILE",
        "$TARGET",
        "$CARGO_HOME",
        "$RUSTUP_HOME",
        "$HOME",
    ];
    if value.len() > 4096 || value.contains('\0') {
        bail!("normalized input path is empty or too long")
    }
    let Some(root) = ROOTS.iter().find(|root| {
        value == **root
            || value
                .strip_prefix(**root)
                .is_some_and(|suffix| suffix.starts_with(['/', '\\']))
    }) else {
        bail!("normalized input path has no recognized root: {value}")
    };
    let suffix = value
        .strip_prefix(root)
        .unwrap_or_default()
        .trim_start_matches(['/', '\\']);
    if suffix.is_empty()
        || suffix
            .split(['/', '\\'])
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        bail!("normalized input path contains a forbidden segment: {value}")
    }
    Ok(())
}

/// Host inputs are absolute paths that no normalized root covers.
pub fn validate_host_input_path(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        bail!("host input path is empty or too long")
    }
    let windows_drive = value.as_bytes().get(1) == Some(&b':')
        && value.as_bytes()[0].is_ascii_alphabetic()
        && matches!(value.as_bytes().get(2), Some(b'/' | b'\\'));
    if !(value.starts_with('/') || windows_drive || value.starts_with("\\\\")) {
        bail!("host input path is not absolute: {value}")
    }
    if value
        .split(['/', '\\'])
        .skip(1)
        .any(|segment| matches!(segment, "." | ".."))
    {
        bail!("host input path contains a forbidden segment: {value}")
    }
    Ok(())
}

pub fn validate_candidate_manifest(candidate: &ActionCandidate) -> Result<()> {
    if candidate.protocol != PROTOCOL_VERSION {
        bail!("unsupported candidate protocol {}", candidate.protocol)
    }
    validate_content_key(&candidate.static_key)?;
    validate_content_key(&candidate.action_key)?;
    if candidate.crate_name.is_empty() || candidate.crate_name.len() > 256 {
        bail!("invalid candidate crate name")
    }
    if candidate.files.len() > MAX_CANDIDATE_FILES
        || candidate.env.len() > MAX_CANDIDATE_ENV
        || candidate.artifacts.len() > MAX_CANDIDATE_ARTIFACTS
    {
        bail!("candidate manifest exceeds cardinality limits")
    }

    let mut file_paths = BTreeSet::new();
    for file in &candidate.files {
        validate_normalized_input_path(&file.path)?;
        validate_content_key(&file.digest)?;
        if !file_paths.insert(&file.path) {
            bail!("duplicate candidate input path: {}", file.path)
        }
    }
    if candidate.host_files.len() > MAX_CANDIDATE_FILES
        || candidate.proc_macros.len() > MAX_CANDIDATE_ENV
    {
        bail!("candidate manifest exceeds cardinality limits")
    }
    for file in &candidate.host_files {
        validate_host_input_path(&file.path)?;
        validate_content_key(&file.digest)?;
        if !file_paths.insert(&file.path) {
            bail!("duplicate candidate input path: {}", file.path)
        }
    }
    for name in &candidate.proc_macros {
        if name.is_empty() || name.len() > 256 {
            bail!("invalid candidate proc-macro name")
        }
    }
    let mut environment_names = BTreeSet::new();
    for input in &candidate.env {
        if input.name.is_empty() || input.name.len() > 256 || input.name.contains('=') {
            bail!("invalid candidate environment name")
        }
        if !environment_names.insert(&input.name) {
            bail!("duplicate candidate environment name: {}", input.name)
        }
        if let Some(digest) = &input.value_digest {
            validate_content_key(digest)?;
        }
    }
    let mut artifact_names = BTreeSet::new();
    for artifact in &candidate.artifacts {
        let path = validate_relative_path(&artifact.file_name)?;
        if path.components().count() != 1 {
            bail!(
                "compiler artifact must be a file name: {}",
                artifact.file_name
            )
        }
        validate_content_key(&artifact.digest)?;
        if !artifact_names.insert(&artifact.file_name) {
            bail!("duplicate compiler artifact: {}", artifact.file_name)
        }
    }
    validate_content_key(&candidate.stdout.digest)?;
    validate_content_key(&candidate.stderr.digest)?;
    let expected = compiler_action_key(
        &candidate.static_key,
        &candidate.files,
        &candidate.host_files,
        &candidate.env,
    );
    if expected != candidate.action_key {
        bail!("candidate action key does not match dependency manifest")
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateIndex {
    pub candidates: Vec<ActionCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub client_id: String,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LeaseResponse {
    Owned {
        token: String,
        expires_ms: u64,
    },
    Wait {
        retry_after_ms: u64,
        expires_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub service: String,
    pub protocol: u32,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerStats {
    pub blobs: u64,
    pub blob_bytes: u64,
    pub action_indexes: u64,
    pub candidates: u64,
    pub active_leases: u64,
    #[serde(default)]
    pub declared_actions: u64,
    #[serde(default)]
    pub archives: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GcReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Compiler candidates and declared/archive records evicted (planned, in
    /// a dry run).
    pub records_evicted: u64,
    pub blobs_evicted: u64,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub max_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_executable_bytes: Option<u64>,
    /// Compiler candidates plus declared and archive records before collection.
    #[serde(default)]
    pub records: u64,
    #[serde(default)]
    pub candidates: u64,
    #[serde(default)]
    pub blobs: u64,
    /// Bytes freed by evicting records (excluding already unreferenced blobs).
    #[serde(default)]
    pub evicted_bytes: u64,
    #[serde(default)]
    pub unreferenced_blobs: u64,
    #[serde(default)]
    pub unreferenced_bytes: u64,
    /// Unreferenced blobs kept because their record may still be uploading.
    #[serde(default)]
    pub protected_blobs: u64,
    /// Planned blob deletions skipped because they were reused mid-collection.
    #[serde(default)]
    pub skipped_blobs: u64,
    #[serde(default)]
    pub journal_entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_kept_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_evicted_ms: Option<u64>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<GcBucket>,
}

/// Totals for one label of one grouping (protocol, kind, scope, last use,
/// crate) in a collection report.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GcBucket {
    pub group: String,
    pub label: String,
    pub records: u64,
    /// Bytes referenced only by records with this label.
    pub exclusive_bytes: u64,
    pub evicted_records: u64,
    pub evicted_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcRequest {
    pub max_bytes: u64,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_protocol: Option<u32>,
    /// Separate budget for final executables; see `GcOptions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_executable_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlatformIdentity {
    pub rustc: String,
    pub cargo: String,
    pub os: String,
    pub arch: String,
    pub path_digest: String,
    pub rustup_home_digest: String,
    pub rustup_toolchain: Option<String>,
    pub host_cc: Option<String>,
    pub host_ld: Option<String>,
    pub host_ar: Option<String>,
}

impl PlatformIdentity {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            rustc: command_version("rustc")?,
            cargo: command_version("cargo")?,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            path_digest: execution::platform_path_digest(),
            rustup_home_digest: digest_bytes(rustup_home().to_string_lossy().as_bytes()),
            rustup_toolchain: std::env::var("RUSTUP_TOOLCHAIN")
                .ok()
                .or_else(active_rustup_toolchain),
            host_cc: optional_command_version("cc"),
            host_ld: optional_command_version("ld"),
            host_ar: optional_command_version("ar"),
        })
    }
}

fn optional_command_version(command: &str) -> Option<String> {
    let output = Command::new(command).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut identity = output.stdout;
    identity.extend_from_slice(&output.stderr);
    String::from_utf8(identity)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn active_rustup_toolchain() -> Option<String> {
    let output = Command::new("rustup")
        .args(["show", "active-toolchain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

pub fn rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| user_home().map(|home| home.join(".rustup")))
        .unwrap_or_else(|| PathBuf::from(".rustup"))
}

pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return Some(home.into());
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

fn command_version(command: &str) -> Result<String> {
    let output = Command::new(command)
        .arg("-vV")
        .output()
        .with_context(|| format!("run {command} -vV"))?;
    if !output.status.success() {
        bail!("{command} -vV failed")
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredActionRecord {
    pub protocol: u32,
    pub key: String,
    pub name: String,
    pub created_ms: u64,
    pub platform: PlatformIdentity,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<Artifact>,
    pub output_paths: Vec<String>,
    pub outputs: Vec<Artifact>,
    pub stdout: StreamArtifact,
    pub stderr: StreamArtifact,
    pub duration_ms: u64,
    pub executor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveManifest {
    pub protocol: u32,
    pub name: String,
    pub tree_digest: String,
    pub created_ms: u64,
    pub producer_action: Option<String>,
    pub files: Vec<Artifact>,
}

/// Output replacement must never delete a declared source input.
pub fn validate_output_roots(inputs: &[Artifact], outputs: &[String]) -> Result<()> {
    for output in outputs {
        let output = validate_relative_path(output)?;
        if output.starts_with(".bellows") {
            bail!(
                "declared output overlaps Bellows state: {}",
                output.display()
            );
        }
        for input in inputs {
            let input = validate_relative_path(&input.file_name)?;
            if input.starts_with(&output) || output.starts_with(&input) {
                bail!(
                    "declared input/output overlap: {} and {}",
                    input.display(),
                    output.display()
                );
            }
        }
    }
    Ok(())
}

pub fn validate_declared_record(record: &DeclaredActionRecord) -> Result<()> {
    validate_output_roots(&record.inputs, &record.output_paths)?;
    if record.protocol != PROTOCOL_VERSION {
        bail!("unsupported declared record protocol {}", record.protocol)
    }
    validate_content_key(&record.key)?;
    if record.name.is_empty() || record.name.len() > 256 {
        bail!("invalid declared action name")
    }
    validate_platform_identity(&record.platform)?;
    validate_declared_command(&record.command)?;
    if record.environment.len() > MAX_RECORD_ENV
        || record.inputs.len() > MAX_RECORD_ARTIFACTS
        || record.output_paths.len() > MAX_RECORD_ARTIFACTS
        || record.outputs.len() > MAX_RECORD_ARTIFACTS
    {
        bail!("declared record exceeds cardinality limits")
    }
    for (name, value) in &record.environment {
        if name.is_empty() || name.len() > 256 || name.contains('=') || value.len() > 1024 * 1024 {
            bail!("invalid declared environment entry")
        }
    }
    validate_artifacts(&record.inputs, "declared input")?;
    validate_artifacts(&record.outputs, "declared output")?;
    let mut output_paths = BTreeSet::new();
    for output in &record.output_paths {
        validate_relative_path(output)?;
        if !output_paths.insert(output) {
            bail!("duplicate declared output path: {output}")
        }
    }
    for output in &record.outputs {
        let covered = record
            .output_paths
            .iter()
            .any(|declaration| relative_path_is_within(&output.file_name, declaration));
        if !covered {
            bail!(
                "record output {} is outside declared roots",
                output.file_name
            )
        }
    }
    validate_content_key(&record.stdout.digest)?;
    validate_content_key(&record.stderr.digest)?;
    if record.executor.is_empty() || record.executor.len() > 1024 {
        bail!("invalid declared executor identity")
    }
    let expected = declared_action_key(
        &record.name,
        &record.platform,
        &record.command,
        &record.environment,
        &record.inputs,
        &record.output_paths,
    );
    if expected != record.key {
        bail!("declared record key does not match manifest")
    }
    Ok(())
}

pub fn validate_archive_manifest(manifest: &ArchiveManifest) -> Result<()> {
    if manifest.protocol != PROTOCOL_VERSION {
        bail!("unsupported archive protocol {}", manifest.protocol)
    }
    validate_archive_name(&manifest.name)?;
    validate_content_key(&manifest.tree_digest)?;
    if let Some(producer) = &manifest.producer_action {
        validate_content_key(producer)?;
    }
    if manifest.files.len() > MAX_RECORD_ARTIFACTS {
        bail!("archive has an invalid file count")
    }
    validate_artifacts(&manifest.files, "archive file")?;
    if tree_digest(&manifest.files) != manifest.tree_digest {
        bail!("archive tree digest mismatch")
    }
    Ok(())
}

fn validate_platform_identity(platform: &PlatformIdentity) -> Result<()> {
    for value in [
        Some(platform.rustc.as_str()),
        Some(platform.cargo.as_str()),
        Some(platform.os.as_str()),
        Some(platform.arch.as_str()),
        platform.rustup_toolchain.as_deref(),
        platform.host_cc.as_deref(),
        platform.host_ld.as_deref(),
        platform.host_ar.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() || value.len() > 64 * 1024 || value.contains('\0') {
            bail!("invalid platform identity field")
        }
    }
    validate_content_key(&platform.path_digest)?;
    validate_content_key(&platform.rustup_home_digest)
}

fn validate_artifacts(artifacts: &[Artifact], kind: &str) -> Result<()> {
    let mut paths = BTreeSet::new();
    for artifact in artifacts {
        validate_relative_path(&artifact.file_name)?;
        validate_content_key(&artifact.digest)?;
        if !paths.insert(&artifact.file_name) {
            bail!("duplicate {kind} path: {}", artifact.file_name)
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub key: String,
    pub name: String,
    pub platform: PlatformIdentity,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<Artifact>,
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub cache_hit: bool,
    pub record: DeclaredActionRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub timestamp_ms: u64,
    pub kind: String,
    pub crate_name: String,
    pub static_key: Option<String>,
    pub action_key: Option<String>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_inner(root.into(), true)
    }

    /// Open a store from a latency-sensitive child process.
    ///
    /// Callers must arrange periodic maintenance through [`Store::open`] in a
    /// parent or service process. Rustc wrappers use this entry point so a
    /// large durable local cache is not recursively scanned once per crate.
    pub fn open_for_access(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_inner(root.into(), false)
    }

    /// Remove orphaned temporary files (an hour old or more) left by interrupted
    /// writes. This walks the whole store: a service runs it in the background,
    /// never before it starts answering.
    pub fn remove_orphan_temps(&self) -> Result<()> {
        cleanup_orphan_temps(&self.root, Duration::from_secs(60 * 60))
    }

    fn open_inner(root: PathBuf, cleanup_temps: bool) -> Result<Self> {
        ensure_directory(&root)?;
        ensure_directory(&root.join("blobs"))?;
        ensure_directory(&root.join("actions"))?;
        ensure_directory(&root.join("declared"))?;
        ensure_directory(&root.join("archives"))?;
        ensure_directory(&root.join("quarantine"))?;
        if cleanup_temps {
            cleanup_orphan_temps(&root, Duration::from_secs(60 * 60))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn check_writable(&self) -> Result<()> {
        let path = self.root.join(format!(
            ".health-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        atomic_write(&path, b"bellows-storage-health")?;
        fs::remove_file(&path)
            .with_context(|| format!("remove health probe {}", path.display()))?;
        sync_parent(&path)
    }

    fn checked_key<'a>(&self, key: &'a str) -> Result<&'a str> {
        validate_content_key(key)?;
        Ok(key)
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let digest = self.checked_key(digest)?;
        Ok(self.root.join("blobs").join(&digest[..2]).join(digest))
    }

    pub fn action_path(&self, static_key: &str) -> Result<PathBuf> {
        let key = self.checked_key(static_key)?;
        Ok(self
            .root
            .join("actions")
            .join(&key[..2])
            .join(format!("{key}.json")))
    }

    pub fn declared_path(&self, key: &str) -> Result<PathBuf> {
        let key = self.checked_key(key)?;
        Ok(self
            .root
            .join("declared")
            .join(&key[..2])
            .join(format!("{key}.json")))
    }

    pub fn archive_path(&self, name: &str) -> Result<PathBuf> {
        validate_archive_name(name)?;
        Ok(self.root.join("archives").join(format!("{name}.json")))
    }

    pub fn put_blob(&self, expected: &str, bytes: &[u8]) -> Result<bool> {
        let actual = digest_bytes(bytes);
        if actual != expected {
            bail!("blob digest mismatch: expected {expected}, got {actual}")
        }
        let temp = self.blob_temp_path(expected)?;
        let written = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        self.commit_blob(expected, &temp)
    }

    /// A fresh temporary path beside the blob's final location, for writing
    /// it before [`Store::commit_blob`]. Orphans are collected like any other
    /// temporary file.
    pub fn blob_temp_path(&self, digest: &str) -> Result<PathBuf> {
        let path = self.blob_path(digest)?;
        let parent = path.parent().context("blob path has no parent")?;
        fs::create_dir_all(parent)?;
        Ok(parent.join(format!(
            ".tmp-{}-{}-{}",
            std::process::id(),
            now_ms(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }

    /// Moves `temp`, a synced file the caller has verified hashes to
    /// `expected`, into place. Only the existence check and the rename hold
    /// the store's mutation lock, so hashing and writing a large blob never
    /// block other writers or a collection. Returns false (and removes
    /// `temp`) when a verified copy was already present, refreshing it so a
    /// running collection keeps it.
    pub fn commit_blob(&self, expected: &str, temp: &Path) -> Result<bool> {
        let path = self.blob_path(expected)?;
        let committed = self.with_mutation_lock(|| {
            if path.exists() {
                if self.read_blob(expected).is_ok() {
                    self.touch_blob(expected)?;
                    return Ok(false);
                }
                fs::remove_file(&path)
                    .with_context(|| format!("remove corrupt blob {expected}"))?;
            }
            fs::rename(temp, &path).with_context(|| format!("move blob {expected} into place"))?;
            sync_parent(&path)?;
            Ok(true)
        });
        if !matches!(committed, Ok(true)) {
            let _ = fs::remove_file(temp);
        }
        committed
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        let bytes = fs::read(&path).with_context(|| format!("read blob {digest}"))?;
        let actual = digest_bytes(&bytes);
        if actual != digest {
            bail!("corrupt blob {digest}: content hashes to {actual}")
        }
        Ok(bytes)
    }

    pub fn read_candidates(&self, static_key: &str) -> Result<CandidateIndex> {
        let path = self.action_path(static_key)?;
        if !path.exists() {
            return Ok(CandidateIndex::default());
        }
        let index: CandidateIndex = self.read_json_record(&path)?;
        self.validate_record(&path, index, |index| {
            if index.candidates.len() > 1024 {
                bail!("candidate index exceeds cardinality limit")
            }
            for candidate in &index.candidates {
                validate_candidate_manifest(candidate)?;
                if candidate.static_key != static_key {
                    bail!("candidate static key does not match storage path")
                }
            }
            Ok(())
        })
    }

    pub fn put_candidate(&self, candidate: ActionCandidate, max_candidates: usize) -> Result<()> {
        validate_candidate_manifest(&candidate)?;
        self.with_mutation_lock(|| self.put_candidate_unlocked(candidate, max_candidates))
    }

    fn put_candidate_unlocked(
        &self,
        candidate: ActionCandidate,
        max_candidates: usize,
    ) -> Result<()> {
        let path = self.action_path(&candidate.static_key)?;
        fs::create_dir_all(path.parent().context("action path has no parent")?)?;
        let mut index = self.read_candidates(&candidate.static_key)?;
        index
            .candidates
            .retain(|c| c.action_key != candidate.action_key);
        let (static_key, action_key) = (candidate.static_key.clone(), candidate.action_key.clone());
        index.candidates.insert(0, candidate);
        index.candidates.truncate(max_candidates.max(1));
        atomic_write(&path, &serde_json::to_vec_pretty(&index)?)?;
        self.record_use(&static_key, &action_key)
    }

    pub fn read_declared(&self, key: &str) -> Result<Option<DeclaredActionRecord>> {
        let path = self.declared_path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        let record: DeclaredActionRecord = self.read_json_record(&path)?;
        self.validate_record(&path, record, |record| {
            validate_declared_record(record)?;
            if record.key != key {
                bail!("declared record key does not match storage path")
            }
            Ok(())
        })
        .map(Some)
    }

    pub fn put_declared(&self, record: &DeclaredActionRecord) -> Result<bool> {
        validate_declared_record(record)?;
        self.with_mutation_lock(|| self.put_declared_unlocked(record))
    }

    pub fn remove_declared(&self, key: &str) -> Result<bool> {
        let path = self.declared_path(key)?;
        self.with_mutation_lock(|| {
            if !path.exists() {
                return Ok(false);
            }
            fs::remove_file(&path).with_context(|| format!("remove declared action {key}"))?;
            sync_parent(&path)?;
            Ok(true)
        })
    }

    fn put_declared_unlocked(&self, record: &DeclaredActionRecord) -> Result<bool> {
        let path = self.declared_path(&record.key)?;
        if let Some(existing) = self.read_declared(&record.key)? {
            if existing == *record {
                return Ok(false);
            }
            bail!("declared action key already exists with different content")
        }
        fs::create_dir_all(path.parent().context("declared path has no parent")?)?;
        atomic_write(&path, &serde_json::to_vec_pretty(record)?)?;
        Ok(true)
    }

    pub fn read_archive(&self, name: &str) -> Result<Option<ArchiveManifest>> {
        let path = self.archive_path(name)?;
        if !path.exists() {
            return Ok(None);
        }
        let manifest: ArchiveManifest = self.read_json_record(&path)?;
        self.validate_record(&path, manifest, |manifest| {
            validate_archive_manifest(manifest)?;
            if manifest.name != name {
                bail!("archive name does not match storage path")
            }
            Ok(())
        })
        .map(Some)
    }

    pub fn put_archive(&self, manifest: &ArchiveManifest) -> Result<bool> {
        validate_archive_manifest(manifest)?;
        self.with_mutation_lock(|| self.put_archive_unlocked(manifest))
    }

    fn put_archive_unlocked(&self, manifest: &ArchiveManifest) -> Result<bool> {
        let path = self.archive_path(&manifest.name)?;
        if let Some(existing) = self.read_archive(&manifest.name)? {
            if existing.tree_digest == manifest.tree_digest {
                return Ok(false);
            }
            bail!("archive name is already bound to a different tree")
        }
        atomic_write(&path, &serde_json::to_vec_pretty(manifest)?)?;
        Ok(true)
    }

    fn with_mutation_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock_path = self.root.join(".mutation.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open store lock {}", lock_path.display()))?;
        fs2::FileExt::lock_exclusive(&lock)
            .with_context(|| format!("lock store {}", self.root.display()))?;
        let result = operation();
        let unlock = fs2::FileExt::unlock(&lock)
            .with_context(|| format!("unlock store {}", self.root.display()));
        match (result, unlock) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn read_json_record<T: DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let bytes = fs::read(path).with_context(|| format!("read record {}", path.display()))?;
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.quarantine_error(path, error.into())),
        }
    }

    fn validate_record<T>(
        &self,
        path: &Path,
        value: T,
        validate: impl FnOnce(&T) -> Result<()>,
    ) -> Result<T> {
        match validate(&value) {
            Ok(()) => Ok(value),
            Err(error) => Err(self.quarantine_error(path, error)),
        }
    }

    fn quarantine_error(&self, path: &Path, error: anyhow::Error) -> anyhow::Error {
        let quarantine = self.root.join("quarantine").join(format!(
            "{}-{}-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("record"),
            now_ms(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let quarantine_result = (|| -> Result<()> {
            fs::rename(path, &quarantine)?;
            sync_parent(&quarantine)
        })();
        match quarantine_result {
            Ok(()) => error.context(format!(
                "record {} failed validation and was quarantined",
                path.display()
            )),
            Err(quarantine_error) => error.context(format!(
                "record {} failed validation; quarantine also failed: {quarantine_error}",
                path.display()
            )),
        }
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("store directory may not be a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("store path is not a directory: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn cleanup_orphan_temps(root: &Path, minimum_age: Duration) -> Result<()> {
    let now = SystemTime::now();
    let mut files = Vec::new();
    collect_paths_internal(root, &mut files, true)?;
    for path in files {
        let is_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".tmp-"));
        if !is_temp {
            continue;
        }
        let old_enough = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= minimum_age);
        if old_enough {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn collect_paths(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    collect_paths_internal(root, files, false)
}

fn collect_paths_internal(
    root: &Path,
    files: &mut Vec<PathBuf>,
    include_temps: bool,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("store traversal encountered symlink: {}", path.display())
        }
        if metadata.is_dir() {
            collect_paths_internal(&path, files, include_temps)?;
        } else if metadata.is_file() {
            if !include_temps
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".tmp-"))
            {
                continue;
            }
            files.push(path);
        } else {
            bail!(
                "store traversal encountered non-file entry: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut files = Vec::new();
    collect_paths(root, &mut files)?;
    files.into_iter().try_fold(0u64, |total, path| {
        Ok(total.saturating_add(fs::metadata(path)?.len()))
    })
}

pub fn validate_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("path must be a non-empty relative path: {value}")
    }
    if value
        .split(['/', '\\'])
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        bail!("path contains a forbidden segment: {value}")
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("path contains a forbidden component: {value}")
        }
    }
    Ok(path.to_path_buf())
}

// Callers validate both relative paths before checking component containment.
// Records may come from Windows even when the content store runs on Linux.
pub fn relative_path_is_within(path: &str, root: &str) -> bool {
    let mut components = path.split(['/', '\\']);
    root.split(['/', '\\'])
        .all(|part| components.next() == Some(part))
}

pub fn validate_archive_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid archive name")
    }
    Ok(())
}

pub fn tree_digest(files: &[Artifact]) -> String {
    let mut files = files.to_vec();
    files.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    digest_bytes(&serde_json::to_vec(&files).unwrap_or_default())
}

pub fn declared_action_key(
    name: &str,
    platform: &PlatformIdentity,
    command: &[String],
    environment: &BTreeMap<String, String>,
    inputs: &[Artifact],
    outputs: &[String],
) -> String {
    let mut inputs = inputs.to_vec();
    inputs.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    let mut outputs = outputs.to_vec();
    outputs.sort();
    digest_bytes(
        &serde_json::to_vec(&(
            PROTOCOL_VERSION,
            name,
            platform,
            command,
            environment,
            inputs,
            outputs,
        ))
        .unwrap_or_default(),
    )
}

pub fn validate_declared_command(command: &[String]) -> Result<()> {
    if command.len() > 4096 || command.iter().any(|argument| argument.len() > 64 * 1024) {
        bail!("declared command exceeds size limits")
    }
    let program = command.first().context("declared command is empty")?;
    if !matches!(program.as_str(), "cargo" | "rustc") {
        bail!("cacheable declared command must use bare cargo or rustc")
    }
    if program == "cargo"
        && (!command.iter().any(|argument| argument == "--locked")
            || !command.iter().any(|argument| argument == "--offline"))
    {
        bail!("cacheable Cargo commands require --locked --offline")
    }
    for argument in &command[1..] {
        let payload = argument
            .split_once('=')
            .map_or(argument.as_str(), |(_, value)| value);
        let windows_absolute =
            payload.as_bytes().get(1) == Some(&b':') || payload.starts_with("\\\\");
        if Path::new(payload).is_absolute()
            || payload.starts_with(['/', '\\'])
            || windows_absolute
            || payload.split(['/', '\\']).any(|segment| segment == "..")
        {
            bail!("declared command argument escapes the sandbox: {argument}")
        }
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        now_ms(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => sync_parent(path),
        Err(error) if path.exists() => {
            let _ = fs::remove_file(&tmp);
            match fs::read(path) {
                Ok(existing) if existing == bytes => Ok(()),
                _ => Err(error.into()),
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error.into())
        }
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().context("destination has no parent")?;
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct PathNormalizer {
    bases: Vec<(String, String)>,
}

impl PathNormalizer {
    pub fn new(mut bases: Vec<(String, PathBuf)>) -> Self {
        let mut rendered = Vec::new();
        for (token, path) in bases.drain(..) {
            let value = path.to_string_lossy().trim_end_matches('/').to_owned();
            if !value.is_empty() && !rendered.iter().any(|(_, p)| p == &value) {
                rendered.push((token.clone(), value.clone()));
                // canonicalize() adds a verbatim prefix on Windows, while
                // Cargo's arguments and environment use ordinary drive paths.
                // Recognize both spellings without changing literal arguments.
                #[cfg(windows)]
                if let Some(ordinary) = value.strip_prefix(r"\\?\") {
                    let ordinary = if let Some(unc) = ordinary.strip_prefix(r"UNC\") {
                        format!(r"\\{unc}")
                    } else {
                        ordinary.to_owned()
                    };
                    rendered.push((token.clone(), ordinary.replace('\\', "/")));
                    rendered.push((token, ordinary));
                }
            }
        }
        rendered.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        Self { bases: rendered }
    }

    /// Every recognized spelling of the directory a token stands for.
    pub fn spellings(&self, token: &str) -> Vec<String> {
        self.bases
            .iter()
            .filter(|(candidate, _)| candidate == token)
            .map(|(_, path)| path.clone())
            .collect()
    }

    pub fn normalize(&self, value: &str) -> String {
        self.bases
            .iter()
            .fold(value.to_owned(), |text, (token, path)| {
                text.replace(path, token)
            })
    }

    pub fn localize(&self, value: &str) -> String {
        self.localizations()
            .into_iter()
            .fold(value.to_owned(), |text, (token, path)| {
                text.replace(token, path)
            })
    }

    /// The one spelling each token localizes to: the ordinary drive path a
    /// fresh process sees, never a Windows verbatim (`\\?\`) spelling. A
    /// restored build-script directive such as `rustc-link-search` becomes a
    /// later rustc argument, so it must be spelled as the script printed it.
    fn localizations(&self) -> Vec<(&str, &str)> {
        let mut chosen: Vec<(&str, &str)> = Vec::new();
        for (token, path) in &self.bases {
            let (token, path) = (token.as_str(), path.as_str());
            if let Some(entry) = chosen.iter_mut().find(|entry| entry.0 == token) {
                if entry.1.starts_with(r"\\?\") && !path.starts_with(r"\\?\") && !path.contains('/')
                {
                    entry.1 = path;
                }
            } else {
                chosen.push((token, path));
            }
        }
        chosen
    }

    pub fn normalize_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        replace_bytes(
            bytes,
            &self
                .bases
                .iter()
                .map(|(t, p)| (p.as_bytes(), t.as_bytes()))
                .collect::<Vec<_>>(),
        )
    }

    pub fn localize_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        replace_bytes(
            bytes,
            &self
                .localizations()
                .into_iter()
                .map(|(t, p)| (t.as_bytes(), p.as_bytes()))
                .collect::<Vec<_>>(),
        )
    }
}

fn replace_bytes(input: &[u8], replacements: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut value = input.to_vec();
    for (from, to) in replacements {
        if from.is_empty() {
            continue;
        }
        let mut output = Vec::with_capacity(value.len());
        let mut offset = 0;
        while let Some(pos) = value[offset..].windows(from.len()).position(|w| w == *from) {
            let absolute = offset + pos;
            output.extend_from_slice(&value[offset..absolute]);
            output.extend_from_slice(to);
            offset = absolute + from.len();
        }
        output.extend_from_slice(&value[offset..]);
        value = output;
    }
    value
}

pub fn parse_dep_info(text: &str) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut files = BTreeSet::new();
    let mut env = BTreeSet::new();
    let logical = text.replace("\\\r\n", "").replace("\\\n", "");
    for line in logical.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let (name, value) = rest
                .split_once('=')
                .map_or((rest, None), |(name, value)| (name, Some(value.to_owned())));
            env.insert((name.to_owned(), value));
            continue;
        }
        let Some((_, dependencies)) = line.split_once(": ") else {
            continue;
        };
        for word in split_makefile_words(dependencies) {
            if !word.is_empty() {
                files.insert(word);
            }
        }
    }
    (files.into_iter().collect(), env.into_iter().collect())
}

/// Rewrite filenames without losing rustc's Make space escapes or changing
/// environment dependency comments (whose values require exact equality).
pub fn rewrite_dep_info(text: &str, rewrite: impl Fn(&str) -> String) -> String {
    let logical = text.replace("\\\r\n", "").replace("\\\n", "");
    let mut result = String::new();
    for line in logical.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let rewrite_words = |words: &str| {
            split_makefile_words(words)
                .iter()
                .map(|word| rewrite(word).replace(' ', "\\ "))
                .collect::<Vec<_>>()
                .join(" ")
        };
        if body.starts_with('#') {
            result.push_str(body);
        } else if let Some((targets, dependencies)) = body.split_once(": ") {
            result.push_str(&rewrite_words(targets));
            result.push_str(": ");
            result.push_str(&rewrite_words(dependencies));
        } else if let Some(target) = body.strip_suffix(':') {
            result.push_str(&rewrite_words(target));
            result.push(':');
        } else {
            result.push_str(body);
        }
        if line.ends_with("\r\n") {
            result.push_str("\r\n");
        } else if line.ends_with('\n') {
            result.push('\n');
        }
    }
    result
}

fn split_makefile_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        // rustc's escape_dep_filename escapes only spaces. Other backslashes
        // are literal, including Windows drive paths and UNC prefixes.
        if ch == '\\' && chars.peek() == Some(&' ') {
            current.push(chars.next().unwrap());
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_candidate() -> ActionCandidate {
        let static_key = digest_bytes(b"static");
        let files = vec![FileInput {
            path: "$WORKSPACE/src/lib.rs".into(),
            digest: digest_bytes(b"source"),
        }];
        let env = vec![EnvInput::capture("MODE", Some("release"))];
        ActionCandidate {
            protocol: PROTOCOL_VERSION,
            action_key: compiler_action_key(&static_key, &files, &[], &env),
            static_key,
            crate_name: "fixture".into(),
            created_ms: 1,
            files,
            host_files: vec![],
            env,
            artifacts: vec![Artifact {
                file_name: "libfixture-abc.rmeta".into(),
                digest: digest_bytes(b"metadata"),
                executable: false,
            }],
            stdout: StreamArtifact {
                digest: digest_bytes(b"stdout"),
                len: 6,
            },
            stderr: StreamArtifact {
                digest: digest_bytes(b"stderr"),
                len: 6,
            },
            proc_macros: vec![],
        }
    }

    #[cfg(windows)]
    #[test]
    fn localizes_to_the_ordinary_drive_spelling() {
        let n = PathNormalizer::new(vec![(
            "$TARGET".into(),
            PathBuf::from(r"\\?\C:\work\target"),
        )]);
        for spelling in [
            r"\\?\C:\work\target\out",
            r"C:\work\target\out",
            "C:/work/target/out",
        ] {
            assert!(n.normalize(spelling).starts_with("$TARGET"), "{spelling}");
        }
        assert_eq!(n.localize(r"$TARGET\out"), r"C:\work\target\out");
        assert_eq!(
            n.localize_bytes(br"-L native=$TARGET\out"),
            br"-L native=C:\work\target\out"
        );
    }

    #[test]
    fn normalizes_longest_base_first_and_round_trips() {
        let n = PathNormalizer::new(vec![
            ("$HOME".into(), PathBuf::from("/home/alice")),
            ("$WORKSPACE".into(), PathBuf::from("/home/alice/code/app")),
        ]);
        let normalized = n.normalize("/home/alice/code/app/src/lib.rs:/home/alice/.cargo");
        assert_eq!(normalized, "$WORKSPACE/src/lib.rs:$HOME/.cargo");
        assert_eq!(
            n.localize(&normalized),
            "/home/alice/code/app/src/lib.rs:/home/alice/.cargo"
        );
    }

    #[test]
    fn parses_files_and_environment_from_dep_info() {
        let dep = "target/foo.rlib: src/lib.rs src/a\\ b.rs \\\n src/nested.rs\n\n# env-dep:MODE=fast\n# env-dep:OPTIONAL\n";
        let (files, env) = parse_dep_info(dep);
        assert_eq!(files, vec!["src/a b.rs", "src/lib.rs", "src/nested.rs"]);
        assert_eq!(
            env,
            vec![
                ("MODE".into(), Some("fast".into())),
                ("OPTIONAL".into(), None)
            ]
        );
    }

    #[test]
    fn dep_info_preserves_windows_backslashes_and_escaped_spaces() {
        let dep = concat!(
            "C:\\build\\foo.rlib: C:\\repo\\src\\lib.rs C:\\my\\ project\\a.rs \\\r\n",
            " \\\\server\\share\\source.rs \\\\?\\C:\\long\\source.rs\r\n",
        );
        let (files, _) = parse_dep_info(dep);
        assert_eq!(
            files,
            vec![
                r"C:\my project\a.rs",
                r"C:\repo\src\lib.rs",
                r"\\?\C:\long\source.rs",
                r"\\server\share\source.rs",
            ]
        );
    }

    #[test]
    fn store_rejects_corrupt_blob() {
        let root = std::env::temp_dir().join(format!("bellows-core-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let digest = digest_bytes(b"good");
        store.put_blob(&digest, b"good").unwrap();
        fs::write(store.blob_path(&digest).unwrap(), b"bad").unwrap();
        assert!(
            store
                .read_blob(&digest)
                .unwrap_err()
                .to_string()
                .contains("corrupt")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_archive_traversal_paths() {
        assert!(validate_relative_path("bin/test").is_ok());
        assert!(validate_relative_path("../escape").is_err());
        assert!(validate_relative_path("/absolute").is_err());
        assert!(validate_relative_path("a/./b").is_err());
    }

    #[test]
    fn normalized_windows_paths_keep_the_same_traversal_boundary() {
        assert!(validate_normalized_input_path(r"$WORKSPACE\src\lib.rs").is_ok());
        assert!(validate_normalized_input_path(r"$WORKSPACE\..\escape.rs").is_err());
        assert!(validate_normalized_input_path(r"C:\outside\lib.rs").is_err());
        assert!(validate_normalized_input_path(r"\\server\share\lib.rs").is_err());
    }

    #[test]
    fn declared_root_coverage_uses_components_across_path_separators() {
        assert!(relative_path_is_within(
            r"output\release\app.exe",
            "output/release"
        ));
        assert!(relative_path_is_within(
            "output/release/app.exe",
            r"output\release"
        ));
        assert!(relative_path_is_within("output", "output"));
        assert!(!relative_path_is_within("output-other/app.exe", "output"));
        assert!(!relative_path_is_within("other/output/app.exe", "output"));
    }

    #[test]
    fn archive_names_are_publish_once() {
        let root = std::env::temp_dir().join(format!("bellows-archive-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let first = ArchiveManifest {
            protocol: PROTOCOL_VERSION,
            name: "tests-v1".into(),
            tree_digest: tree_digest(&[]),
            created_ms: 1,
            producer_action: None,
            files: vec![],
        };
        assert!(store.put_archive(&first).unwrap());
        assert!(!store.put_archive(&first).unwrap());
        let mut second = first.clone();
        second.files.push(Artifact {
            file_name: "two".into(),
            digest: digest_bytes(b"two"),
            executable: false,
        });
        second.tree_digest = tree_digest(&second.files);
        assert!(store.put_archive(&second).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn declared_keys_ignore_manifest_order() {
        let platform = PlatformIdentity {
            rustc: "rustc test".into(),
            cargo: "cargo test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            path_digest: digest_bytes(b"path"),
            rustup_home_digest: digest_bytes(b"rustup"),
            rustup_toolchain: None,
            host_cc: None,
            host_ld: None,
            host_ar: None,
        };
        let first = Artifact {
            file_name: "a".into(),
            digest: digest_bytes(b"a"),
            executable: false,
        };
        let second = Artifact {
            file_name: "b".into(),
            digest: digest_bytes(b"b"),
            executable: false,
        };
        let one = declared_action_key(
            "demo",
            &platform,
            &["cargo".into()],
            &BTreeMap::new(),
            &[first.clone(), second.clone()],
            &["out/a".into(), "out/b".into()],
        );
        let two = declared_action_key(
            "demo",
            &platform,
            &["cargo".into()],
            &BTreeMap::new(),
            &[second, first],
            &["out/b".into(), "out/a".into()],
        );
        assert_eq!(one, two);
    }

    #[test]
    fn gc_removes_records_before_referenced_blobs() {
        let root = std::env::temp_dir().join(format!("bellows-gc-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let bytes = b"archive payload";
        let digest = digest_bytes(bytes);
        store.put_blob(&digest, bytes).unwrap();
        store
            .put_archive(&ArchiveManifest {
                protocol: PROTOCOL_VERSION,
                name: "gc-test".into(),
                tree_digest: tree_digest(&[Artifact {
                    file_name: "payload".into(),
                    digest: digest.clone(),
                    executable: false,
                }]),
                created_ms: now_ms(),
                producer_action: None,
                files: vec![Artifact {
                    file_name: "payload".into(),
                    digest: digest.clone(),
                    executable: false,
                }],
            })
            .unwrap();
        // Blobs touched in the last second are kept for a racing publication.
        fs::OpenOptions::new()
            .append(true)
            .open(store.blob_path(&digest).unwrap())
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .unwrap();
        let report = store.gc(0).unwrap();
        assert_eq!(report.bytes_after, 0);
        assert!(store.read_archive("gc-test").unwrap().is_none());
        assert!(store.read_blob(&digest).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn declared_commands_cannot_escape_the_sandbox() {
        assert!(
            validate_declared_command(&[
                "cargo".into(),
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path=fixture/Cargo.toml".into(),
            ])
            .is_ok()
        );
        for command in [
            vec![
                "cargo".into(),
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path=../../Cargo.toml".into(),
            ],
            vec!["rustc".into(), "/tmp/ambient.rs".into()],
            vec!["rustc".into(), r"\rooted\ambient.rs".into()],
            vec!["rustc".into(), r"C:ambient.rs".into()],
            vec!["/tmp/cargo".into(), "--locked".into(), "--offline".into()],
        ] {
            assert!(validate_declared_command(&command).is_err());
        }
    }

    #[test]
    fn compiler_action_key_is_stable_and_manifest_sensitive() {
        let static_key = digest_bytes(b"static identity");
        let files = vec![FileInput {
            path: "$WORKSPACE/src/lib.rs".into(),
            digest: digest_bytes(b"source"),
        }];
        let environment = vec![EnvInput::capture("MODE", Some("release"))];
        let key = compiler_action_key(&static_key, &files, &[], &environment);
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            compiler_action_key(&static_key, &files, &[], &environment)
        );
        assert_ne!(
            key,
            compiler_action_key(&digest_bytes(b"other"), &files, &[], &environment)
        );
    }

    /// Every stored key and blob address is a BLAKE3 digest. A dependency
    /// update that changed its output would silently re-key every store.
    #[test]
    fn digests_match_the_blake3_reference_vectors() {
        assert_eq!(
            digest_bytes(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            digest_bytes(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn content_keys_are_canonical_blake3_hex() {
        assert!(validate_content_key(&digest_bytes(b"valid")).is_ok());
        assert!(validate_content_key("abc12345").is_err());
        assert!(validate_content_key(&"A".repeat(CONTENT_KEY_LEN)).is_err());
        assert!(validate_content_key(&"g".repeat(CONTENT_KEY_LEN)).is_err());
    }

    #[test]
    fn candidate_manifest_rejects_poisoning_shapes() {
        assert!(validate_candidate_manifest(&valid_candidate()).is_ok());

        let mut wrong_key = valid_candidate();
        wrong_key.action_key = digest_bytes(b"forged");
        assert!(validate_candidate_manifest(&wrong_key).is_err());

        let mut duplicate = valid_candidate();
        duplicate.files.push(duplicate.files[0].clone());
        duplicate.action_key = compiler_action_key(
            &duplicate.static_key,
            &duplicate.files,
            &duplicate.host_files,
            &duplicate.env,
        );
        assert!(validate_candidate_manifest(&duplicate).is_err());

        let mut escaped = valid_candidate();
        escaped.files[0].path = "$WORKSPACE/../../etc/passwd".into();
        escaped.action_key = compiler_action_key(
            &escaped.static_key,
            &escaped.files,
            &escaped.host_files,
            &escaped.env,
        );
        assert!(validate_candidate_manifest(&escaped).is_err());

        let mut absolute = valid_candidate();
        absolute.files[0].path = "/etc/passwd".into();
        absolute.action_key = compiler_action_key(
            &absolute.static_key,
            &absolute.files,
            &absolute.host_files,
            &absolute.env,
        );
        assert!(validate_candidate_manifest(&absolute).is_err());
    }

    #[test]
    fn host_inputs_are_absolute_and_part_of_the_action_key() {
        for path in [
            "/usr/lib/libasound.so",
            r"C:\Windows\System32\kernel32.lib",
            r"\\server\share\a.lib",
        ] {
            assert!(validate_host_input_path(path).is_ok(), "{path}");
        }
        for path in [
            "relative/lib.a",
            "/usr/../etc/passwd",
            "",
            r"C:relative.lib",
        ] {
            assert!(validate_host_input_path(path).is_err(), "{path}");
        }
        let mut candidate = valid_candidate();
        let before = candidate.action_key.clone();
        candidate.host_files.push(FileInput {
            path: "/usr/lib/libz.so".into(),
            digest: digest_bytes(b"zlib"),
        });
        assert!(validate_candidate_manifest(&candidate).is_err());
        candidate.action_key = compiler_action_key(
            &candidate.static_key,
            &candidate.files,
            &candidate.host_files,
            &candidate.env,
        );
        assert_ne!(candidate.action_key, before);
        assert!(validate_candidate_manifest(&candidate).is_ok());
        candidate.host_files[0].path = "$WORKSPACE/relative".into();
        candidate.action_key = compiler_action_key(
            &candidate.static_key,
            &candidate.files,
            &candidate.host_files,
            &candidate.env,
        );
        assert!(validate_candidate_manifest(&candidate).is_err());
    }

    #[test]
    fn normalized_environment_inputs_change_the_action_key() {
        let static_key = digest_bytes(b"static");
        let literal = vec![EnvInput::capture("OUT_DIR", Some("$TARGET/out"))];
        let normalized = vec![EnvInput::capture_normalized("OUT_DIR", "$TARGET/out")];
        assert_eq!(literal[0].value_digest, normalized[0].value_digest);
        assert_ne!(
            compiler_action_key(&static_key, &[], &[], &literal),
            compiler_action_key(&static_key, &[], &[], &normalized)
        );
        // Version-5 records without the field still decode (as literal).
        let decoded: EnvInput =
            serde_json::from_str(r#"{"name":"A","value_digest":null}"#).unwrap();
        assert!(!decoded.normalized);
    }

    #[test]
    fn corrupt_candidate_indexes_are_quarantined_and_recoverable() {
        let root = std::env::temp_dir().join(format!("bellows-quarantine-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let candidate = valid_candidate();
        store.put_candidate(candidate.clone(), 8).unwrap();
        let path = store.action_path(&candidate.static_key).unwrap();
        fs::write(&path, b"{truncated").unwrap();
        assert!(store.read_candidates(&candidate.static_key).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read_dir(root.join("quarantine")).unwrap().count(), 1);
        store.put_candidate(candidate.clone(), 8).unwrap();
        assert_eq!(
            store
                .read_candidates(&candidate.static_key)
                .unwrap()
                .candidates,
            vec![candidate]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn structurally_invalid_records_are_quarantined_and_recoverable() {
        let root = std::env::temp_dir().join(format!("bellows-validation-quarantine-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let candidate = valid_candidate();
        store.put_candidate(candidate.clone(), 8).unwrap();
        let path = store.action_path(&candidate.static_key).unwrap();
        let mut invalid = candidate.clone();
        invalid.protocol = PROTOCOL_VERSION - 1;
        fs::write(
            &path,
            serde_json::to_vec(&CandidateIndex {
                candidates: vec![invalid],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(store.read_candidates(&candidate.static_key).is_err());
        assert!(!path.exists());
        store.put_candidate(candidate.clone(), 8).unwrap();
        assert_eq!(
            store
                .read_candidates(&candidate.static_key)
                .unwrap()
                .candidates,
            vec![candidate]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_candidate_publication_does_not_lose_records() {
        let root = std::env::temp_dir().join(format!("bellows-concurrency-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut threads = Vec::new();
        for index in 0..16 {
            let store = store.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut candidate = valid_candidate();
                candidate.files[0].digest = digest_bytes(format!("source-{index}").as_bytes());
                candidate.action_key = compiler_action_key(
                    &candidate.static_key,
                    &candidate.files,
                    &candidate.host_files,
                    &candidate.env,
                );
                barrier.wait();
                store.put_candidate(candidate, 32).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let index = store.read_candidates(&digest_bytes(b"static")).unwrap();
        assert_eq!(index.candidates.len(), 16);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gc_aborts_on_malformed_records_without_deleting_blobs() {
        let root = std::env::temp_dir().join(format!("bellows-gc-corrupt-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let bytes = b"live artifact";
        let digest = digest_bytes(bytes);
        store.put_blob(&digest, bytes).unwrap();
        let candidate = valid_candidate();
        store.put_candidate(candidate.clone(), 8).unwrap();
        fs::write(
            store.action_path(&candidate.static_key).unwrap(),
            b"bad json",
        )
        .unwrap();
        assert!(store.gc(0).is_err());
        assert_eq!(store.read_blob(&digest).unwrap(), bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gc_preserves_a_recent_blob_during_record_publication() {
        let root = std::env::temp_dir().join(format!("bellows-gc-upload-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let bytes = b"publication in progress";
        let digest = digest_bytes(bytes);
        store.put_blob(&digest, bytes).unwrap();
        let report = store.gc(0).unwrap();
        assert_eq!(report.blobs_evicted, 0);
        assert_eq!(store.read_blob(&digest).unwrap(), bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gc_ignores_cancellation_stranded_atomic_temp_files() {
        let root = std::env::temp_dir().join(format!("bellows-gc-temp-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let directory = root.join("actions").join("aa");
        fs::create_dir_all(&directory).unwrap();
        let temp = directory.join(".tmp-cancelled-publication");
        fs::write(&temp, b"partial json").unwrap();
        assert!(store.gc(0).is_ok());
        assert!(temp.exists());
        let _ = fs::remove_dir_all(root);
    }
}
