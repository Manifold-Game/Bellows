//! Cache build-script runs.
//!
//! Cargo runs `target/<profile>/build/<pkg>-<hash>/build-script-build`, a
//! hard link to the compiled script. After compiling or restoring a script,
//! the wrapper moves the real binary aside and puts a Bellows launcher in its
//! place. When Cargo runs it, the launcher looks up a verified earlier run of
//! the same script with the same inputs and restores its `OUT_DIR` tree and
//! directives, or runs the real script and records it.
//!
//! Identity follows Cargo's own rerun model, so a restored run is as current
//! as one Cargo would skip in an existing target directory: the script
//! binary, `$RUSTC -vV`, the C/C++ drivers, the relevant environment, the
//! `rerun-if-changed` paths (or the whole package when none are declared)
//! and the `rerun-if-env-changed` values. A directory input counts by its
//! recursive listing as well as its files' contents: Cargo reruns a script
//! when a file appears in or disappears from a watched directory. Other host tools a script runs are
//! part of the trusted toolchain boundary, as for declared actions. Only runs
//! that start from an empty `OUT_DIR` are published.
use super::{
    Identity, NotStored, Remote, compiler_identity, hash_field, local_store, normalizer,
    record_event, relevant_environment, root_bases, state_dir, validate_candidate,
};
use anyhow::{Context, Result, anyhow, bail};
use bellows_core::{
    ActionCandidate, Artifact, EnvInput, FileInput, PIN_PREFIX, PROTOCOL_VERSION, PathNormalizer,
    Store, StreamArtifact, atomic_write, compiler_action_key, digest_bytes, now_ms,
    validate_candidate_manifest, validate_relative_path,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Set while running as a launcher: Cargo captures the script's stderr.
pub static SILENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const RECORD: &str = ".bellows-build-script.json";
const REAL_PREFIX: &str = ".bellows-real-";
const TREE: &str = "out-dir.tree";
/// Environment-input prefix for the listing of a directory that is itself a
/// normalized root (the package directory of a script that declares no
/// inputs, or `rerun-if-changed=.`): a bare `$WORKSPACE` is not a valid
/// input path, so its listing is computed at lookup like a virtual variable.
const LISTING_PREFIX: &str = "BELLOWS_LISTING:";
const TREE_MAGIC: &[u8] = b"BELLOWS-TREE-1\n";
/// Larger trees are regenerated rather than stored.
const MAX_TREE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct LauncherRecord {
    real: String,
}

/// True for `rustc` compiling a build script into Cargo's `build/` directory.
pub fn is_build_script_compile(crate_name: &str, out_dir: &Path) -> bool {
    crate_name.starts_with("build_script_")
        && out_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "build")
}

/// Replace a freshly compiled or restored script with the launcher.
pub fn install_launcher(out_dir: &Path, unit_file: &str) -> Result<()> {
    if env::var("BELLOWS_BUILD_SCRIPTS").as_deref() == Ok("0") {
        return Ok(());
    }
    let script = out_dir.join(unit_file);
    let real_name = format!("{REAL_PREFIX}{unit_file}");
    let real = out_dir.join(&real_name);
    let launcher = env::current_exe()?;
    fs::rename(&script, &real)
        .with_context(|| format!("move build script {}", script.display()))?;
    // Always a private copy, never a hard link: MSVC's link.exe (and linkers
    // such as mold) rewrite an existing output file in place, which through
    // a hard link would overwrite the Bellows executable itself. On btrfs
    // and XFS the copy is a reflink.
    let installed = fs::copy(&launcher, &script)
        .map(|_| ())
        .with_context(|| format!("install build-script launcher {}", script.display()));
    // Cargo decides freshness from the script executable's mtime: keep the
    // one rustc produced, or every dependent build looks stale.
    let installed = installed.and_then(|()| {
        let modified = fs::metadata(&real)?.modified()?;
        fs::File::options()
            .write(true)
            .open(&script)?
            .set_modified(modified)
            .context("preserve build-script mtime")
    });
    if let Err(error) = installed {
        let _ = fs::remove_file(&script);
        let _ = fs::rename(&real, &script);
        return Err(error);
    }
    atomic_write(
        &out_dir.join(RECORD),
        &serde_json::to_vec(&LauncherRecord { real: real_name })?,
    )
}

/// Before rustc writes a build-script executable, remove a previous
/// launcher, its real binary and its record so the new output is a fresh
/// file and a stale record can never name an outdated script.
pub fn remove_launcher(out_dir: &Path, unit_file: &str) {
    if out_dir.join(RECORD).exists() {
        let _ = fs::remove_file(out_dir.join(RECORD));
        let _ = fs::remove_file(out_dir.join(unit_file));
        let _ = fs::remove_file(out_dir.join(format!("{REAL_PREFIX}{unit_file}")));
    }
}

/// The real script, when this process was started by Cargo as a launcher.
pub fn launched_as(args: &[OsString]) -> Option<PathBuf> {
    let program = Path::new(args.first()?);
    let name = program.file_name()?.to_str()?;
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name != "build-script-build" && !name.starts_with("build_script_") {
        return None;
    }
    let dir = program.parent()?;
    let record: LauncherRecord = serde_json::from_slice(&fs::read(dir.join(RECORD)).ok()?).ok()?;
    let real = validate_relative_path(&record.real).ok()?;
    if real.components().count() != 1 || !record.real.starts_with(REAL_PREFIX) {
        return None;
    }
    Some(dir.join(real))
}

pub fn run(real: &Path, args: &[OsString]) -> Result<ExitStatus> {
    SILENT.store(true, std::sync::atomic::Ordering::Relaxed);
    let configured = env::var_os("BELLOWS_STATE_DIR").is_some()
        && env::var("BELLOWS_BUILD_SCRIPTS").as_deref() != Ok("0");
    let (Some(out_dir), true) = (env::var_os("OUT_DIR").map(PathBuf::from), configured) else {
        return Ok(Command::new(real).args(args).status()?);
    };
    let crate_name = format!(
        "build-script:{}",
        env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".into())
    );
    match cached_run(real, args, &out_dir, &crate_name) {
        Ok(status) => Ok(status),
        Err(error) => {
            record_event(
                "fallback",
                &crate_name,
                None,
                None,
                &format!("build-script cache failed; running the script: {error:#}"),
            );
            Ok(Command::new(real).args(args).status()?)
        }
    }
}

struct Stores {
    l1: Option<Store>,
    remote: Option<Remote>,
}

fn stores(workspace: &Path) -> Stores {
    let local_only = env::var("BELLOWS_LOCAL_ONLY").as_deref() == Ok("1");
    let l1 = if env::var("BELLOWS_L1").as_deref() == Ok("0") {
        None
    } else if local_only {
        local_store(Some(&state_dir(workspace)), false).ok()
    } else {
        Store::open_for_access(state_dir(workspace).join(format!("l1-v{PROTOCOL_VERSION}"))).ok()
    };
    let remote = (!local_only)
        .then(|| {
            let server =
                env::var("BELLOWS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
            Remote::new(&server, env::var("BELLOWS_AUTH_TOKEN").ok()).ok()
        })
        .flatten();
    Stores { l1, remote }
}

fn cached_run(
    real: &Path,
    args: &[OsString],
    out_dir: &Path,
    crate_name: &str,
) -> Result<ExitStatus> {
    let mut identity = identity(real, args, out_dir, crate_name)?;
    let stores = stores(&identity.workspace);
    let mut reasons = Vec::new();
    let mut lookups = Vec::new();
    if let Some(Ok(index)) = stores
        .l1
        .as_ref()
        .map(|store| store.read_candidates(&identity.static_key))
    {
        lookups.push((index, false));
    }
    if let Some(Ok(index)) = stores
        .remote
        .as_ref()
        .map(|remote| remote.candidates(&identity.static_key))
    {
        lookups.push((index, true));
    }
    // Root directory listings the candidates were recorded with, as of now.
    for (index, _) in &lookups {
        for input in index.candidates.iter().flat_map(|c| &c.env) {
            if let Some(root) = input.name.strip_prefix(LISTING_PREFIX)
                && !identity.virtual_env.contains_key(&input.name)
            {
                let path = PathBuf::from(identity.normalizer.localize(root));
                if let Ok(digest) = directory_listing_digest(&path) {
                    identity.virtual_env.insert(input.name.clone(), digest);
                }
            }
        }
    }
    for (index, remote) in lookups {
        for candidate in &index.candidates {
            if let Err(reason) = validate_candidate(candidate, &identity) {
                reasons.push(reason);
                continue;
            }
            let fetch = |digests: &[&str]| -> Result<Vec<Vec<u8>>> {
                match (remote, &stores.remote) {
                    (true, Some(client)) => {
                        let blobs = client.blobs(digests)?;
                        if let Some(store) = &stores.l1 {
                            for (digest, bytes) in digests.iter().zip(&blobs) {
                                let _ = store.put_blob(digest, bytes);
                            }
                        }
                        Ok(blobs)
                    }
                    _ => super::read_blobs(stores.l1.as_ref().context("no local cache")?, digests),
                }
            };
            // The remote use is recorded during the restore, not after it.
            let restored = std::thread::scope(|scope| {
                let used = stores
                    .remote
                    .as_ref()
                    .map(|client| scope.spawn(|| client.record_use(candidate)));
                let restored = restore(candidate, &identity, out_dir, fetch);
                if let Some(used) = used {
                    let _ = used.join();
                }
                restored
            });
            match restored {
                Ok(()) => {
                    if remote && let Some(store) = &stores.l1 {
                        let _ = store.put_candidate(candidate.clone(), 8);
                    }
                    if !remote && let Some(store) = &stores.l1 {
                        let _ = store.record_use(&candidate.static_key, &candidate.action_key);
                    }
                    record_event(
                        if remote { "hit" } else { "l1_hit" },
                        crate_name,
                        Some(&identity.static_key),
                        Some(&candidate.action_key),
                        "restored build-script run (OUT_DIR and directives)",
                    );
                    return Ok(super::success_status());
                }
                Err(error) => record_event(
                    "corrupt",
                    crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    &format!("build-script candidate rejected during restore: {error:#}"),
                ),
            }
        }
    }
    let detail = reasons
        .first()
        .map(|reason| format!("build-script run: {reason}"))
        .unwrap_or_else(|| {
            "not cached yet: first build of this crate seen here (build-script run)".into()
        });
    record_event(
        "miss",
        crate_name,
        Some(&identity.static_key),
        None,
        &detail,
    );

    let fresh = tree_is_empty(out_dir)?;
    // A script that declares no inputs is recorded against its whole package
    // directory. Cargo creates `target` there during a fresh checkout's first
    // build, which moves the directory's timestamp without changing its
    // listing; compare that listing instead (see `capture`).
    let package_listing = directory_listing_digest(&identity.workspace).ok();
    let started = std::time::SystemTime::now();
    let mut child = Command::new(real)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start build script")?;
    let stdout = child.stdout.take().context("capture build-script stdout")?;
    let stderr = child.stderr.take().context("capture build-script stderr")?;
    let stdout_thread = std::thread::spawn(move || super::tee(stdout, std::io::stdout()));
    let stderr_thread = std::thread::spawn(move || super::tee(stderr, std::io::stderr()));
    let status = child.wait()?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("stdout relay panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("stderr relay panicked"))??;
    if !status.success() {
        return Ok(status);
    }
    let captured = if !fresh {
        Err(anyhow::Error::new(NotStored(
            "build script ran over an existing OUT_DIR; result stays in this checkout".into(),
        )))
    } else if env::var("BELLOWS_READ_ONLY").as_deref() == Ok("1") {
        return Ok(status);
    } else {
        capture(
            &identity,
            out_dir,
            &stdout,
            &stderr,
            crate_name,
            started,
            package_listing.as_deref(),
        )
    };
    match captured {
        Ok((candidate, blobs, pinned)) => {
            if let Some(store) = &stores.l1 {
                for (digest, bytes) in &blobs {
                    store.put_blob(digest, bytes)?;
                }
                store.put_candidate(candidate.clone(), 8)?;
            }
            if let Some(remote) = &stores.remote {
                remote.put_blobs(blobs)?;
                remote.put_candidate(&candidate)?;
            }
            let scope = if pinned {
                "pinned to this checkout: it embeds the workspace or target path"
            } else {
                "shareable across checkouts"
            };
            record_event(
                "store",
                crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("stored build-script run ({scope})"),
            );
        }
        Err(error) => {
            let (kind, detail) = match error.downcast_ref::<NotStored>() {
                Some(reason) => ("not_stored", format!("ran but not stored: {reason}")),
                None => (
                    "fallback",
                    format!("build-script capture skipped after a successful run: {error:#}"),
                ),
            };
            record_event(kind, crate_name, Some(&identity.static_key), None, &detail);
        }
    }
    Ok(status)
}

fn identity(real: &Path, args: &[OsString], out_dir: &Path, crate_name: &str) -> Result<Identity> {
    // Cargo runs build scripts in the package directory.
    let workspace = env::current_dir()?.canonicalize()?;
    // Cargo runs build scripts in their package directory, inside its checkout.
    let normalizer = normalizer(&workspace, out_dir, None);
    let root_normalizer = PathNormalizer::new(root_bases(&workspace, out_dir, None));
    let digests = super::digests::Digests::new(&state_dir(&workspace));
    let mut hasher = blake3::Hasher::new();
    hash_field(
        &mut hasher,
        "protocol",
        PROTOCOL_VERSION.to_string().as_bytes(),
    );
    // v2: directory inputs record their listing. A v1 record lists only the
    // files that existed when it was made, so it must never be found again.
    hash_field(&mut hasher, "kind", b"build-script-run-v2");
    hash_field(&mut hasher, "script", digests.file(real)?.as_bytes());
    for arg in args {
        hash_field(&mut hasher, "arg", arg.to_string_lossy().as_bytes());
    }
    hash_field(
        &mut hasher,
        "cwd",
        normalizer
            .normalize(&workspace.to_string_lossy())
            .as_bytes(),
    );
    if let Some(rustc) = env::var_os("RUSTC") {
        let compiler = compiler_identity(Path::new(&rustc), &state_dir(&workspace))?;
        hash_field(&mut hasher, "rustc", &compiler.stdout);
    }
    // The C/C++ drivers `cc`-based scripts run, by content.
    let path_env = env::var("PATH").unwrap_or_default();
    for driver in ["CC", "CXX", "AR"]
        .iter()
        .filter_map(|name| env::var(name).ok())
        .chain(["cc", "c++", "ar"].map(str::to_owned))
    {
        if let Ok(resolved) = super::link::resolve_program(&driver, &path_env, &workspace) {
            hash_field(&mut hasher, "tool", driver.as_bytes());
            hash_field(
                &mut hasher,
                "tool-digest",
                digests.file(&resolved)?.as_bytes(),
            );
        }
    }
    for (name, value) in relevant_environment(&normalizer) {
        hash_field(&mut hasher, &format!("env:{name}"), value.as_bytes());
    }
    let static_key = hasher.finalize().to_hex().to_string();
    let mut pins = vec![(
        format!("{PIN_PREFIX}$WORKSPACE"),
        workspace.to_string_lossy().into_owned(),
    )];
    for token in ["$TARGET", "$CHECKOUT"] {
        if let Some(root) = root_normalizer.spellings(token).first() {
            pins.push((format!("{PIN_PREFIX}{token}"), root.clone()));
        }
    }
    let checkout = super::checkout_root(&workspace);
    Ok(Identity {
        static_key: static_key.clone(),
        normalizer,
        root_normalizer,
        workspace,
        fingerprint: super::diagnostics::Fingerprint {
            key: static_key,
            components: BTreeMap::new(),
        },
        diagnostic_group: crate_name.to_owned(),
        digests,
        pins,
        virtual_env: BTreeMap::new(),
        checkout,
        diagnostic_width: None,
    })
}

/// Cargo target directories (marked with `CACHEDIR.TAG`) nested in OUT_DIR
/// are scratch for nested builds, never script outputs.
fn is_cache_directory(path: &Path) -> bool {
    path.join("CACHEDIR.TAG").is_file()
}

fn tree_is_empty(out_dir: &Path) -> Result<bool> {
    let Ok(entries) = fs::read_dir(out_dir) else {
        return Ok(true);
    };
    for entry in entries {
        let path = entry?.path();
        if !(path.is_dir() && is_cache_directory(&path)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn collect_tree(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(anyhow::Error::new(NotStored(format!(
                "OUT_DIR contains a symlink: {}",
                path.display()
            ))));
        }
        if kind.is_dir() {
            if !is_cache_directory(&path) {
                collect_tree(root, &path, files)?;
            }
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
    Ok(())
}

fn pack_tree(out_dir: &Path) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    collect_tree(out_dir, out_dir, &mut files)?;
    files.sort();
    let mut archive = TREE_MAGIC.to_vec();
    for (relative, path) in files {
        let bytes = fs::read(&path)?;
        let header = serde_json::json!({
            "path": relative,
            "len": bytes.len(),
            "executable": super::is_executable(&path)?,
        });
        archive.extend(serde_json::to_vec(&header)?);
        archive.push(b'\n');
        archive.extend(bytes);
        if archive.len() as u64 > MAX_TREE_BYTES {
            return Err(anyhow::Error::new(NotStored(
                "OUT_DIR exceeds the stored tree limit".into(),
            )));
        }
    }
    Ok(archive)
}

fn unpack_tree(archive: &[u8]) -> Result<Vec<(PathBuf, bool, &[u8])>> {
    let mut rest = archive
        .strip_prefix(TREE_MAGIC)
        .context("not a Bellows tree")?;
    let mut files = Vec::new();
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .context("truncated tree header")?;
        let header: serde_json::Value = serde_json::from_slice(&rest[..end])?;
        let path = validate_relative_path(header["path"].as_str().context("tree path")?)?;
        let len = header["len"].as_u64().context("tree length")? as usize;
        let executable = header["executable"].as_bool().unwrap_or(false);
        rest = &rest[end + 1..];
        let bytes = rest.get(..len).context("truncated tree entry")?;
        files.push((path, executable, bytes));
        rest = &rest[len..];
    }
    Ok(files)
}

/// Cargo's rerun model: declared paths, or the whole package otherwise.
fn declared_inputs(stdout: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut environment = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some(directive) = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"))
        else {
            continue;
        };
        if let Some(path) = directive.strip_prefix("rerun-if-changed=") {
            files.push(path.to_owned());
        } else if let Some(name) = directive.strip_prefix("rerun-if-env-changed=") {
            environment.push(name.to_owned());
        }
    }
    (files, environment)
}

/// A directory the input walk skips: build output and VCS metadata.
fn skipped_directory(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "target" || name == ".git")
        || is_cache_directory(path)
}

/// The digest of a directory input's recursive listing (relative path and
/// kind of every entry the input walk visits), so that adding, removing or
/// renaming a file changes the key even though no recorded file changed.
/// Domain-separated from file digests.
pub(crate) fn directory_listing_digest(root: &Path) -> Result<String> {
    fn walk(root: &Path, dir: &Path, entries: &mut Vec<String>) -> Result<()> {
        if skipped_directory(dir) {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let relative = path
                .strip_prefix(root)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() {
                entries.push(format!("d {relative}"));
                walk(root, &path, entries)?;
            } else if kind.is_symlink() {
                entries.push(format!("l {relative}"));
            } else {
                entries.push(format!("f {relative}"));
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort();
    let mut listing = b"bellows-directory-listing-v1\0".to_vec();
    for entry in entries {
        listing.extend_from_slice(entry.as_bytes());
        listing.push(0);
    }
    Ok(digest_bytes(&listing))
}

/// The digest a recorded input path is checked against: a directory's
/// listing, or a file's contents.
pub(crate) fn input_digest(digests: &super::digests::Digests, path: &Path) -> Result<String> {
    if fs::symlink_metadata(path)?.is_dir() {
        directory_listing_digest(path)
    } else {
        digests.file(path)
    }
}

fn walk_inputs(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        if skipped_directory(path) {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            walk_inputs(&entry?.path(), files)?;
        }
    } else if metadata.is_file() {
        files.insert(path.to_path_buf());
    } else if metadata.file_type().is_symlink() {
        let resolved = path.canonicalize()?;
        if resolved.is_file() {
            files.insert(resolved);
        }
    }
    Ok(())
}

type Capture = (ActionCandidate, BTreeMap<String, Vec<u8>>, bool);

fn capture(
    identity: &Identity,
    out_dir: &Path,
    stdout: &[u8],
    stderr: &[u8],
    crate_name: &str,
    started: std::time::SystemTime,
    package_listing: Option<&str>,
) -> Result<Capture> {
    let tree = pack_tree(out_dir)?;
    let scanners = ["$CHECKOUT", "$WORKSPACE", "$TARGET"].map(|token| {
        (
            token,
            super::leak::Scanner::new(&identity.root_normalizer.spellings(token)),
        )
    });
    let mut leaked = BTreeSet::new();
    for (token, scanner) in &scanners {
        if scanner.leaks(&tree) {
            leaked.insert(*token);
        }
    }
    let (declared_files, declared_env) = declared_inputs(stdout);
    let mut paths = BTreeSet::new();
    if declared_files.is_empty() {
        walk_inputs(&identity.workspace, &mut paths)?;
        paths.insert(identity.workspace.clone());
    } else {
        for declared in &declared_files {
            let path = identity.workspace.join(declared);
            if !path.exists() {
                return Err(anyhow::Error::new(NotStored(format!(
                    "rerun-if-changed path {declared} does not exist, so Cargo reruns the script every time"
                ))));
            }
            walk_inputs(&path, &mut paths)?;
            if fs::symlink_metadata(&path)?.is_dir() {
                // The directory itself, by listing: see `directory_listing_digest`.
                paths.insert(path);
            }
        }
    }
    let mut files = Vec::new();
    let mut host_files = Vec::new();
    let mut listings = Vec::new();
    let package = identity
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| identity.workspace.clone());
    for path in paths {
        let absolute = path.canonicalize().unwrap_or(path);
        let normalized = identity.normalizer.normalize(&absolute.to_string_lossy());
        // As for compiler inputs (`changed_during_compile`): an input written
        // while the script ran may not be what it read. A directory's own
        // timestamp moves when an entry is added or removed; the package
        // directory is judged by its listing from before the run instead.
        let produced_by_cargo = ["$TARGET", "$PROFILE"]
            .iter()
            .any(|root| normalized.starts_with(root));
        let changed = if absolute == package && absolute.is_dir() {
            package_listing.is_none_or(|before| {
                directory_listing_digest(&absolute).ok().as_deref() != Some(before)
            })
        } else {
            !produced_by_cargo && super::changed_during_compile(&absolute, started)
        };
        if changed {
            return Err(anyhow::Error::new(NotStored(format!(
                "{} changed while the build script ran",
                absolute.display()
            ))));
        }
        if normalized.starts_with('$')
            && bellows_core::validate_normalized_input_path(&normalized).is_err()
            && fs::symlink_metadata(&absolute)?.is_dir()
        {
            let digest = directory_listing_digest(&absolute)?;
            listings.push(EnvInput::capture(
                format!("{LISTING_PREFIX}{normalized}"),
                Some(&digest),
            ));
            continue;
        }
        if super::outside_checkout_roots(&normalized)
            && super::foreign_checkout(&absolute, identity.checkout.as_deref())
        {
            return Err(anyhow::Error::new(NotStored(format!(
                "reads {} from another checkout",
                absolute.display()
            ))));
        }
        let digest = input_digest(&identity.digests, &absolute)?;
        if bellows_core::validate_normalized_input_path(&normalized).is_ok() {
            files.push(FileInput {
                path: normalized,
                digest,
            });
        } else {
            host_files.push(FileInput {
                path: absolute.to_string_lossy().into_owned(),
                digest,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    host_files.sort_by(|a, b| a.path.cmp(&b.path));
    host_files.dedup_by(|a, b| a.path == b.path);
    let mut env_inputs = declared_env
        .iter()
        .map(|name| match env::var(name) {
            Ok(value) => {
                let normalized = identity.root_normalizer.normalize(&value);
                if normalized != value && leaked.is_empty() {
                    EnvInput::capture_normalized(name, &normalized)
                } else {
                    EnvInput::capture(name, Some(&value))
                }
            }
            Err(_) => EnvInput::capture(name, None),
        })
        .chain(listings)
        .collect::<Vec<_>>();
    env_inputs.extend(
        identity
            .pins
            .iter()
            .filter(|(name, _)| leaked.iter().any(|token| name.ends_with(token)))
            .map(|(name, value)| EnvInput::capture(name, Some(value))),
    );
    env_inputs.sort_by(|a, b| a.name.cmp(&b.name));
    env_inputs.dedup_by(|a, b| a.name == b.name);
    let stdout = identity.normalizer.normalize_bytes(stdout);
    let stderr = identity.normalizer.normalize_bytes(stderr);
    let mut blobs = BTreeMap::new();
    let tree_digest = digest_bytes(&tree);
    blobs.insert(tree_digest.clone(), tree);
    let stdout_digest = digest_bytes(&stdout);
    let stderr_digest = digest_bytes(&stderr);
    let stdout_len = stdout.len() as u64;
    let stderr_len = stderr.len() as u64;
    blobs.insert(stdout_digest.clone(), stdout);
    blobs.insert(stderr_digest.clone(), stderr);
    let candidate = ActionCandidate {
        protocol: PROTOCOL_VERSION,
        static_key: identity.static_key.clone(),
        action_key: compiler_action_key(&identity.static_key, &files, &host_files, &env_inputs),
        crate_name: crate_name.to_owned(),
        created_ms: now_ms(),
        files,
        host_files,
        env: env_inputs,
        artifacts: vec![Artifact {
            file_name: TREE.into(),
            digest: tree_digest,
            executable: false,
        }],
        stdout: StreamArtifact {
            digest: stdout_digest,
            len: stdout_len,
        },
        stderr: StreamArtifact {
            digest: stderr_digest,
            len: stderr_len,
        },
        proc_macros: Vec::new(),
    };
    validate_candidate_manifest(&candidate)?;
    Ok((candidate, blobs, !leaked.is_empty()))
}

fn restore(
    candidate: &ActionCandidate,
    identity: &Identity,
    out_dir: &Path,
    fetch: impl FnOnce(&[&str]) -> Result<Vec<Vec<u8>>>,
) -> Result<()> {
    validate_candidate_manifest(candidate)?;
    let [artifact] = candidate.artifacts.as_slice() else {
        bail!("build-script record must hold exactly one tree")
    };
    if artifact.file_name != TREE {
        bail!("unexpected build-script artifact {}", artifact.file_name)
    }
    let [archive, stdout, stderr]: [Vec<u8>; 3] = fetch(&[
        &artifact.digest,
        &candidate.stdout.digest,
        &candidate.stderr.digest,
    ])?
    .try_into()
    .map_err(|_| anyhow!("blob fetch returned the wrong number of blobs"))?;
    let files = unpack_tree(&archive)?;
    if stdout.len() as u64 != candidate.stdout.len || stderr.len() as u64 != candidate.stderr.len {
        bail!("build-script stream length does not match manifest")
    }
    // Replace the tree, keeping nested Cargo target directories (scratch).
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !entry.file_type()?.is_symlink() {
                if !is_cache_directory(&path) {
                    fs::remove_dir_all(&path)?;
                }
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    fs::create_dir_all(out_dir)?;
    for (relative, executable, bytes) in files {
        let destination = super::safe_destination(out_dir, &relative)?;
        atomic_write(&destination, bytes)?;
        super::set_file_executable(&destination, executable)?;
    }
    std::io::stdout().write_all(&identity.normalizer.localize_bytes(&stdout))?;
    std::io::stderr().write_all(&identity.normalizer.localize_bytes(&stderr))?;
    Ok(())
}

pub fn unit_file(crate_name: &str, extra_filename: &str) -> String {
    format!("{crate_name}{extra_filename}{}", env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_round_trip_and_reject_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        fs::create_dir_all(out.join("nested/deeper")).unwrap();
        fs::create_dir_all(out.join("nested-target")).unwrap();
        fs::write(out.join("nested-target/CACHEDIR.TAG"), "Signature").unwrap();
        fs::write(out.join("nested-target/huge.rlib"), "scratch").unwrap();
        fs::write(out.join("gen.rs"), "pub const A: u8 = 1;").unwrap();
        fs::write(out.join("nested/deeper/lib.a"), [0u8, 1, 2]).unwrap();
        let archive = pack_tree(&out).unwrap();
        let files = unpack_tree(&archive).unwrap();
        let names = files
            .iter()
            .map(|(path, _, _)| path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();
        assert_eq!(names, ["gen.rs", "nested/deeper/lib.a"]);
        assert!(tree_is_empty(&temp.path().join("missing")).unwrap());
        assert!(!tree_is_empty(&out).unwrap());
        fs::remove_file(out.join("gen.rs")).unwrap();
        fs::remove_dir_all(out.join("nested")).unwrap();
        assert!(tree_is_empty(&out).unwrap());

        let mut forged = TREE_MAGIC.to_vec();
        forged.extend(br#"{"path":"../escape","len":1,"executable":false}"#);
        forged.extend(b"\nx");
        assert!(unpack_tree(&forged).is_err());
    }

    #[test]
    fn launchers_are_private_copies_and_removed_before_recompiling() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("build_script_build-abc");
        fs::write(&script, b"real script").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&script)
            .unwrap()
            .set_modified(old)
            .unwrap();
        install_launcher(temp.path(), "build_script_build-abc").unwrap();
        assert_eq!(fs::metadata(&script).unwrap().modified().unwrap(), old);
        // Writing through the installed launcher must not touch this
        // process's executable (an in-place linker write).
        fs::write(&script, b"relinked in place").unwrap();
        assert_ne!(
            fs::read(env::current_exe().unwrap()).unwrap(),
            b"relinked in place"
        );
        assert_eq!(
            fs::read(temp.path().join(".bellows-real-build_script_build-abc")).unwrap(),
            b"real script"
        );
        remove_launcher(temp.path(), "build_script_build-abc");
        assert!(!script.exists());
        assert!(!temp.path().join(RECORD).exists());
        assert!(
            !temp
                .path()
                .join(".bellows-real-build_script_build-abc")
                .exists()
        );
    }

    #[test]
    fn directives_follow_cargos_rerun_model() {
        let (files, env) = declared_inputs(
            b"cargo:rerun-if-changed=build.rs\ncargo::rerun-if-changed=src\ncargo:rerun-if-env-changed=CC\ncargo:rustc-link-lib=z\n",
        );
        assert_eq!(files, ["build.rs", "src"]);
        assert_eq!(env, ["CC"]);
    }

    #[test]
    fn launchers_are_recognized_only_with_their_record() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("build-script-build");
        assert!(launched_as(&[script.clone().into_os_string()]).is_none());
        fs::write(
            temp.path().join(RECORD),
            br#"{"real":".bellows-real-build_script_build-abc"}"#,
        )
        .unwrap();
        assert_eq!(
            launched_as(&[script.into_os_string()]),
            Some(temp.path().join(".bellows-real-build_script_build-abc"))
        );
        fs::write(temp.path().join(RECORD), br#"{"real":"../escape"}"#).unwrap();
        assert!(launched_as(&[temp.path().join("build-script-build").into_os_string()]).is_none());
        assert!(launched_as(&[temp.path().join("bellows").into_os_string()]).is_none());
        assert!(is_build_script_compile(
            "build_script_build",
            Path::new("/t/debug/build/zstd-sys-abc")
        ));
        assert!(!is_build_script_compile(
            "build_script_build",
            Path::new("/t/debug/deps")
        ));
    }
}
