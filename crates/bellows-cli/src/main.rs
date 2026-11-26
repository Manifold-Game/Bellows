use anyhow::{Context, Result, anyhow, bail};
use bellows_core::{
    ActionCandidate, ArchiveManifest, Artifact, CandidateIndex, DeclaredActionRecord, EnvInput,
    Event, ExecuteRequest, ExecuteResponse, FileInput, GcOptions, GcReport, GcRequest,
    HealthResponse, LeaseRequest, LeaseResponse, PIN_PREFIX, PROTOCOL_VERSION, PathNormalizer,
    PinnedCheckout, PlatformIdentity, ServerStats, Store, StreamArtifact, atomic_write,
    compiler_action_key, declared_action_key, digest_bytes, now_ms, parse_dep_info, rustup_home,
    tree_digest, validate_archive_manifest, validate_candidate_manifest, validate_declared_command,
    validate_declared_record, validate_relative_path,
};
use clap::{Args, Parser, Subcommand};
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod archive;
mod build_script;
mod diagnostics;
mod digests;
mod leak;
mod link;
mod relay;
mod restore;
mod terminal;

#[derive(Parser, Debug)]
#[command(name = "bellows", version, about = "Cargo-native remote builds")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run Cargo through the daemonless local cache.
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run an ordinary command with Bellows installed as Cargo's rustc wrapper.
    Run {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Run Cargo-compatible work against a daemonless, durable local cache.
    Local {
        /// Persistent cache directory (defaults to the platform user cache).
        #[arg(long, env = "BELLOWS_STATE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Relay a build's wrappers to a remote server over warm connections,
    /// publishing in the background. `bellows run` starts one itself for a
    /// non-loopback server; start this one when Cargo runs with
    /// RUSTC_WRAPPER set directly (as in CI), then point the wrappers at it.
    Relay {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
        /// Loopback address to listen on (port 0 picks a free port).
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: std::net::SocketAddr,
        /// Write `BELLOWS_SERVER=` and `BELLOWS_AUTH_TOKEN=` lines for the
        /// wrappers here (mode 0600), once the relay is listening.
        #[arg(long)]
        env_file: Option<PathBuf>,
        /// Directory under which accepted blobs wait for upload, in a fresh
        /// subdirectory removed on exit (default: the state directory).
        #[arg(long)]
        spool_dir: Option<PathBuf>,
    },
    /// Check server connectivity and the local toolchain.
    Doctor {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
    },
    /// Show remote cache and local session statistics.
    Stats {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        selection: diagnostics::Selection,
    },
    /// Explain recent misses, bypasses, fallbacks, and integrity failures.
    Explain {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Group decisions by reason, with affected crates and examples.
        #[arg(long)]
        summary: bool,
        #[command(flatten)]
        selection: diagnostics::Selection,
    },
    /// Publish and restore immutable compile-once/test-many trees.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommands,
    },
    /// Run an explicitly declared, sandboxed Cargo/rustc action locally.
    Action {
        #[command(subcommand)]
        command: ActionCommands,
    },
    /// Execute a declared Cargo/rustc action on bellowsd.
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },
    /// Capture and compare advisory compiler-aware workspace snapshots.
    Analyze {
        #[command(subcommand)]
        command: AnalyzeCommands,
    },
    /// Run a quiescent, reference-aware remote cache collection.
    Gc {
        /// Evict least recently used records until blobs fit in this budget.
        #[arg(long)]
        max_mb: u64,
        /// Report what a collection would evict without changing the store.
        #[arg(long)]
        dry_run: bool,
        /// Also evict every compiler record older than this protocol.
        #[arg(long)]
        min_protocol: Option<u32>,
        /// Give final executables (binaries, test harnesses, examples) their
        /// own budget, evicted least recently used first, so libraries,
        /// procedural macros and build scripts never make room for them.
        #[arg(long)]
        max_executable_mb: Option<u64>,
        /// Keep records pinned to a checkout that no longer exists until the
        /// budget reaches them. A local collection otherwise evicts them
        /// whatever the budget (also set by BELLOWS_KEEP_MISSING_CHECKOUTS=1).
        #[arg(long)]
        keep_missing_checkouts: bool,
        /// Print the full report as JSON.
        #[arg(long)]
        json: bool,
        /// Collect the daemonless local cache instead of a server.
        #[arg(long)]
        local: bool,
        #[arg(long, requires = "local")]
        cache_dir: Option<PathBuf>,
        /// Collect a store directory directly, such as a stopped server's
        /// data directory. A running server's directory accepts only
        /// `--dry-run`; collect it through `--server` instead.
        #[arg(long, conflicts_with = "local")]
        store_dir: Option<PathBuf>,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
}

#[derive(Subcommand, Debug)]
enum ArchiveCommands {
    Publish {
        name: String,
        path: PathBuf,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
    Restore {
        name: String,
        path: PathBuf,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
}

#[derive(Subcommand, Debug)]
enum ActionCommands {
    Run(DeclaredRunArgs),
}

#[derive(Subcommand, Debug)]
enum RemoteCommands {
    Run(DeclaredRunArgs),
}

#[derive(Subcommand, Debug)]
enum AnalyzeCommands {
    Snapshot {
        name: String,
    },
    Compare {
        before: String,
        after: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug, Clone)]
struct ConnectionArgs {
    #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
    server: String,
    #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
    token: Option<String>,
}

#[derive(Args, Debug)]
struct DeclaredRunArgs {
    /// Use the daemonless local cache instead of bellowsd.
    #[arg(long)]
    local: bool,
    #[arg(long, requires = "local")]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    name: String,
    #[arg(long = "input", required = true)]
    inputs: Vec<PathBuf>,
    #[arg(long = "output", required = true)]
    outputs: Vec<PathBuf>,
    #[arg(long = "env")]
    environment: Vec<String>,
    #[command(flatten)]
    connection: ConnectionArgs,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let result = if env::var_os(bellows_core::execution::REMAP_ENV).is_some() {
        bellows_core::execution::remap_compiler(&args[1..]).map(|status| status.code().unwrap_or(1))
    } else if let Some(real) = build_script::launched_as(&args) {
        build_script::run(&real, &args[1..]).map(|status| status.code().unwrap_or(1))
    } else if is_wrapper_invocation(&args) {
        rustc_wrapper(&args[1..]).map(|status| status.code().unwrap_or(1))
    } else {
        run_cli()
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(1, 255) as u8),
        Err(error) => {
            eprintln!(
                "{}",
                terminal::error(terminal::stderr_color(), &format!("{error:#}"))
            );
            ExitCode::FAILURE
        }
    }
}

fn is_wrapper_invocation(args: &[OsString]) -> bool {
    if args.len() < 2 {
        return false;
    }
    let name = Path::new(&args[1])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(name);
    name == "rustc" || name.starts_with("rustc-") || name == "clippy-driver"
}

fn run_cli() -> Result<i32> {
    match Cli::parse().command {
        Commands::Cargo { arguments } => {
            let command = std::iter::once(OsString::from("cargo"))
                .chain(arguments)
                .collect();
            run_local_command(None, command)
        }
        Commands::Run {
            server,
            token,
            command,
        } => run_command(server, token, command),
        Commands::Local { cache_dir, command } => run_local_command(cache_dir, command),
        Commands::Relay {
            server,
            token,
            listen,
            env_file,
            spool_dir,
        } => run_relay(server, token, listen, env_file, spool_dir),
        Commands::Doctor { server, token } => {
            doctor(&server, token.as_deref())?;
            Ok(0)
        }
        Commands::Stats {
            server,
            token,
            json,
            selection,
        } => {
            if selection.local {
                show_local_stats(&selection, json)?;
            } else {
                show_stats(&server, token.as_deref(), &selection, json)?;
            }
            Ok(0)
        }
        Commands::Explain {
            limit,
            json,
            summary,
            selection,
        } => {
            explain(&selection, limit, json, summary)?;
            Ok(0)
        }
        Commands::Archive { command } => {
            run_archive(command)?;
            Ok(0)
        }
        Commands::Action { command } => {
            match command {
                ActionCommands::Run(args) => run_declared_action(args, false)?,
            }
            Ok(0)
        }
        Commands::Remote { command } => {
            match command {
                RemoteCommands::Run(args) => {
                    if args.local {
                        bail!("bellows remote run cannot be combined with --local")
                    }
                    run_declared_action(args, true)?
                }
            }
            Ok(0)
        }
        Commands::Analyze { command } => {
            run_analysis(command)?;
            Ok(0)
        }
        Commands::Gc {
            max_mb,
            dry_run,
            min_protocol,
            max_executable_mb,
            keep_missing_checkouts,
            json,
            local,
            cache_dir,
            store_dir,
            connection,
        } => {
            let options = GcOptions {
                max_bytes: max_mb.saturating_mul(1024 * 1024),
                dry_run,
                min_protocol,
                max_executable_bytes: max_executable_mb.map(|mb| mb.saturating_mul(1024 * 1024)),
                // Only a store on this machine names checkouts that can be
                // looked for here; a server never does (see `PinnedCheckout`).
                evict_missing_checkouts: (local || store_dir.is_some())
                    && !(keep_missing_checkouts || keep_missing_checkouts_from_env()),
            };
            let report = if let Some(directory) = store_dir {
                collect_store_directory(&directory, &options)?
            } else if local {
                // A dry run only reads: no maintenance on open.
                local_store(cache_dir.as_deref(), !dry_run)?.collect_locally(&options)?
            } else {
                let report = Remote::new(&connection.server, connection.token)?.gc(&GcRequest {
                    max_bytes: options.max_bytes,
                    dry_run,
                    min_protocol,
                    max_executable_bytes: options.max_executable_bytes,
                })?;
                // An older server ignores the field and applies only --max-mb.
                if options.max_executable_bytes.is_some() && report.max_executable_bytes.is_none() {
                    bail!("this server does not support --max-executable-mb; upgrade bellowsd")
                }
                report
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_gc_report(&report);
            }
            Ok(0)
        }
    }
}

/// Opens a store directory for collection. A directory a running server owns
/// is only read: the server's own collection coordinates with its uploads.
fn collect_store_directory(directory: &Path, options: &GcOptions) -> Result<GcReport> {
    if !directory.join("blobs").is_dir() || !directory.join("actions").is_dir() {
        bail!(
            "{} is not a Bellows store (no blobs/ and actions/)",
            directory.display()
        )
    }
    let server_lock = directory.join(".server.lock");
    let served = server_lock.exists()
        && fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&server_lock)
            .map(|file| {
                let locked = fs2::FileExt::try_lock_exclusive(&file).is_err();
                if !locked {
                    let _ = fs2::FileExt::unlock(&file);
                }
                locked
            })
            .unwrap_or(true);
    if served && !options.dry_run {
        bail!(
            "{} is owned by a running bellowsd; collect it with --server, or pass --dry-run",
            directory.display()
        )
    }
    let store = if options.dry_run {
        Store::open_for_access(directory)?
    } else {
        Store::open(directory)?
    };
    store.collect_locally(options)
}

/// How many deleted checkouts a text report lists by path.
const DELETED_CHECKOUTS_SHOWN: usize = 20;

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Whether collections on this machine keep records of deleted checkouts.
fn keep_missing_checkouts_from_env() -> bool {
    env::var("BELLOWS_KEEP_MISSING_CHECKOUTS").as_deref() == Ok("1")
}

fn print_gc_report(report: &GcReport) {
    let color = terminal::stdout_color();
    let verb = if report.dry_run {
        "would evict"
    } else {
        "evicted"
    };
    println!(
        "{}",
        terminal::status(
            color,
            "gc",
            &format!(
                "{} → {}{}",
                human_bytes(report.bytes_before),
                human_bytes(report.bytes_after),
                if report.dry_run { " (dry run)" } else { "" }
            ),
            &format!(
                "{verb} {} of {} records · {} blobs",
                report.records_evicted, report.records, report.blobs_evicted
            ),
        )
    );
    if report.records == 0 && report.blobs == 0 {
        return;
    }
    println!(
        "{}",
        terminal::key_value(color, "budget", human_bytes(report.max_bytes))
    );
    if let Some(budget) = report.max_executable_bytes {
        println!(
            "{}",
            terminal::key_value(color, "executable budget", human_bytes(budget))
        );
    }
    println!(
        "{}",
        terminal::key_value(
            color,
            "freed by eviction",
            human_bytes(report.evicted_bytes)
        )
    );
    if report.deleted_checkout_records > 0 {
        println!(
            "{}",
            terminal::key_value(
                color,
                "deleted checkouts",
                format!(
                    "{verb} {} records ({}) pinned to {}{}",
                    report.deleted_checkout_records,
                    human_bytes(report.deleted_checkout_bytes),
                    plural(report.deleted_checkouts.len(), "deleted checkout"),
                    if report.deleted_checkouts.len() > DELETED_CHECKOUTS_SHOWN {
                        format!("; the {DELETED_CHECKOUTS_SHOWN} largest follow, --json lists all")
                    } else {
                        String::new()
                    }
                )
            )
        );
        for checkout in report
            .deleted_checkouts
            .iter()
            .take(DELETED_CHECKOUTS_SHOWN)
        {
            println!(
                "    {:>10}  {:>7} records  {}",
                human_bytes(checkout.evicted_bytes),
                checkout.records,
                checkout.path
            );
        }
    }
    println!(
        "{}",
        terminal::key_value(
            color,
            "unreferenced",
            format!(
                "{} in {} blobs ({} recent blobs kept for uploads in flight)",
                human_bytes(report.unreferenced_bytes),
                report.unreferenced_blobs,
                report.protected_blobs
            )
        )
    );
    if report.skipped_blobs > 0 {
        println!(
            "{}",
            terminal::key_value(
                color,
                "kept",
                format!("{} blobs reused during collection", report.skipped_blobs)
            )
        );
    }
    let now = now_ms();
    let age = |ms: u64| {
        let hours = now.saturating_sub(ms) / (60 * 60 * 1000);
        if hours < 48 {
            format!("{hours} h ago")
        } else {
            format!("{} days ago", hours / 24)
        }
    };
    if let Some(ms) = report.newest_evicted_ms {
        println!(
            "{}",
            terminal::key_value(color, "newest evicted use", age(ms))
        );
    }
    if let Some(ms) = report.oldest_kept_ms {
        println!("{}", terminal::key_value(color, "oldest kept use", age(ms)));
    }
    println!(
        "{}",
        terminal::key_value(
            color,
            "access journal",
            format!("{} recorded uses", report.journal_entries)
        )
    );
    let mut group = "";
    for bucket in &report.breakdown {
        if bucket.group != group {
            group = bucket.group.as_str();
            println!("{}", terminal::section(color, &format!("By {group}")));
        }
        println!(
            "  {:<32} {:>8} records  {:>10} exclusive  {verb} {:>7} records / {:>10}",
            bucket.label,
            bucket.records,
            human_bytes(bucket.exclusive_bytes),
            bucket.evicted_records,
            human_bytes(bucket.evicted_bytes)
        );
    }
}

fn run_command(server: String, token: Option<String>, command: Vec<OsString>) -> Result<i32> {
    let (program, arguments) = command.split_first().context("missing command")?;
    let budget = Budget::from_env()?;
    let workspace = env::current_dir()?.canonicalize()?;
    let state_dir = state_dir(&workspace);
    if let Err(error) = fs::create_dir_all(&state_dir) {
        return run_without_cache(program, arguments, &error.into());
    }
    // Every compile falls back to plain rustc when the server is down. Say so
    // up front (and again at the end) instead of only in per-crate events.
    let unreachable = Remote::new(&server, token.clone())
        .and_then(|remote| remote.reachable())
        .err()
        .map(|error| unreachable_message(&server, &error));
    if let Some(message) = &unreachable {
        if env::var("BELLOWS_REQUIRE_SERVER").as_deref() == Ok("1") {
            bail!("{message} (BELLOWS_REQUIRE_SERVER=1)")
        }
        eprintln!(
            "{}",
            terminal::attention(terminal::stderr_color(), "server unreachable", message)
        );
    }
    let wrapper = env::current_exe()?.canonicalize()?;
    let relay = if unreachable.is_none() && relay_wanted(&server)? {
        match start_relay(
            &server,
            token.clone(),
            "127.0.0.1:0".parse()?,
            None,
            &state_dir,
        ) {
            Ok(relay) => Some(relay),
            Err(error) => {
                eprintln!(
                    "{}",
                    terminal::warning(
                        terminal::stderr_color(),
                        "relay disabled",
                        &format!("wrappers connect to {server} directly: {error:#}")
                    )
                );
                None
            }
        }
    } else {
        None
    };
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("RUSTC_WRAPPER", &wrapper)
        .env_remove("BELLOWS_LOCAL_ONLY")
        .env("BELLOWS_WORKSPACE", &workspace)
        .env("BELLOWS_STATE_DIR", &state_dir);
    match &relay {
        Some(relay) => {
            child
                .env("BELLOWS_SERVER", &relay.url)
                .env("BELLOWS_AUTH_TOKEN", &relay.token);
        }
        None => {
            child.env("BELLOWS_SERVER", &server);
            if let Some(token) = token {
                child.env("BELLOWS_AUTH_TOKEN", token);
            }
        }
    }
    eprintln!(
        "{}",
        terminal::status(
            terminal::stderr_color(),
            "running",
            "cargo",
            &display_command(program, arguments),
        )
    );
    digests::prune(&state_dir, Duration::from_secs(14 * 24 * 60 * 60));
    let session = diagnostics::BuildSession::start(&state_dir, &workspace);
    session.configure(&mut child);
    let result = child.status().context("start wrapped command");
    if let Some(relay) = relay {
        finish_relay(relay)?;
    }
    // The wrappers' local store, unless they ran without one.
    let cache_line = (env::var("BELLOWS_L1").as_deref() != Ok("0"))
        .then(|| enforce_budget(&state_dir.join(format!("l1-v{PROTOCOL_VERSION}")), budget))
        .flatten();
    session.finish(result.as_ref().ok().and_then(ExitStatus::code).unwrap_or(1));
    print_summary_line(cache_line);
    if let Some(message) = &unreachable {
        eprintln!(
            "{}",
            terminal::attention(terminal::stderr_color(), "server unreachable", message)
        );
    } else {
        // The server answered at the start but stopped during the build (a
        // restart, say): say so once, whatever the output mode.
        let lost = session.remote_fallbacks();
        if lost > 0 {
            eprintln!(
                "{}",
                terminal::attention(
                    terminal::stderr_color(),
                    "server unreachable",
                    &format!(
                        "{server} stopped answering during this build: {lost} compile(s) fell back to \
                         plain, uncached rustc. `bellows doctor` checks it"
                    ),
                )
            );
        }
    }
    Ok(result?.code().unwrap_or(1))
}

/// Whether `bellows run` puts a relay between its wrappers and `server`:
/// `BELLOWS_RELAY=1` or `0` decides, and otherwise any server off this host
/// gets one (a loopback server has no round trip worth hiding).
fn relay_wanted(server: &str) -> Result<bool> {
    match env::var("BELLOWS_RELAY").as_deref() {
        Ok("1") => return Ok(true),
        Ok("0") => return Ok(false),
        Ok(other) if !other.is_empty() => bail!("BELLOWS_RELAY must be 0 or 1, not {other}"),
        _ => {}
    }
    let url = Url::parse(server).context("parse Bellows server URL")?;
    let host = url.host_str().unwrap_or_default();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Ok(match host.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => !host.is_empty() && !host.eq_ignore_ascii_case("localhost"),
    })
}

fn bounded_env(name: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64> {
    let Ok(value) = env::var(name) else {
        return Ok(default);
    };
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("{name} must be a whole number"))?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("{name} must be between {minimum} and {maximum}")
    }
    Ok(parsed)
}

fn start_relay(
    server: &str,
    token: Option<String>,
    listen: std::net::SocketAddr,
    spool_dir: Option<PathBuf>,
    state_dir: &Path,
) -> Result<relay::Relay> {
    if !listen.ip().is_loopback() {
        bail!("the relay listens on loopback only, not {listen}")
    }
    let base = Remote::new(server, token.clone())?.base;
    // A directory of its own, which the relay removes when it finishes.
    let spool_dir = spool_dir
        .unwrap_or_else(|| state_dir.to_path_buf())
        .join(format!("relay-spool-{}-{}", std::process::id(), now_ms()));
    relay::Relay::start(relay::RelayOptions {
        upstream: base,
        upstream_token: token.filter(|value| !value.is_empty()),
        listen,
        spool_dir,
        spool_limit_bytes: bounded_env("BELLOWS_RELAY_SPOOL_MB", 4096, 1, 1 << 20)? * 1024 * 1024,
        upload_concurrency: bounded_env("BELLOWS_RELAY_UPLOADS", 8, 1, 64)? as usize,
        connect_timeout: bounded_timeout(
            "BELLOWS_CONNECT_TIMEOUT_MS",
            env::var("BELLOWS_CONNECT_TIMEOUT_MS").ok().as_deref(),
            2_000,
            100,
            30_000,
        )?,
    })
}

/// Waits for the relay's background uploads and says what they did.
fn finish_relay(relay: relay::Relay) -> Result<()> {
    let drain = Duration::from_secs(bounded_env("BELLOWS_RELAY_DRAIN_SECS", 600, 1, 86_400)?);
    let summary = relay.finish(drain);
    let quiet = summary.blobs_uploaded == 0
        && summary.blobs_present == 0
        && summary.records == 0
        && summary.failures == 0
        && !summary.abandoned;
    if !quiet {
        let color = terminal::stderr_color();
        let line = if summary.failures > 0 || summary.abandoned {
            terminal::warning(color, "relay", &summary.describe())
        } else {
            terminal::status(color, "published", "relay", &summary.describe())
        };
        eprintln!("{line}");
    }
    Ok(())
}

fn run_relay(
    server: String,
    token: Option<String>,
    listen: std::net::SocketAddr,
    env_file: Option<PathBuf>,
    spool_dir: Option<PathBuf>,
) -> Result<i32> {
    let workspace = env::current_dir()?;
    let state_dir = state_dir(&workspace);
    if let Err(error) = Remote::new(&server, token.clone()).and_then(|remote| remote.reachable()) {
        eprintln!(
            "{}",
            terminal::attention(
                terminal::stderr_color(),
                "server unreachable",
                &format!(
                    "{server}: {error:#}; the relay starts anyway and wrappers fall back while it stays down"
                )
            )
        );
    }
    let relay = start_relay(&server, token, listen, spool_dir, &state_dir)?;
    if let Some(path) = &env_file {
        write_private_file(
            path,
            format!(
                "BELLOWS_SERVER={}\nBELLOWS_AUTH_TOKEN={}\n",
                relay.url, relay.token
            )
            .as_bytes(),
        )
        .with_context(|| format!("write relay environment {}", path.display()))?;
    }
    eprintln!(
        "{}",
        terminal::status(
            terminal::stderr_color(),
            "relaying",
            &relay.url,
            &format!("to {server}; stop with SIGTERM or Ctrl-C to drain uploads"),
        )
    );
    relay.block_on(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut terminate) = signal(SignalKind::terminate()) {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
                return;
            }
        }
        let _ = tokio::signal::ctrl_c().await;
    });
    if let Some(path) = &env_file {
        let _ = fs::remove_file(path);
    }
    finish_relay(relay)?;
    Ok(0)
}

/// Writes `bytes` to `path` readable only by this user, replacing it whole.
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::Builder::new()
        .prefix(".bellows-relay-env-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

fn unreachable_message(server: &str, error: &anyhow::Error) -> String {
    format!(
        "{server}: {error:#}. Every compile in this build falls back to plain, uncached rustc. Start bellowsd \
         (`bellows doctor` checks it), or use `bellows cargo ...` for the daemonless local cache"
    )
}

fn run_local_command(cache_dir: Option<PathBuf>, command: Vec<OsString>) -> Result<i32> {
    let (program, arguments) = command.split_first().context("missing command")?;
    let budget = Budget::from_env()?;
    let workspace = env::current_dir()?.canonicalize()?;
    let state_dir = match local_state_dir(cache_dir.as_deref()).and_then(|state| {
        local_store(Some(&state), true)?.check_writable()?;
        Ok(state)
    }) {
        Ok(state) => state,
        Err(error) => return run_without_cache(program, arguments, &error),
    };
    let wrapper = env::current_exe()?.canonicalize()?;
    eprintln!(
        "{}",
        terminal::status(
            terminal::stderr_color(),
            "running",
            "local",
            &display_command(program, arguments),
        )
    );
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("BELLOWS_LOCAL_ONLY", "1")
        .env("BELLOWS_WORKSPACE", &workspace)
        .env("BELLOWS_STATE_DIR", &state_dir)
        .env_remove("BELLOWS_SERVER")
        .env_remove("BELLOWS_AUTH_TOKEN");
    digests::prune(&state_dir, Duration::from_secs(14 * 24 * 60 * 60));
    let session = diagnostics::BuildSession::start(&state_dir, &workspace);
    session.configure(&mut child);
    let result = child.status().context("start locally wrapped command");
    let cache_line = enforce_budget(
        &state_dir.join(format!("store-v{PROTOCOL_VERSION}")),
        budget,
    );
    session.finish(result.as_ref().ok().and_then(ExitStatus::code).unwrap_or(1));
    print_summary_line(cache_line);
    Ok(result?.code().unwrap_or(1))
}

const MEBIBYTE: u64 = 1024 * 1024;

/// An automatic collection frees a store down to this share of its budget, so
/// the builds that follow have room before another collection is due.
const COLLECT_TO_PERCENT: u64 = 80;

/// A local store this large with no budget is reported after every build that
/// uses it: nothing else would ever say that it only grows.
const UNBUDGETED_NOTICE_BYTES: u64 = 20 * 1024 * MEBIBYTE;

/// How large a local store may grow before a wrapped build collects it. It
/// applies to each store on its own: the daemonless store that
/// `bellows cargo` and `bellows local` use, and the local store beside a
/// `bellows run` session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Budget {
    /// `BELLOWS_MAX_MB` is not set: only `bellows gc` ever collects.
    Unset,
    /// `BELLOWS_MAX_MB=0`: unbounded on purpose.
    Unbounded,
    Limited {
        max_bytes: u64,
        max_executable_bytes: Option<u64>,
    },
}

/// What a wrapped build does about its store once it has finished.
#[derive(Debug, PartialEq, Eq)]
enum BudgetAction {
    Nothing,
    /// Say that the store is large and nothing bounds it.
    Notice,
    /// Collect least recently used records until the blobs fit `to_bytes`.
    Collect {
        max_bytes: u64,
        to_bytes: u64,
        max_executable_bytes: Option<u64>,
    },
}

impl Budget {
    fn from_env() -> Result<Self> {
        let value = |name: &str| -> Result<Option<String>> {
            match env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(env::VarError::NotPresent) => Ok(None),
                Err(env::VarError::NotUnicode(_)) => {
                    bail!("{name} must be a whole number of mebibytes")
                }
            }
        };
        Self::parse(
            value("BELLOWS_MAX_MB")?.as_deref(),
            value("BELLOWS_MAX_EXECUTABLE_MB")?.as_deref(),
        )
    }

    fn parse(max_mb: Option<&str>, max_executable_mb: Option<&str>) -> Result<Self> {
        let mebibytes = |name: &str, value: Option<&str>| -> Result<Option<u64>> {
            let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
                return Ok(None);
            };
            let parsed = value.parse::<u64>().with_context(|| {
                format!("{name} must be a whole number of mebibytes, not {value:?}")
            })?;
            Ok(Some(parsed.saturating_mul(MEBIBYTE)))
        };
        let max_bytes = mebibytes("BELLOWS_MAX_MB", max_mb)?;
        let max_executable_bytes = mebibytes("BELLOWS_MAX_EXECUTABLE_MB", max_executable_mb)?;
        match max_bytes {
            Some(0) | None if max_executable_bytes.is_some() => {
                bail!(
                    "BELLOWS_MAX_EXECUTABLE_MB needs BELLOWS_MAX_MB: it is a share of that budget"
                )
            }
            None => Ok(Self::Unset),
            Some(0) => Ok(Self::Unbounded),
            Some(max_bytes) => Ok(Self::Limited {
                max_bytes,
                max_executable_bytes,
            }),
        }
    }

    /// What an automatic collection under a budget of `max_bytes` frees to.
    fn collect_to(max_bytes: u64) -> u64 {
        max_bytes / 100 * COLLECT_TO_PERCENT
    }

    fn action(self, stored_bytes: u64) -> BudgetAction {
        match self {
            Self::Unset if stored_bytes >= UNBUDGETED_NOTICE_BYTES => BudgetAction::Notice,
            Self::Unset | Self::Unbounded => BudgetAction::Nothing,
            Self::Limited {
                max_bytes,
                max_executable_bytes,
            } if stored_bytes > max_bytes => BudgetAction::Collect {
                max_bytes,
                to_bytes: Self::collect_to(max_bytes),
                max_executable_bytes,
            },
            Self::Limited { .. } => BudgetAction::Nothing,
        }
    }
}

/// Applies the budget to the store a wrapped build used, once the build has
/// finished, and returns what to say about it in the build summary: nothing
/// when the store is within its budget, or when another build is already
/// collecting it. A collection that fails is reported and never fails the
/// build.
fn enforce_budget(store_root: &Path, budget: Budget) -> Option<String> {
    // A session whose wrappers never ran has no store, and creates none.
    if !store_root.join("blobs").is_dir() {
        return None;
    }
    let color = terminal::stderr_color();
    let failed = |what: &str, error: anyhow::Error| {
        Some(terminal::warning(
            color,
            "cache",
            &format!("{what} for {}: {error:#}", store_root.display()),
        ))
    };
    let store = match Store::open_for_access(store_root) {
        Ok(store) => store,
        Err(error) => return failed("budget not applied", error),
    };
    let stored = match store.blob_bytes() {
        Ok(stored) => stored,
        Err(error) => return failed("budget not applied", error),
    };
    match budget.action(stored) {
        BudgetAction::Nothing => None,
        BudgetAction::Notice => Some(terminal::attention(
            color,
            "cache",
            &format!(
                "{} holds {} and has no budget, so it only grows. Set BELLOWS_MAX_MB (mebibytes) to \
                 have builds collect it, or BELLOWS_MAX_MB=0 to keep it unbounded without this notice",
                store_root.display(),
                human_bytes(stored)
            ),
        )),
        BudgetAction::Collect {
            max_bytes,
            to_bytes,
            max_executable_bytes,
        } => {
            let options = GcOptions {
                max_bytes: to_bytes,
                max_executable_bytes,
                evict_missing_checkouts: !keep_missing_checkouts_from_env(),
                ..GcOptions::default()
            };
            match store.try_collect_locally(&options) {
                Ok(Some(report)) => Some(terminal::status(
                    color,
                    "gc",
                    "cache",
                    &collection_summary(&report, max_bytes),
                )),
                // Another build that finished with this one is collecting.
                Ok(None) => None,
                Err(error) => failed(
                    &format!(
                        "over its {} budget at {}, and the collection failed",
                        human_bytes(max_bytes),
                        human_bytes(stored)
                    ),
                    error,
                ),
            }
        }
    }
}

/// One line for an automatic collection: what the store held and holds now
/// against its budget, and what was evicted to get there.
fn collection_summary(report: &GcReport, max_bytes: u64) -> String {
    let mut line = format!(
        "{} → {} of a {} budget · evicted {} of {} records · {} blobs · {:.1}s",
        human_bytes(report.bytes_before),
        human_bytes(report.bytes_after),
        human_bytes(max_bytes),
        report.records_evicted,
        report.records,
        report.blobs_evicted,
        report.duration_ms as f64 / 1000.0
    );
    if report.deleted_checkout_records > 0 {
        line.push_str(&format!(
            " · {} of them pinned to {}",
            report.deleted_checkout_records,
            plural(report.deleted_checkouts.len(), "deleted checkout")
        ));
    }
    if report.bytes_after > max_bytes {
        line.push_str(
            " · still over budget: blobs that builds in progress are writing are kept for an hour",
        );
    }
    line
}

/// Prints a line that belongs to the build summary, when one is printed.
fn print_summary_line(line: Option<String>) {
    if let Some(line) = line
        && terminal::Output::from_env().prints_build_summary()
    {
        eprintln!("{line}");
    }
}

fn run_without_cache(
    program: &OsStr,
    arguments: &[OsString],
    error: &anyhow::Error,
) -> Result<i32> {
    eprintln!(
        "{}",
        terminal::warning(
            terminal::stderr_color(),
            "cache disabled",
            &format!("cache initialization failed; running command normally: {error:#}")
        )
    );
    let mut child = Command::new(program);
    child.args(arguments);
    // Preserve an unrelated caller-supplied wrapper, but don't re-enter Bellows.
    if env::var_os("RUSTC_WRAPPER").is_some_and(|path| {
        Path::new(&path).canonicalize().ok()
            == env::current_exe().ok().and_then(|p| p.canonicalize().ok())
    }) {
        child.env_remove("RUSTC_WRAPPER");
    }
    Ok(child
        .status()
        .context("start command without cache")?
        .code()
        .unwrap_or(1))
}

fn local_state_dir(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()?.join(path)
        });
    }
    if let Some(path) = env::var_os("BELLOWS_STATE_DIR") {
        return Ok(absolute_path(Path::new(&path), &env::current_dir()?));
    }
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(root).join("bellows"));
    }
    if let Some(root) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(root).join("Bellows"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache/bellows"));
    }
    bail!("cannot locate a user cache directory; pass --cache-dir")
}

fn local_store(cache_dir: Option<&Path>, maintenance: bool) -> Result<Store> {
    let root = local_state_dir(cache_dir)?.join(format!("store-v{PROTOCOL_VERSION}"));
    if maintenance {
        Store::open(root)
    } else {
        Store::open_for_access(root)
    }
}

fn display_command(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|v| v.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone)]
struct Remote {
    base: String,
    token: Option<String>,
    client: Client,
    /// Most blob requests one restore or publication has in flight at once.
    fetch_concurrency: usize,
}

/// Blobs at or below this size are uploaded without asking first whether the
/// server has them: the upload costs about the same round trip as the
/// question, and the server keeps (and refreshes) a copy it already holds.
const UPLOAD_WITHOUT_ASKING_BYTES: usize = 256 * 1024;

fn fetch_concurrency() -> Result<usize> {
    let name = "BELLOWS_FETCH_CONCURRENCY";
    let Ok(value) = env::var(name) else {
        return Ok(16);
    };
    let parsed = value
        .parse::<usize>()
        .with_context(|| format!("{name} must be a whole number"))?;
    if !(1..=64).contains(&parsed) {
        bail!("{name} must be between 1 and 64")
    }
    Ok(parsed)
}

/// Applies `f` to every item on up to `limit` threads, keeping input order.
/// Remote requests are latency-bound, so a restore that needs several blobs
/// waits about one round trip instead of one per blob.
fn parallel_map<T: Sync, R: Send>(items: &[T], limit: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 || limit <= 1 {
        return items.iter().map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots = items
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect::<Vec<_>>();
    thread::scope(|scope| {
        for _ in 0..limit.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(index) else { break };
                    let result = f(item);
                    *slots[index].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every item was mapped")
        })
        .collect()
}

impl Remote {
    fn new(base: &str, token: Option<String>) -> Result<Self> {
        let parsed = Url::parse(base).context("parse Bellows server URL")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!(
                "Bellows server must be an HTTP(S) base URL without credentials, query, or fragment"
            )
        }
        let connect_timeout = bounded_timeout(
            "BELLOWS_CONNECT_TIMEOUT_MS",
            env::var("BELLOWS_CONNECT_TIMEOUT_MS").ok().as_deref(),
            2_000,
            100,
            30_000,
        )?;
        let request_timeout = bounded_timeout(
            "BELLOWS_REQUEST_TIMEOUT_MS",
            env::var("BELLOWS_REQUEST_TIMEOUT_MS").ok().as_deref(),
            120_000,
            1_000,
            600_000,
        )?;
        Ok(Self {
            base: parsed.as_str().trim_end_matches('/').to_owned(),
            token: token.filter(|value| !value.is_empty()),
            // One pooled client per process: requests after the first reuse its
            // connection. Over HTTPS, ALPN picks HTTP/2, so a restore's parallel
            // blob requests share that one connection (and its TLS handshake)
            // instead of opening one each. Plain HTTP stays HTTP/1.1.
            client: Client::builder()
                .connect_timeout(connect_timeout)
                .timeout(request_timeout)
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(90))
                .http2_adaptive_window(true)
                .build()?,
            fetch_concurrency: fetch_concurrency()?,
        })
    }

    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    fn health(&self) -> Result<HealthResponse> {
        Ok(self
            .auth(self.client.get(format!("{}/v1/health", self.base)))
            .send()?
            .error_for_status()?
            .json()?)
    }

    /// A quick liveness probe, bounded well below the cache-traffic timeout.
    fn reachable(&self) -> Result<()> {
        self.auth(self.client.get(format!("{}/v1/health", self.base)))
            .timeout(Duration::from_secs(5))
            .send()?
            .error_for_status()?;
        Ok(())
    }

    fn stats(&self) -> Result<ServerStats> {
        Ok(self
            .auth(self.client.get(format!("{}/v1/stats", self.base)))
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn candidates(&self, static_key: &str) -> Result<CandidateIndex> {
        let response = self
            .auth(
                self.client
                    .get(format!("{}/v1/actions/{static_key}", self.base)),
            )
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(CandidateIndex::default());
        }
        Ok(response.error_for_status()?.json()?)
    }

    fn blob(&self, digest: &str) -> Result<Vec<u8>> {
        let bytes = self
            .auth(self.client.get(format!("{}/v1/blobs/{digest}", self.base)))
            .send()?
            .error_for_status()?
            .bytes()?
            .to_vec();
        let actual = digest_bytes(&bytes);
        if actual != digest {
            bail!("remote blob {digest} failed integrity verification (got {actual})")
        }
        Ok(bytes)
    }

    /// Fetches and verifies several blobs, in request order. Requests run in
    /// parallel; a repeated digest is fetched once, and the empty blob (most
    /// compiler streams) is never requested, since its digest is its proof.
    fn blobs(&self, digests: &[&str]) -> Result<Vec<Vec<u8>>> {
        let empty = digest_bytes(&[]);
        let mut seen = BTreeSet::new();
        let unique = digests
            .iter()
            .copied()
            .filter(|digest| *digest != empty && seen.insert(*digest))
            .collect::<Vec<_>>();
        let fetched = parallel_map(&unique, self.fetch_concurrency, |digest| self.blob(digest));
        let mut by_digest = BTreeMap::new();
        for (digest, result) in unique.iter().zip(fetched) {
            by_digest.insert(*digest, result?);
        }
        let mut remaining = BTreeMap::<&str, usize>::new();
        for digest in digests {
            *remaining.entry(digest).or_default() += 1;
        }
        Ok(digests
            .iter()
            .map(|digest| {
                if *digest == empty {
                    return Vec::new();
                }
                let left = remaining.get_mut(digest).expect("counted above");
                *left -= 1;
                if *left == 0 {
                    by_digest.remove(digest).expect("fetched above")
                } else {
                    by_digest[digest].clone()
                }
            })
            .collect())
    }

    /// Uploads several blobs in parallel; see [`Remote::put_blob`].
    fn put_blobs(&self, blobs: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<()> {
        let blobs = blobs
            .into_iter()
            .map(|blob| std::sync::Mutex::new(Some(blob)))
            .collect::<Vec<_>>();
        for result in parallel_map(&blobs, self.fetch_concurrency, |slot| {
            let (digest, bytes) = slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .expect("each blob is uploaded once");
            self.put_blob(&digest, bytes)
        }) {
            result?;
        }
        Ok(())
    }

    fn put_blob(&self, digest: &str, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > UPLOAD_WITHOUT_ASKING_BYTES {
            let present = self
                .auth(self.client.head(format!("{}/v1/blobs/{digest}", self.base)))
                .send()?;
            if present.status().is_success() {
                return Ok(());
            }
        }
        self.auth(self.client.put(format!("{}/v1/blobs/{digest}", self.base)))
            .body(bytes)
            .send()?
            .error_for_status()?;
        Ok(())
    }

    fn declared(&self, key: &str) -> Result<Option<DeclaredActionRecord>> {
        let response = self
            .auth(self.client.get(format!("{}/v1/declared/{key}", self.base)))
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let record: DeclaredActionRecord = response.error_for_status()?.json()?;
        validate_declared_record(&record)?;
        if record.key != key {
            bail!("remote declared record does not match requested key")
        }
        Ok(Some(record))
    }

    fn put_declared(&self, record: &DeclaredActionRecord) -> Result<()> {
        validate_declared_record(record)?;
        self.auth(
            self.client
                .put(format!("{}/v1/declared/{}", self.base, record.key)),
        )
        .json(record)
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn archive(&self, name: &str) -> Result<Option<ArchiveManifest>> {
        let response = self
            .auth(self.client.get(format!("{}/v1/archives/{name}", self.base)))
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let manifest: ArchiveManifest = response.error_for_status()?.json()?;
        validate_archive_manifest(&manifest)?;
        if manifest.name != name {
            bail!("remote archive does not match requested name")
        }
        Ok(Some(manifest))
    }

    fn put_archive(&self, manifest: &ArchiveManifest) -> Result<()> {
        validate_archive_manifest(manifest)?;
        self.auth(
            self.client
                .put(format!("{}/v1/archives/{}", self.base, manifest.name)),
        )
        .json(manifest)
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn execute(&self, request: &ExecuteRequest) -> Result<ExecuteResponse> {
        Ok(self
            .auth(self.client.post(format!("{}/v1/execute", self.base)))
            .json(request)
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn gc(&self, request: &GcRequest) -> Result<GcReport> {
        Ok(self
            .auth(self.client.post(format!("{}/v1/admin/gc", self.base)))
            // A full collection reads every record; it outlasts the
            // per-request timeout sized for cache traffic.
            .timeout(Duration::from_secs(60 * 60))
            .json(request)
            .send()?
            .error_for_status()?
            .json()?)
    }

    /// Tells the server a candidate was reused, so its collection keeps
    /// recently used records. Best effort: older servers lack the endpoint.
    fn record_use(&self, candidate: &ActionCandidate) {
        let _ = self
            .auth(self.client.post(format!(
                "{}/v1/actions/{}/{}/used",
                self.base, candidate.static_key, candidate.action_key
            )))
            .timeout(Duration::from_secs(2))
            .send();
    }

    fn put_candidate(&self, candidate: &ActionCandidate) -> Result<()> {
        self.auth(self.client.put(format!(
            "{}/v1/actions/{}/{}",
            self.base, candidate.static_key, candidate.action_key
        )))
        .json(&for_server(candidate))
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn acquire(&self, static_key: &str, client_id: &str) -> Result<LeaseResponse> {
        Ok(self
            .auth(
                self.client
                    .post(format!("{}/v1/leases/{static_key}", self.base)),
            )
            .json(&LeaseRequest {
                client_id: client_id.into(),
                ttl_ms: 120_000,
            })
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn release(&self, static_key: &str, token: &str) {
        let _ = self
            .auth(
                self.client
                    .delete(format!("{}/v1/leases/{static_key}/{token}", self.base)),
            )
            .send();
    }
}

fn bounded_timeout(
    name: &str,
    value: Option<&str>,
    default_ms: u64,
    minimum_ms: u64,
    maximum_ms: u64,
) -> Result<Duration> {
    let milliseconds = match value {
        Some(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer number of milliseconds"))?,
        None => default_ms,
    };
    if !(minimum_ms..=maximum_ms).contains(&milliseconds) {
        bail!("{name} must be between {minimum_ms} and {maximum_ms} milliseconds")
    }
    Ok(Duration::from_millis(milliseconds))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    /// rlib/rmeta/dep-info only: rustc never runs a linker.
    Library,
    /// Executables, test harnesses, cdylib/dylib/proc-macro shared objects.
    Linked,
}

impl OutputKind {
    fn name(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Linked => "linked",
        }
    }
}

/// File naming for the compilation target (Cargo omits `--target` for host
/// units such as build scripts and procedural macros).
#[derive(Debug, Clone, Copy)]
struct Naming {
    exe_suffix: &'static str,
    dll_prefix: &'static str,
    dll_suffix: &'static str,
    msvc: bool,
}

impl Naming {
    fn for_target(target: Option<&str>) -> Self {
        let (windows, msvc, apple, wasm) = match target {
            Some(triple) => (
                triple.contains("windows"),
                triple.contains("msvc"),
                triple.contains("apple"),
                triple.starts_with("wasm"),
            ),
            None => (
                cfg!(windows),
                cfg!(target_env = "msvc"),
                cfg!(target_vendor = "apple"),
                false,
            ),
        };
        if wasm {
            Self {
                exe_suffix: ".wasm",
                dll_prefix: "",
                dll_suffix: ".wasm",
                msvc: false,
            }
        } else if windows {
            Self {
                exe_suffix: ".exe",
                dll_prefix: "",
                dll_suffix: ".dll",
                msvc,
            }
        } else if apple {
            Self {
                exe_suffix: "",
                dll_prefix: "lib",
                dll_suffix: ".dylib",
                msvc: false,
            }
        } else {
            Self {
                exe_suffix: "",
                dll_prefix: "lib",
                dll_suffix: ".so",
                msvc: false,
            }
        }
    }
}

/// A `-l` request on a library compile whose file rustc bundles into the rlib.
#[derive(Debug, Clone)]
struct StaticLibrary {
    name: String,
    verbatim: bool,
}

#[derive(Debug)]
struct Invocation {
    rustc: PathBuf,
    args: Vec<String>,
    crate_name: String,
    out_dir: PathBuf,
    kind: OutputKind,
    naming: Naming,
    /// Output file-name stems owned by this unit (`{crate}{extra}`, `lib…`).
    stems: Vec<String>,
    /// Outputs that must exist after a successful compile.
    expected_names: BTreeSet<String>,
    explicit_inputs: Vec<PathBuf>,
    /// Crate names of directly loaded procedural macros.
    proc_macros: Vec<String>,
    /// Cargo's shared incremental directory (`-C incremental=`).
    incremental: Option<PathBuf>,
    /// `{crate}{extra-filename}`: unique per Cargo unit.
    unit: String,
    /// This compile produces a procedural macro (it runs inside rustc).
    proc_macro_crate: bool,
    static_libraries: Vec<StaticLibrary>,
    native_search: Vec<PathBuf>,
    /// Cargo passed the arguments in a response file (`@path`), because the
    /// command line was too long for the platform; the compile does too.
    response_file: bool,
}

/// Crates found in the sysroot; Cargo passes them to `--extern` without a
/// path (`--extern proc_macro`). The compiler identity covers them.
const SYSROOT_CRATES: &[&str] = &["proc_macro", "test", "std", "core", "alloc"];

impl Invocation {
    fn analyze(raw: &[OsString]) -> std::result::Result<Self, String> {
        let rustc = PathBuf::from(raw.first().ok_or("missing rustc executable")?);
        let args = raw[1..]
            .iter()
            .map(|v| {
                v.to_str()
                    .map(str::to_owned)
                    .ok_or("non-UTF-8 rustc argument")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if args
            .iter()
            .any(|a| a == "-vV" || a.starts_with("--print") || a == "-")
        {
            return Err("compiler probe".into());
        }
        let response_file = args.iter().any(|arg| arg.starts_with('@'));
        let args = expand_response_files(args)?;
        if option_value(&args, "--sysroot").is_some() {
            return Err("custom sysroot contents are not modeled".into());
        }
        let target = option_value(&args, "--target");
        if target
            .as_deref()
            .is_some_and(|target| target.ends_with(".json") || Path::new(&target).is_file())
        {
            return Err("custom target specification contents are not modeled".into());
        }
        if codegen_value(&args, "target-cpu").as_deref() == Some("native") {
            return Err("host-native CPU features are not modeled in the cache key".into());
        }
        if codegen_values(&args)
            .any(|value| value == "save-temps" || value.starts_with("save-temps="))
        {
            return Err("compiler temporary outputs are not modeled".into());
        }
        if let Some(flag) = unmodeled_unstable_flag(&args) {
            return Err(format!("unstable compiler flag -Z {flag} is not modeled"));
        }
        let crate_name = option_value(&args, "--crate-name").ok_or("missing --crate-name")?;
        let test_harness = args.iter().any(|arg| arg == "--test");
        let declared_crate_types = multi_option_values(&args, "--crate-type");
        let crate_types = if test_harness || declared_crate_types.is_empty() {
            // `--test` builds a harness executable whatever the crate type.
            vec!["bin".to_owned()]
        } else {
            declared_crate_types
                .iter()
                .flat_map(|value| value.split(','))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        if let Some(kind) = crate_types.iter().find(|kind| {
            !matches!(
                kind.as_str(),
                "lib" | "rlib" | "bin" | "cdylib" | "dylib" | "proc-macro"
            )
        }) {
            return Err(format!("linked crate type {kind} is not modeled"));
        }
        let incremental = codegen_value(&args, "incremental").map(PathBuf::from);
        let (native_search, static_libraries) = native_inputs(&args)?;
        let out_dir = PathBuf::from(option_value(&args, "--out-dir").ok_or("missing --out-dir")?);
        // Cargo omits the extra filename for packages that build a cdylib or
        // dylib (users load them by name) and for MSVC/wasm executables
        // (debug-info names must stay predictable). The unit is still unique
        // within its output directory.
        let predictable_name = crate_types
            .iter()
            .any(|kind| matches!(kind.as_str(), "bin" | "cdylib" | "dylib"));
        let extra_filename = match codegen_value(&args, "extra-filename") {
            Some(extra) if extra.is_empty() || extra.contains(['/', '\\']) => {
                return Err("ambiguous extra filename".into());
            }
            Some(extra) => extra,
            None if predictable_name => String::new(),
            None => return Err("missing -C extra-filename".into()),
        };
        let source = args
            .iter()
            .find(|arg| arg.ends_with(".rs") && Path::new(arg.as_str()).exists())
            .map(PathBuf::from)
            .ok_or("missing primary Rust source")?;

        let mut explicit_inputs = vec![source.clone()];
        let mut proc_macros = Vec::new();
        for value in multi_option_values(&args, "--extern") {
            let Some((name, path)) = value.split_once('=') else {
                let name = value.rsplit(':').next().unwrap_or(&value);
                if SYSROOT_CRATES.contains(&name) {
                    continue;
                }
                return Err("extern dependency has no explicit artifact path".into());
            };
            let path = PathBuf::from(path);
            if !path.is_file() {
                return Err("extern dependency artifact is missing".into());
            }
            let ext = path.extension().and_then(OsStr::to_str).unwrap_or_default();
            if matches!(ext, "so" | "dylib" | "dll") {
                proc_macros.push(name.rsplit(':').next().unwrap_or(name).to_owned());
            }
            explicit_inputs.push(path);
        }

        let emits = multi_option_values(&args, "--emit");
        if emits.len() != 1
            || emits[0]
                .split(',')
                .any(|kind| !matches!(kind, "dep-info" | "metadata" | "link"))
            || args.iter().any(|arg| arg.starts_with("-o"))
        {
            return Err("unsupported emit set or custom output destination; running rustc to produce every requested output".into());
        }
        let emit = emits[0].split(',').collect::<BTreeSet<_>>();
        if !emit.contains("dep-info") {
            return Err("unsupported emit set".into());
        }
        let naming = Naming::for_target(target.as_deref());
        let unit = format!("{crate_name}{extra_filename}");
        let library = format!("lib{unit}");
        let mut expected_names = BTreeSet::from([format!("{unit}.d")]);
        let has = |kind: &str| crate_types.iter().any(|t| t == kind);
        let mut linked = false;
        if emit.contains("metadata") {
            expected_names.insert(format!("{library}.rmeta"));
        }
        if emit.contains("link") {
            if has("lib") || has("rlib") {
                expected_names.insert(format!("{library}.rlib"));
            }
            if has("bin") {
                expected_names.insert(format!("{unit}{}", naming.exe_suffix));
                linked = true;
            }
            if has("cdylib") || has("dylib") || has("proc-macro") {
                expected_names.insert(format!("{}{unit}{}", naming.dll_prefix, naming.dll_suffix));
                linked = true;
            }
        }
        let kind = if linked {
            OutputKind::Linked
        } else {
            OutputKind::Library
        };
        if has_unmodeled_codegen_inputs(&args) {
            return Err("native linker or external codegen inputs are not modeled".into());
        }
        // Link inputs come from `--print link-args`; clippy-driver combined
        // with that print request writes corrupted `# env-dep:` names (for
        // example `64` in place of CLIPPY_ARGS), which would make Cargo
        // rebuild on every run. Its linked units (build scripts and proc
        // macros of workspace members) are compiled directly.
        let driver = rustc
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        if kind == OutputKind::Linked && !(driver == "rustc" || driver.starts_with("rustc-")) {
            return Err(format!(
                "linked outputs compiled by {driver} are not modeled"
            ));
        }
        let stems = vec![unit.clone(), library];
        let proc_macro_crate = has("proc-macro");
        Ok(Self {
            rustc,
            args,
            crate_name,
            out_dir,
            kind,
            naming,
            stems,
            expected_names,
            explicit_inputs,
            proc_macros,
            incremental,
            unit,
            proc_macro_crate,
            static_libraries,
            native_search,
            response_file,
        })
    }

    /// True for a file name this unit's compile writes.
    fn owns(&self, file_name: &str) -> bool {
        self.stems.iter().any(|stem| {
            file_name
                .strip_prefix(stem.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    }
}

/// Parse native search paths and libraries. Library search directories
/// (`-L native=`, `-L framework=`) cannot supply Rust crates; `-L all=`,
/// `crate=` and bare `-L` can, so they remain unmodeled.
fn native_inputs(
    args: &[String],
) -> std::result::Result<(Vec<PathBuf>, Vec<StaticLibrary>), String> {
    let mut search = Vec::new();
    let mut libraries = Vec::new();
    let mut values = Vec::<(&str, &str)>::new();
    for (index, arg) in args.iter().enumerate() {
        for flag in ["-L", "-l"] {
            if arg == flag {
                if let Some(value) = args.get(index + 1) {
                    values.push((flag, value));
                }
            } else if let Some(value) = arg.strip_prefix(flag)
                && !value.is_empty()
            {
                values.push((flag, value));
            }
        }
    }
    for (flag, value) in values {
        if flag == "-L" {
            match value.split_once('=') {
                Some(("dependency", _)) => {}
                Some(("native" | "framework", path)) => search.push(PathBuf::from(path)),
                _ => {
                    return Err(format!(
                        "native linker or external codegen inputs are not modeled: search path -L {value} may supply crates"
                    ));
                }
            }
            continue;
        }
        let (kind, name) = match value.split_once('=') {
            Some((kind, name)) => (kind, name),
            None => ("dylib", value),
        };
        let (kind, modifiers) = kind.split_once(':').unwrap_or((kind, ""));
        let name = name.split(':').next().unwrap_or(name);
        match kind {
            "static" => {
                if !modifiers.split(',').any(|m| m == "-bundle") {
                    libraries.push(StaticLibrary {
                        name: name.to_owned(),
                        verbatim: modifiers.split(',').any(|m| m == "+verbatim"),
                    });
                }
            }
            "dylib" | "framework" | "raw-dylib" => {}
            _ => {
                return Err(format!(
                    "native linker or external codegen inputs are not modeled: -l {value}"
                ));
            }
        }
    }
    Ok((search, libraries))
}

/// Unstable flags whose effect is fully described by the arguments (kept in
/// the key) and whose inputs arrive through dep-info. `-Zbuild-std` passes
/// the first two to every crate; the rust-src sources are tracked files and
/// the nightly compiler is part of the identity. Anything else that could
/// write extra files or read untracked inputs stays unmodeled.
const MODELED_UNSTABLE_FLAGS: &[&str] = &[
    "unstable-options",
    "force-unstable-if-unmarked",
    "share-generics",
    "threads",
    "macro-backtrace",
];

fn unmodeled_unstable_flag(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(index, arg)| {
        let value = if arg == "-Z" {
            args.get(index + 1).map(String::as_str).unwrap_or_default()
        } else {
            arg.strip_prefix("-Z")?
        };
        let name = value.split('=').next().unwrap_or(value);
        (!MODELED_UNSTABLE_FLAGS.contains(&name)).then(|| name.to_owned())
    })
}

/// Codegen inputs that remain outside the model for any output kind.
fn has_unmodeled_codegen_inputs(args: &[String]) -> bool {
    codegen_values(args).any(|value| {
        ["linker-plugin-lto=", "profile-use=", "llvm-plugins="]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    })
}

fn codegen_values(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter().enumerate().filter_map(|(i, arg)| {
        if arg == "-C" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("-C")
        }
    })
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == name {
            args.get(i + 1).cloned()
        } else {
            arg.strip_prefix(&format!("{name}=")).map(str::to_owned)
        }
    })
}

fn multi_option_values(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            if arg == name {
                args.get(i + 1).cloned()
            } else {
                arg.strip_prefix(&format!("{name}=")).map(str::to_owned)
            }
        })
        .collect()
}

fn codegen_value(args: &[String], name: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == "-C" {
            args.get(i + 1)?
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned)
        } else {
            arg.strip_prefix("-C")?
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned)
        }
    })
}

struct Identity {
    static_key: String,
    normalizer: PathNormalizer,
    /// Normalizes only the workspace and target roots: path-valued `env!`
    /// dependencies compare in this form when the outputs embed no root.
    root_normalizer: PathNormalizer,
    workspace: PathBuf,
    fingerprint: diagnostics::Fingerprint,
    diagnostic_group: String,
    digests: digests::Digests,
    /// Synthetic inputs that pin a leaking candidate to this checkout.
    pins: Vec<(String, String)>,
    /// Environment overrides given to rustc (see `stable_empty_directories`).
    virtual_env: BTreeMap<String, String>,
    /// The canonical git checkout containing this compile, if any.
    checkout: Option<PathBuf>,
    /// This compile's working directory and checkout under every spelling
    /// that reaches them (see `own_roots`).
    own_roots: Vec<PathBuf>,
    /// The `--diagnostic-width` rustc was given, if any.
    diagnostic_width: Option<String>,
}

/// Synthetic input recording the terminal width a candidate's diagnostics
/// were rendered for: the width, `*` for a candidate with no diagnostics, or
/// `absent` for a compile given no width. Candidates from before this input
/// existed were all compiled without a width.
const DIAGNOSTIC_WIDTH_INPUT: &str = "@bellows:diagnostic-width";
const ANY_DIAGNOSTIC_WIDTH: &str = "*";

impl Identity {
    fn diagnostic_width(&self) -> &str {
        self.diagnostic_width.as_deref().unwrap_or("absent")
    }

    /// The value rustc sees for an environment variable.
    fn env_value(&self, name: &str) -> Option<String> {
        self.virtual_env
            .get(name)
            .cloned()
            .or_else(|| env::var(name).ok())
    }

    fn pin_value(&self, name: &str) -> Option<&str> {
        self.pins
            .iter()
            .find(|(pin, _)| pin == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A successful compile whose result is deliberately not stored.
#[derive(Debug)]
struct NotStored(String);

impl std::fmt::Display for NotStored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotStored {}

fn rustc_wrapper(raw: &[OsString]) -> Result<ExitStatus> {
    let started = Instant::now();
    if let Some((out_dir, unit_file)) = build_script_unit(raw) {
        build_script::remove_launcher(&out_dir, &unit_file);
    }
    let result = match cache_or_compile(raw) {
        Ok(status) => Ok(status),
        Err(error) => {
            let crate_name = wrapper_crate_name(raw);
            record_event(
                "fallback",
                crate_name,
                None,
                None,
                &format!("cache pipeline failed; retrying official rustc: {error:#}"),
            );
            passthrough(raw)
        }
    };
    if result.as_ref().is_ok_and(ExitStatus::success)
        && let Some((out_dir, unit_file)) = build_script_unit(raw)
        && out_dir.join(&unit_file).is_file()
        && let Err(error) = build_script::install_launcher(&out_dir, &unit_file)
    {
        record_event(
            "fallback",
            wrapper_crate_name(raw),
            None,
            None,
            &format!("build-script launcher not installed; the script runs uncached: {error:#}"),
        );
    }
    record_event_duration(
        "compiler_timing",
        wrapper_crate_name(raw),
        None,
        None,
        "compiler wrapper elapsed time",
        Some(started.elapsed().as_millis() as u64),
    );
    result
}

/// The output directory and executable name of a build-script compile.
fn build_script_unit(raw: &[OsString]) -> Option<(PathBuf, String)> {
    let args = raw
        .iter()
        .skip(1)
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let crate_name = option_value(&args, "--crate-name")?;
    let out_dir = PathBuf::from(option_value(&args, "--out-dir")?);
    if !build_script::is_build_script_compile(&crate_name, &out_dir)
        || !multi_option_values(&args, "--emit")
            .iter()
            .any(|emit| emit.split(',').any(|kind| kind == "link"))
    {
        return None;
    }
    let extra = codegen_value(&args, "extra-filename").unwrap_or_default();
    Some((out_dir, build_script::unit_file(&crate_name, &extra)))
}

fn wrapper_crate_name(raw: &[OsString]) -> &str {
    raw.windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .and_then(|pair| pair[1].to_str())
        .unwrap_or("rustc")
}

fn cache_or_compile(raw: &[OsString]) -> Result<ExitStatus> {
    let local_only = env::var("BELLOWS_LOCAL_ONLY").as_deref() == Ok("1");
    let invocation = match Invocation::analyze(raw) {
        Ok(invocation) => invocation,
        Err(reason) => {
            record_event("bypass", wrapper_crate_name(raw), None, None, &reason);
            return passthrough(raw);
        }
    };
    let identity = match build_identity(&invocation) {
        Ok(identity) => identity,
        Err(error) => {
            record_event(
                "fallback",
                &invocation.crate_name,
                None,
                None,
                &format!("identity: {error:#}"),
            );
            return passthrough(raw);
        }
    };
    let remote = if local_only {
        None
    } else {
        let server = env::var("BELLOWS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
        match Remote::new(&server, env::var("BELLOWS_AUTH_TOKEN").ok()) {
            Ok(remote) => Some(remote),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("invalid remote configuration: {error}"),
                );
                return passthrough(raw);
            }
        }
    };
    let l1_disabled = env::var("BELLOWS_L1").as_deref() == Ok("0");
    let l1 = if l1_disabled {
        None
    } else {
        let store = if local_only {
            local_store(Some(&state_dir(&identity.workspace)), false)
        } else {
            Store::open_for_access(
                state_dir(&identity.workspace).join(format!("l1-v{PROTOCOL_VERSION}")),
            )
        };
        match store {
            Ok(store) => Some(store),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("L1 is unavailable: {error:#}"),
                );
                None
            }
        }
    };
    let identity_hint = diagnostics::remember_identity(
        &state_dir(&identity.workspace),
        &identity.diagnostic_group,
        &identity.fingerprint,
    )
    .unwrap_or_else(|error| {
        format!("no usable candidate; diagnostic identity history unavailable: {error:#}")
    });
    let mut local_index = CandidateIndex::default();
    if let Some(store) = &l1 {
        match store.read_candidates(&identity.static_key) {
            Ok(index) => {
                local_index = index;
                match try_l1_candidates(
                    store,
                    remote.as_ref(),
                    &invocation,
                    &identity,
                    &local_index,
                ) {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(error) => record_event(
                        "fallback",
                        &invocation.crate_name,
                        Some(&identity.static_key),
                        None,
                        &format!("L1 restore failed: {error:#}"),
                    ),
                }
            }
            Err(error) => record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("L1 index is unavailable or corrupt: {error:#}"),
            ),
        }
    }

    let (index, remote_available) = match &remote {
        Some(remote) => match remote.candidates(&identity.static_key) {
            Ok(index) => (index, true),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("remote unavailable: {error}"),
                );
                (CandidateIndex::default(), false)
            }
        },
        None => (CandidateIndex::default(), false),
    };
    if let Some(remote) = &remote
        && let Some(status) = try_candidates(remote, l1.as_ref(), &invocation, &identity, &index)?
    {
        return Ok(status);
    }

    let mut reasons = Vec::new();
    if !local_index.candidates.is_empty() {
        reasons.push(format!(
            "local cache: {}",
            explain_candidates(&identity, &local_index)
        ));
    }
    if !index.candidates.is_empty() {
        reasons.push(format!(
            "remote cache: {}",
            explain_candidates(&identity, &index)
        ));
    }
    if remote.is_some() && !remote_available {
        reasons.push("remote unavailable (see fallback event for request error)".into());
    }
    // An L1 turned off with BELLOWS_L1=0 is configuration, reported once by
    // `doctor`; only a failure to open it (already a fallback) explains a miss.
    if l1.is_none() && !l1_disabled {
        reasons.push("local cache unavailable (see fallback event)".into());
    }
    if local_index.candidates.is_empty() && index.candidates.is_empty() {
        reasons.push(identity_hint);
    }
    let miss_detail = reasons.join("; ");
    record_event(
        "miss",
        &invocation.crate_name,
        Some(&identity.static_key),
        None,
        &miss_detail,
    );

    let client_id = format!("{}-{}", std::process::id(), now_ms());
    let mut owned_token = None;
    if let Some(remote) = &remote
        && remote_available
    {
        match remote.acquire(&identity.static_key, &client_id) {
            Ok(LeaseResponse::Owned { token, .. }) => owned_token = Some(token),
            Ok(LeaseResponse::Wait {
                retry_after_ms,
                expires_ms,
            }) => {
                record_event(
                    "wait",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    "another runner owns this cold action",
                );
                let max_wait = match bounded_timeout(
                    "BELLOWS_MAX_WAIT_MS",
                    env::var("BELLOWS_MAX_WAIT_MS").ok().as_deref(),
                    30_000,
                    0,
                    600_000,
                ) {
                    Ok(duration) => duration,
                    Err(error) => {
                        record_event(
                            "fallback",
                            &invocation.crate_name,
                            Some(&identity.static_key),
                            None,
                            &format!("invalid single-flight wait limit: {error}"),
                        );
                        Duration::ZERO
                    }
                };
                let deadline = Instant::now() + max_wait;
                let mut checked_candidates = index
                    .candidates
                    .iter()
                    .map(|candidate| candidate.action_key.clone())
                    .collect::<BTreeSet<_>>();
                let mut lease_failed = false;
                while Instant::now() < deadline && now_ms() < expires_ms {
                    thread::sleep(Duration::from_millis(retry_after_ms.clamp(50, 1_000)));
                    if let Ok(mut index) = remote.candidates(&identity.static_key) {
                        index.candidates.retain(|candidate| {
                            checked_candidates.insert(candidate.action_key.clone())
                        });
                        if let Some(status) =
                            try_candidates(remote, l1.as_ref(), &invocation, &identity, &index)?
                        {
                            record_event(
                                "single_flight",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                "restored result published by lease owner",
                            );
                            return Ok(status);
                        }
                    }
                    // A static key can have several valid candidates (for example,
                    // different exact OUT_DIR dependencies). Once the owner releases
                    // its lease, compile our variant instead of waiting for an
                    // incompatible result until the original lease expires.
                    match remote.acquire(&identity.static_key, &client_id) {
                        Ok(LeaseResponse::Owned { token, .. }) => {
                            owned_token = Some(token);
                            record_event(
                                "lease_acquired",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                "acquired released lease; no published candidate matches this invocation",
                            );
                            break;
                        }
                        Ok(LeaseResponse::Wait { .. }) => {}
                        Err(error) => {
                            record_event(
                                "fallback",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                &format!("lease unavailable while waiting: {error}"),
                            );
                            lease_failed = true;
                            break;
                        }
                    }
                }
                if owned_token.is_none() && !lease_failed {
                    record_event(
                        "fallback",
                        &invocation.crate_name,
                        Some(&identity.static_key),
                        None,
                        "single-flight wait timed out; compiling locally",
                    );
                }
            }
            Err(error) => record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("lease unavailable: {error}"),
            ),
        }
    }

    let outcome = compile_and_capture(&invocation, &identity);
    let (status, captured) = match outcome {
        Ok(value) => value,
        Err(error) => {
            if let Some(token) = &owned_token
                && let Some(remote) = &remote
            {
                remote.release(&identity.static_key, token);
            }
            return Err(error);
        }
    };
    if !status.success() {
        if let Some(token) = &owned_token
            && let Some(remote) = &remote
        {
            remote.release(&identity.static_key, token);
        }
        return Ok(status);
    }
    // A read-only client (for example CI jobs sharing a developer's local
    // service) restores verified results but never publishes its own.
    let captured = captured.filter(|_| env::var("BELLOWS_READ_ONLY").as_deref() != Ok("1"));
    if let Some(captured) = captured {
        let scope = if captured.pinned {
            format!(
                " ({} output, pinned to this checkout: it embeds the workspace or target path)",
                invocation.kind.name()
            )
        } else {
            format!(
                " ({} output, shareable across checkouts)",
                invocation.kind.name()
            )
        };
        let mut stored_locally = false;
        if let Some(store) = &l1 {
            match cache_captured(store, &captured) {
                Ok(()) => stored_locally = true,
                Err(error) => record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("L1 publication failed: {error:#}"),
                ),
            }
        }
        if stored_locally && remote.is_none() {
            record_event(
                "store",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&captured.candidate.action_key),
                &format!("stored compiler result{scope}"),
            );
        }
        if let Some(remote) = &remote
            && remote_available
        {
            match publish(remote, captured) {
                Ok(action_key) => record_event(
                    "store",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&action_key),
                    &format!("published compiler result{scope}"),
                ),
                // A result larger than the server accepts stays local.
                Err(error) if format!("{error:#}").contains("413") => record_event(
                    "not_stored",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    "compiled but not stored remotely: an output exceeds the server's blob size limit (kept in the local cache)",
                ),
                Err(error) => record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("upload failed after successful local compile: {error:#}"),
                ),
            }
        }
    }
    if let Some(token) = &owned_token
        && let Some(remote) = &remote
    {
        remote.release(&identity.static_key, token);
    }
    Ok(status)
}

/// Replaces each `@path` argument with the file's lines, as rustc does: one
/// argument per line, no quoting, not recursive. Cargo writes such a file
/// when a command line is too long for the platform (Windows, `-Zbuild-std`).
/// The file's path is scratch and never part of the identity; its contents
/// are. `@shell:` files use shell quoting and are not modeled.
fn expand_response_files(args: Vec<String>) -> std::result::Result<Vec<String>, String> {
    let mut expanded = Vec::with_capacity(args.len());
    for arg in args {
        let Some(path) = arg.strip_prefix('@') else {
            expanded.push(arg);
            continue;
        };
        if path.starts_with("shell:") {
            return Err("shell-quoted rustc response files are not modeled".into());
        }
        let text = fs::read_to_string(path)
            .map_err(|error| format!("read rustc response file {path}: {error}"))?;
        expanded.extend(text.lines().map(str::to_owned));
    }
    Ok(expanded)
}

/// Starts rustc with `args`, through a response file when Cargo needed one.
fn rustc_command(
    invocation: &Invocation,
    args: &[String],
) -> Result<(Command, Option<tempfile::TempPath>)> {
    let mut command = Command::new(&invocation.rustc);
    if !invocation.response_file || args.iter().any(|arg| arg.contains('\n')) {
        command.args(args);
        return Ok((command, None));
    }
    fs::create_dir_all(&invocation.out_dir).context("create output directory")?;
    let mut file = tempfile::Builder::new()
        .prefix(".bellows-args-")
        .tempfile_in(&invocation.out_dir)
        .context("create rustc response file")?;
    file.write_all(args.join("\n").as_bytes())
        .context("write rustc response file")?;
    let path = file.into_temp_path();
    command.arg(format!("@{}", path.display()));
    Ok((command, Some(path)))
}

fn passthrough(raw: &[OsString]) -> Result<ExitStatus> {
    let (rustc, args) = raw.split_first().context("missing rustc executable")?;
    Ok(Command::new(rustc).args(args).status()?)
}

fn normalizer(workspace: &Path, out_dir: &Path, source: Option<&Path>) -> PathNormalizer {
    let mut bases = root_bases(workspace, out_dir, source);
    let home = bellows_core::user_home();
    if let Some(cargo_home) = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")))
    {
        bases.push(("$CARGO_HOME".into(), canonical_base(cargo_home)));
    }
    bases.push(("$RUSTUP_HOME".into(), canonical_base(rustup_home())));
    if let Some(home) = home {
        bases.push(("$HOME".into(), canonical_base(home)));
    }
    PathNormalizer::new(bases)
}

/// The per-checkout roots: the workspace and the Cargo target directory.
/// `source` is the compile's crate root, which locates its checkout when the
/// working directory lies outside one (see `compile_checkout`).
fn root_bases(workspace: &Path, out_dir: &Path, source: Option<&Path>) -> Vec<(String, PathBuf)> {
    let target = target_root(workspace, out_dir);
    let mut bases = vec![("$WORKSPACE".into(), workspace.to_path_buf())];
    // The source checkout containing this compile. Nested Cargo builds and
    // path dependencies run rustc from a subdirectory while reading (and
    // baking) paths elsewhere in the checkout. It is found from the source
    // tree, never from where `bellows run` happened to be launched: a session
    // started in a crate subdirectory must not shrink the checkout.
    if let Some(checkout) = compile_checkout(workspace, source)
        && checkout != workspace
    {
        if let Some(given) = given_spelling(workspace, &checkout)
            .or_else(|| source_spelling(workspace, source, &checkout))
        {
            bases.push(("$CHECKOUT".into(), given));
        }
        bases.push(("$CHECKOUT".into(), checkout));
    }
    // Cargo's manifest directory may retain an 8.3 spelling even when the
    // process working directory has already been expanded by Windows.
    for alias in [
        env::current_dir().ok(),
        env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        if alias.canonicalize().ok().as_deref() == workspace.canonicalize().ok().as_deref() {
            bases.push(("$WORKSPACE".into(), alias));
        }
    }
    if let Some(target) = target {
        // Profiles that pass identical rustc arguments (for example
        // `release` and a `test-fast` inheriting it) differ only in their
        // `<target>/<profile>` directory. Naming it separately lets one
        // verified result serve every such profile.
        if let Some(profile) = profile_root(&target, out_dir) {
            bases.push(("$PROFILE".into(), canonical_base(profile.clone())));
            bases.push(("$PROFILE".into(), profile));
        }
        bases.push(("$TARGET".into(), canonical_base(target.clone())));
        // Retain the caller's spelling too: Windows temp directories may use
        // 8.3 names that canonicalize() expands to a different path string.
        bases.push(("$TARGET".into(), target));
    }
    bases
}

/// The git checkout containing `dir` (a worktree's `.git` file or a clone's
/// `.git` directory); otherwise the session's launch directory when it
/// contains `dir`. Sources under Cargo's or rustup's home are never part of
/// a checkout.
fn checkout_root(dir: &Path) -> Option<PathBuf> {
    let dir = canonical_base(dir.to_path_buf());
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| bellows_core::user_home().map(|home| home.join(".cargo")));
    if cargo_home
        .into_iter()
        .chain([rustup_home()])
        .any(|managed| dir.starts_with(canonical_base(managed)))
    {
        return None;
    }
    git_root(&dir).or_else(|| {
        env::var_os("BELLOWS_WORKSPACE")
            .map(PathBuf::from)
            .filter(|launch| launch.is_absolute())
            .map(canonical_base)
            .filter(|launch| dir.starts_with(launch))
    })
}

/// The checkout a compile belongs to: the one containing its working
/// directory, or else the one containing its crate root. Cargo may run rustc
/// from a directory outside every checkout (trybuild's generated project in a
/// target directory that lives elsewhere) for a crate whose sources and path
/// dependencies are in one; `/bellows/checkout` must still name that checkout,
/// or the dependency paths its metadata carries resolve against the wrong
/// directory in diagnostics.
fn compile_checkout(workspace: &Path, source: Option<&Path>) -> Option<PathBuf> {
    checkout_root(workspace).or_else(|| {
        let source = source?;
        let dir = absolute_path(source, workspace);
        checkout_root(dir.parent()?)
    })
}

/// Cargo's spelling of `root` (a checkout found from the crate root, not the
/// working directory): the matching ancestor of the crate root as Cargo
/// passed it, which may keep a Windows 8.3 name or a symlink that the
/// canonical `root` has resolved.
fn source_spelling(workspace: &Path, source: Option<&Path>, root: &Path) -> Option<PathBuf> {
    let given = absolute_path(source?, workspace);
    let canonical = canonical_base(given.clone());
    let depth = canonical.strip_prefix(root).ok()?.components().count();
    let spelling = given.ancestors().nth(depth)?.to_string_lossy().into_owned();
    // A manifest path may use `/` on Windows; rustc spells the checkout's
    // files (compiled from inside it) with native separators.
    Some(PathBuf::from(if cfg!(windows) {
        spelling.replace('/', "\\")
    } else {
        spelling
    }))
}

/// A canonical path as a fresh process spells it: without a Windows
/// verbatim (`\\?\` or `\\?\UNC\`) prefix.
fn ordinary_spelling(path: &Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_owned(),
        },
        None => text.into_owned(),
    }
}

fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Cargo's spelling of `root`, an ancestor of the canonical `workspace`: the
/// same number of levels above the process working directory.
fn given_spelling(workspace: &Path, root: &Path) -> Option<PathBuf> {
    let depth = workspace.strip_prefix(root).ok()?.components().count();
    env::current_dir()
        .ok()?
        .ancestors()
        .nth(depth)
        .map(Path::to_path_buf)
}

/// An input inside a different git checkout than this compile's: its path
/// names another worktree, so the same record must not serve this one.
fn foreign_checkout(path: &Path, checkout: Option<&Path>) -> bool {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    match git_root(resolved.parent().unwrap_or(&resolved)) {
        Some(root) => checkout != Some(root.as_path()),
        None => false,
    }
}

/// Inputs that no per-checkout root covers (`$HOME/…` or absolute host
/// paths); toolchain and Cargo-home sources are shared by every checkout.
fn outside_checkout_roots(normalized: &str) -> bool {
    ![
        "$CHECKOUT",
        "$WORKSPACE",
        "$TARGET",
        "$PROFILE",
        "$CARGO_HOME",
        "$RUSTUP_HOME",
    ]
    .iter()
    .any(|root| normalized.starts_with(root))
}

/// `<target>[/<triple>]/<profile>` for Cargo's `deps`, `build/<unit>` and
/// `examples` output directories.
fn profile_root(target: &Path, out_dir: &Path) -> Option<PathBuf> {
    out_dir
        .ancestors()
        .find(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "deps" || name == "build" || name == "examples")
        })
        .and_then(Path::parent)
        .filter(|profile| profile.starts_with(target) && *profile != target)
        .map(Path::to_path_buf)
}

fn canonical_base(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn target_root(workspace: &Path, out_dir: &Path) -> Option<PathBuf> {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| absolute_path(&path, workspace))
        .or_else(|| infer_target_root(out_dir))
}

fn infer_target_root(out_dir: &Path) -> Option<PathBuf> {
    out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "target"))
        .map(Path::to_path_buf)
        .or_else(|| {
            // Cargo's --target-dir flag is not exported to rustc as
            // CARGO_TARGET_DIR. Compiler products still have the stable
            // <root>/<profile>/deps shape (or <root>/<triple>/<profile>/deps
            // for cross compilation), so normalize the nearest complete
            // target subtree even when its directory has a custom name.
            (out_dir.file_name().is_some_and(|name| name == "deps"))
                .then(|| out_dir.parent()?.parent().map(Path::to_path_buf))
                .flatten()
        })
}

fn absolute_path(path: &Path, workspace: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// The canonical path of a compiler input, which is how a record names it.
///
/// Resolving a symlink (or a junction) on the way loses what the link pointed
/// at. The record would name the link's target, and a later compile whose
/// link points somewhere else would be checked against the old target's
/// files, find them unchanged, and restore code it does not compile. An input
/// reached through a link is therefore not cacheable.
///
/// The exception is a link at or above one of this compile's own roots: its
/// working directory or its checkout, under a spelling in `own_roots`. A
/// record names such an input relative to that root (`$WORKSPACE/src/lib.rs`),
/// never by where the link led, and every compile derives its roots afresh
/// from its own working directory. A compile whose link points elsewhere is
/// then checked against the files it reads itself. Below the root the rule is
/// unchanged, so the rest of the path holds no link.
fn canonical_compiler_input(path: &Path, own_roots: &[PathBuf]) -> Result<PathBuf> {
    let path = without_dot_segments(path)?;
    for ancestor in path.ancestors() {
        if own_roots.iter().any(|root| root == ancestor) {
            break;
        }
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            bail!(
                "symlinked compiler input is not cacheable: {}",
                ancestor.display()
            );
        }
    }
    Ok(path.canonicalize()?)
}

/// `path` without `.` and `..` segments. A `..` that follows a link is not
/// resolved: the system may take the parent of the link's target, not of the
/// link, so the path cannot be judged by its spelling.
fn without_dot_segments(path: &Path) -> Result<PathBuf> {
    let mut plain = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if fs::symlink_metadata(&plain)?.file_type().is_symlink() {
                    bail!(
                        "symlinked compiler input is not cacheable: {} is followed by `..`",
                        plain.display()
                    );
                }
                plain.pop();
            }
            other => plain.push(other.as_os_str()),
        }
    }
    Ok(plain)
}

/// This compile's working directory and checkout under every spelling that
/// reaches them: their canonical paths, and each spelling the normalizer
/// knows them by (Cargo's, which may pass through a symlink or junction) that
/// resolves to one of them right now. Only spellings the normalizer knows
/// qualify, so an input accepted below one is also normalized wherever it is
/// written down: in arguments, dep-info and compiler messages.
fn own_roots(
    workspace: &Path,
    checkout: Option<&Path>,
    root_normalizer: &PathNormalizer,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (token, root) in [("$WORKSPACE", Some(workspace)), ("$CHECKOUT", checkout)] {
        let Some(root) = root else {
            continue;
        };
        let canonical = root.to_string_lossy();
        roots.push(root.to_path_buf());
        roots.push(PathBuf::from(ordinary_spelling(root)));
        // A build that reaches its roots plainly has no other spelling, and
        // pays for no lookup here.
        for spelling in other_spellings(&canonical, &root_normalizer.spellings(token)) {
            let spelling = PathBuf::from(spelling);
            if spelling
                .canonicalize()
                .is_ok_and(|resolved| resolved == root)
            {
                roots.push(spelling);
            }
        }
    }
    roots
}

/// The spellings of a root that are not just another way to write its
/// canonical path: without the Windows verbatim prefix, with forward slashes
/// or, on Windows, in another letter case (some terminals start a build with
/// a lower-case drive letter). What remains reaches the root by another
/// route: through a symlink or junction, or by a Windows 8.3 name.
fn other_spellings(canonical: &str, spellings: &[String]) -> Vec<String> {
    let ordinary = ordinary_spelling(Path::new(canonical));
    let forward = ordinary.replace('\\', "/");
    let same = |spelling: &str, written: &str| {
        if cfg!(windows) {
            spelling.eq_ignore_ascii_case(written)
        } else {
            spelling == written
        }
    };
    let mut others = spellings
        .iter()
        .filter(|spelling| {
            ![canonical, ordinary.as_str(), forward.as_str()]
                .iter()
                .any(|written| same(spelling, written))
        })
        .cloned()
        .collect::<Vec<_>>();
    others.sort();
    others.dedup();
    others
}

/// Cargo's spelling of `root` when it reaches the root through a symlink or
/// junction: the one rustc prints for the files below it. A restore must
/// write that spelling, not the link's target: Cargo decides what to rebuild
/// from the paths in a restored dep-info file, and paths that name the target
/// would keep it watching the old target after the link has moved.
fn link_spelling(normalizer: &PathNormalizer, token: &str, root: &Path) -> Option<String> {
    let through_link = |spelling: &Path| {
        spelling.ancestors().any(|ancestor| {
            fs::symlink_metadata(ancestor).is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
    };
    other_spellings(&root.to_string_lossy(), &normalizer.spellings(token))
        .into_iter()
        .find(|spelling| {
            let path = Path::new(spelling);
            // Never the verbatim or forward-slash form of a Windows path: no
            // fresh process spells a directory that way.
            let unusual =
                spelling.starts_with(r"\\?\") || (cfg!(windows) && spelling.contains('/'));
            !unusual
                && through_link(path)
                && path.canonicalize().is_ok_and(|resolved| resolved == root)
        })
}

/// Localizes this compile's roots to the spelling that reaches them through a
/// link, where there is one (see `link_spelling`).
pub(crate) fn localize_through_links(
    normalizers: [&mut PathNormalizer; 2],
    workspace: &Path,
    checkout: Option<&Path>,
) {
    let [normalizer, root_normalizer] = normalizers;
    for (token, root) in [("$WORKSPACE", Some(workspace)), ("$CHECKOUT", checkout)] {
        if let Some(root) = root
            && let Some(spelling) = link_spelling(root_normalizer, token, root)
        {
            normalizer.localize_as(token, &spelling);
            root_normalizer.localize_as(token, &spelling);
        }
    }
}

/// The value a root's pin compares: the root's canonical path and, when the
/// build reaches it under another spelling (through a symlink or junction, or
/// by a Windows 8.3 name), those spellings too. An output that embeds a root
/// may embed any of them, so a pinned candidate is reused only by a build
/// that spells the root the same way. For a root reached plainly this is the
/// canonical path alone, as it always was.
pub(crate) fn pin_value(canonical: &str, spellings: &[String]) -> String {
    let others = other_spellings(canonical, spellings);
    if others.is_empty() {
        canonical.to_owned()
    } else {
        format!("{canonical}\nspelled {}", others.join("\nspelled "))
    }
}

fn diagnostic_width(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--diagnostic-width" {
            args.get(index + 1).cloned()
        } else {
            arg.strip_prefix("--diagnostic-width=").map(str::to_owned)
        }
    })
}

/// Whether a compile printed anything whose rendering depends on the
/// terminal width. Cargo's JSON artifact notifications do not.
fn has_rendered_diagnostics(stream: &[u8]) -> bool {
    stream.split(|byte| *byte == b'\n').any(|line| {
        let line = line.trim_ascii();
        !line.is_empty() && !line.starts_with(br#"{"$message_type":"artifact""#)
    })
}

fn normalized_compiler_arguments(
    invocation: &Invocation,
    normalizer: &PathNormalizer,
) -> Vec<String> {
    let source = invocation
        .explicit_inputs
        .first()
        .map(|path| path.to_string_lossy());
    let mut normalized = Vec::with_capacity(invocation.args.len());
    let mut path_value = false;
    let mut skip_next = false;
    for (index, arg) in invocation.args.iter().enumerate() {
        // The incremental session directory is scratch state, not an input:
        // incremental and non-incremental compiles of the same crate share
        // one identity (see the incremental policy in compile_and_capture).
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if arg == "-C"
            && invocation
                .args
                .get(index + 1)
                .is_some_and(|value| value.starts_with("incremental="))
        {
            skip_next = true;
            continue;
        }
        if arg.starts_with("-Cincremental=") {
            continue;
        }
        // Terminal width only reflows rendered diagnostics. It is checked
        // against a candidate only when that candidate has diagnostics to
        // replay (see DIAGNOSTIC_WIDTH_INPUT).
        if arg == "--diagnostic-width" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--diagnostic-width=") {
            continue;
        }
        let is_path = path_value
            || source.as_deref() == Some(arg.as_str())
            || arg.starts_with("--out-dir=")
            || arg.starts_with("--extern=")
            || arg.starts_with("-Ldependency=");
        normalized.push(if is_path {
            normalizer.normalize(arg)
        } else {
            arg.clone()
        });
        path_value = matches!(arg.as_str(), "--out-dir" | "--extern" | "-L");
    }
    normalized
}

fn build_identity(invocation: &Invocation) -> Result<Identity> {
    // Cargo can change directory for a manifest or a nested build. Relative
    // rustc arguments and dep-info belong to that invocation's working dir,
    // not the parent Bellows session's launch directory.
    let workspace = env::current_dir()?.canonicalize()?;
    let crate_root = invocation.explicit_inputs.first().map(PathBuf::as_path);
    let mut normalizer = normalizer(&workspace, &invocation.out_dir, crate_root);
    let mut root_normalizer =
        PathNormalizer::new(root_bases(&workspace, &invocation.out_dir, crate_root));
    let checkout = compile_checkout(&workspace, crate_root);
    localize_through_links(
        [&mut normalizer, &mut root_normalizer],
        &workspace,
        checkout.as_deref(),
    );
    let digests = digests::Digests::new(&state_dir(&workspace));
    let compiler = compiler_identity(&invocation.rustc, &state_dir(&workspace))?;
    let normalized_args = normalized_compiler_arguments(invocation, &normalizer);
    let mut components = diagnostics::argument_components(&normalized_args, str::to_owned);
    components.insert(
        "argument order".into(),
        digest_bytes(&serde_json::to_vec(&normalized_args)?),
    );
    components.insert(
        "compiler version changed".into(),
        digest_bytes(&compiler.stdout),
    );
    components.insert(
        "protocol changed".into(),
        digest_bytes(PROTOCOL_VERSION.to_string().as_bytes()),
    );
    // Earlier captures treated every backslash as a Make escape and could
    // omit the real dependency. Never reuse those compiler manifests.
    let dep_info_format = b"rustc-space-escape-v5-structured-dep-info";
    components.insert(
        "dependency parser changed".into(),
        digest_bytes(dep_info_format),
    );
    components.insert(
        "output kind changed".into(),
        digest_bytes(invocation.kind.name().as_bytes()),
    );
    let mut hasher = blake3::Hasher::new();
    hash_field(
        &mut hasher,
        "protocol",
        PROTOCOL_VERSION.to_string().as_bytes(),
    );
    hash_field(&mut hasher, "compiler", &compiler.stdout);
    hash_field(&mut hasher, "dep-info-format", dep_info_format);
    hash_field(
        &mut hasher,
        "output-kind",
        invocation.kind.name().as_bytes(),
    );
    for arg in &normalized_args {
        hash_field(&mut hasher, "arg", arg.as_bytes());
    }
    hash_field(
        &mut hasher,
        "remap",
        b"remap-v2:$CARGO_HOME=/bellows/cargo-home;$CHECKOUT=/bellows/checkout;$WORKSPACE=<checkout- or cargo-home-relative>;$TARGET=/bellows/target;$PROFILE=/bellows/profile",
    );
    let path_use = path_use(invocation, host_triple(&compiler.stdout).as_deref());
    for (name, value) in compile_environment(&normalizer, &path_use) {
        hash_field(&mut hasher, &format!("env:{name}"), value.as_bytes());
        components.insert(
            format!("environment changed: {name}"),
            digest_bytes(value.as_bytes()),
        );
    }
    if let PathUse::Linker(program) = path_use {
        let path_env = env::var("PATH").unwrap_or_default();
        let found = match link::resolve_program(program, &path_env, &workspace) {
            Ok(path) => format!("{program}:{}", digests.file(&path)?),
            Err(_) => format!("{program}:not on PATH"),
        };
        hash_field(&mut hasher, "linker-on-path", found.as_bytes());
        components.insert(
            format!("linker on PATH changed: {program}"),
            digest_bytes(found.as_bytes()),
        );
    }
    let own_roots = own_roots(&workspace, checkout.as_deref(), &root_normalizer);
    let mut inputs = invocation.explicit_inputs.clone();
    inputs.extend(resolve_static_libraries(invocation, &workspace)?);
    inputs.sort();
    inputs.dedup();
    for path in inputs {
        let absolute = canonical_compiler_input(&absolute_path(&path, &workspace), &own_roots)
            .with_context(|| format!("canonicalize compiler input {}", path.display()))?;
        let input_digest = digests.file(&absolute)?;
        components.insert(
            format!(
                "explicit input changed: {}",
                normalizer.normalize(&absolute.to_string_lossy())
            ),
            input_digest.clone(),
        );
        hash_field(
            &mut hasher,
            &format!(
                "input:{}",
                normalizer.normalize(&absolute.to_string_lossy())
            ),
            input_digest.as_bytes(),
        );
    }
    let static_key = hasher.finalize().to_hex().to_string();
    let source = invocation
        .explicit_inputs
        .first()
        .map(|p| normalizer.normalize(&absolute_path(p, &workspace).to_string_lossy()))
        .unwrap_or_default();
    let mut pins = vec![(
        format!("{PIN_PREFIX}$WORKSPACE"),
        pin_value(
            &workspace.to_string_lossy(),
            &root_normalizer.spellings("$WORKSPACE"),
        ),
    )];
    if let Some(target) = target_root(&workspace, &invocation.out_dir) {
        pins.push((
            format!("{PIN_PREFIX}$TARGET"),
            pin_value(
                &canonical_base(target).to_string_lossy(),
                &root_normalizer.spellings("$TARGET"),
            ),
        ));
    }
    let checkout_spellings = root_normalizer.spellings("$CHECKOUT");
    if let Some(longest) = checkout_spellings.first() {
        pins.push((
            format!("{PIN_PREFIX}$CHECKOUT"),
            pin_value(longest, &checkout_spellings),
        ));
    }
    let virtual_env = stable_empty_directories(invocation, &root_normalizer);
    Ok(Identity {
        diagnostic_width: diagnostic_width(&invocation.args),
        checkout,
        own_roots,
        virtual_env,
        fingerprint: diagnostics::Fingerprint {
            key: static_key.clone(),
            components,
        },
        diagnostic_group: format!("{}:{source}", invocation.crate_name),
        static_key,
        normalizer,
        root_normalizer,
        workspace,
        digests,
        pins,
    })
}

/// A procedural macro runs only inside rustc, so a checkout directory it
/// bakes in (wit-bindgen bakes its build script's `OUT_DIR` as
/// `DEBUG_OUTPUT_DIR`) is only used while compiling its consumers. When such
/// a directory is empty, a machine-wide empty directory at a checkout-
/// independent path is observably equivalent, and it makes the macro — and
/// through its crate hash every consumer — identical across worktrees.
/// Cargo's own variables and non-empty directories are never substituted.
fn stable_empty_directories(
    invocation: &Invocation,
    root_normalizer: &PathNormalizer,
) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    if !invocation.proc_macro_crate {
        return overrides;
    }
    let Some(root) = user_cache_root().map(|root| root.join("stable-dirs-v1")) else {
        return overrides;
    };
    for (name, value) in env::vars() {
        if name.starts_with("CARGO_") {
            continue;
        }
        let normalized = root_normalizer.normalize(&value);
        if !(normalized.starts_with("$TARGET") || normalized.starts_with("$PROFILE")) {
            continue;
        }
        let empty = fs::read_dir(&value).is_ok_and(|mut entries| entries.next().is_none());
        if !empty {
            continue;
        }
        let stable = root.join(&digest_bytes(normalized.as_bytes())[..32]);
        if fs::create_dir_all(&stable).is_ok() {
            overrides.insert(name, stable.to_string_lossy().into_owned());
        }
    }
    overrides
}

/// The platform user cache, independent of per-workspace `BELLOWS_STATE_DIR`.
fn user_cache_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(root).join("bellows"));
    }
    if let Some(root) = env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(root).join("Bellows"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/bellows"))
}

struct CompilerIdentity {
    stdout: Vec<u8>,
}

/// `rustc -vV` output, memoized per concrete compiler binary. A rustup proxy
/// resolves its toolchain from overrides and environment on every call, so
/// only binaries that are not proxies are memoized.
fn compiler_identity(rustc: &Path, state: &Path) -> Result<CompilerIdentity> {
    let proxy = rustc.parent().is_none_or(|dir| {
        dir.join(format!("rustup{}", env::consts::EXE_SUFFIX))
            .exists()
    });
    let memo = (!proxy && rustc.is_absolute())
        .then(|| fs::metadata(rustc).ok())
        .flatten()
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |time| time.as_nanos());
            let key = digest_bytes(
                format!("{}\0{}\0{modified}", rustc.display(), metadata.len()).as_bytes(),
            );
            state.join("compilers-v1").join(key)
        });
    if let Some(memo) = &memo
        && let Ok(stdout) = fs::read(memo)
        && !stdout.is_empty()
    {
        return Ok(CompilerIdentity { stdout });
    }
    let output = Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        bail!("rustc -vV failed")
    }
    if let Some(memo) = memo {
        let _ = atomic_write(&memo, &output.stdout);
    }
    Ok(CompilerIdentity {
        stdout: output.stdout,
    })
}

/// Files a library compile bundles for `-l static=NAME`: every candidate in
/// every `-L native=` directory, so a newly shadowing archive also misses.
fn resolve_static_libraries(invocation: &Invocation, workspace: &Path) -> Result<Vec<PathBuf>> {
    if invocation.kind != OutputKind::Library || invocation.static_libraries.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for library in &invocation.static_libraries {
        let names = if library.verbatim {
            vec![library.name.clone()]
        } else if invocation.naming.msvc {
            vec![format!("{}.lib", library.name)]
        } else {
            vec![
                format!("lib{}.a", library.name),
                format!("{}.lib", library.name),
            ]
        };
        let before = found.len();
        for dir in &invocation.native_search {
            for name in &names {
                let path = absolute_path(&dir.join(name), workspace);
                if path.is_file() {
                    found.push(path);
                }
            }
        }
        if found.len() == before {
            bail!(
                "static native library {} was not found in the -L native directories",
                library.name
            )
        }
    }
    Ok(found)
}

fn relevant_environment(normalizer: &PathNormalizer) -> BTreeMap<String, String> {
    relevant_environment_of(env::vars(), normalizer, cfg!(windows))
}

/// Windows variable names are case-insensitive, and the same variable reaches
/// a build spelled differently (`Path` from Explorer and PowerShell, `PATH`
/// from a POSIX shell), so on Windows each name is keyed in one spelling.
fn relevant_environment_of(
    vars: impl IntoIterator<Item = (String, String)>,
    normalizer: &PathNormalizer,
    case_insensitive: bool,
) -> BTreeMap<String, String> {
    vars.into_iter()
        .map(|(name, value)| {
            let name = if case_insensitive {
                environment_spelling(&name)
            } else {
                name
            };
            (name, value)
        })
        .filter(|(name, _)| is_relevant_environment_name(name))
        .map(|(name, value)| (name, normalizer.normalize(&value)))
        .collect()
}

/// The spelling a case-insensitive variable name is keyed under: its entry in
/// [`EXACT_ENVIRONMENT`], else upper case (every other list is upper case).
fn environment_spelling(name: &str) -> String {
    EXACT_ENVIRONMENT
        .iter()
        .find(|listed| listed.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_ascii_uppercase(), |listed| (*listed).to_string())
}

/// The environment a compile's key covers: the relevant environment without
/// the variables that describe the shell rather than the compile. Two shells
/// on one machine (a POSIX shell sets HOME, LANG and LC_ALL and prepends its
/// own PATH entries; PowerShell sets none) otherwise never share a result.
/// rustc reads none of them; `env!` and `option_env!` reads are tracked
/// through dep-info. A proc macro can still read them, or run a program from
/// PATH, without any record, so its output can differ between shells and a
/// compile that uses one is shared across them. PATH stays only where it can
/// choose the program rustc runs ([`PathUse`]). Build-script runs keep all of
/// them: a build script may run any program on PATH.
fn compile_environment(
    normalizer: &PathNormalizer,
    path_use: &PathUse,
) -> BTreeMap<String, String> {
    without_shell(relevant_environment(normalizer), path_use)
}

fn without_shell(
    mut environment: BTreeMap<String, String>,
    path_use: &PathUse,
) -> BTreeMap<String, String> {
    for name in SHELL_ENVIRONMENT {
        environment.remove(*name);
    }
    if cfg!(windows) {
        // The Windows loader never reads them.
        environment.remove("LD_LIBRARY_PATH");
        environment.remove("DYLD_LIBRARY_PATH");
    }
    if !matches!(path_use, PathUse::Whole) {
        environment.remove("PATH");
    }
    environment
}

const SHELL_ENVIRONMENT: &[&str] = &["HOME", "LANG", "LC_ALL", "TZ"];

/// What PATH can change about one compile.
#[derive(Debug, PartialEq, Eq)]
enum PathUse {
    /// Nothing: rustc runs no program for a library, and finds a wasm or
    /// Windows-default linker without PATH (the sysroot's rust-lld; Visual
    /// Studio's link.exe through the registry and the keyed VC*/WindowsSdk*
    /// variables).
    None,
    /// Only which program of this name rustc runs as the linker. The key
    /// holds that program's content digest, so another shell's PATH that
    /// finds the same linker shares results, and a different one misses.
    Linker(&'static str),
    /// Anything: a C compiler driver searches PATH for its own tools, and an
    /// explicit `-C linker` may be a script. The key keeps all of PATH.
    Whole,
}

/// The `host:` triple from `rustc -vV` output.
fn host_triple(version: &[u8]) -> Option<String> {
    String::from_utf8_lossy(version)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_owned())
}

/// What PATH can change about this compile, judged from the target rustc
/// links for (its `--target`, else its host) and the environment the linker
/// lookup reads.
fn path_use(invocation: &Invocation, host: Option<&str>) -> PathUse {
    path_use_with(invocation, host, env::var_os("VCINSTALLDIR").is_some())
}

fn path_use_with(invocation: &Invocation, host: Option<&str>, vc_environment: bool) -> PathUse {
    if invocation.kind != OutputKind::Linked {
        return PathUse::None;
    }
    if codegen_value(&invocation.args, "linker").is_some() {
        return PathUse::Whole;
    }
    let flavor = codegen_value(&invocation.args, "linker-flavor");
    match flavor.as_deref() {
        Some("lld-link") => return PathUse::Linker("lld-link"),
        Some(_) => return PathUse::Whole,
        None => {}
    }
    // Without a `--target` rustc links for its own host. When the host is
    // unknown, assume nothing.
    let target = option_value(&invocation.args, "--target");
    let Some(triple) = target.as_deref().or(host) else {
        return PathUse::Whole;
    };
    if triple.starts_with("wasm") && !triple.contains("emscripten") {
        // The sysroot's rust-lld. Emscripten links with `emcc` from PATH.
        PathUse::None
    } else if triple.contains("msvc") {
        // rustc asks the Visual Studio installation for link.exe, which needs
        // no PATH. It takes link.exe from PATH when the Visual Studio
        // environment is loaded, and when it runs on another system (an xwin
        // cross build). A Windows host that finds no Visual Studio also falls
        // back to PATH; that is not detected here.
        if vc_environment || !host.is_some_and(|host| host.contains("windows")) {
            PathUse::Linker("link.exe")
        } else {
            PathUse::None
        }
    } else {
        PathUse::Whole
    }
}

const EXACT_ENVIRONMENT: &[&str] = &[
    "AR",
    "CC",
    "CXX",
    "DEBUG",
    "DYLD_LIBRARY_PATH",
    "HOME",
    "HOST",
    "INCLUDE",
    "LANG",
    "LC_ALL",
    "LD_LIBRARY_PATH",
    "LIB",
    "LIBPATH",
    "LIBRARY_PATH",
    "MACOSX_DEPLOYMENT_TARGET",
    "NM",
    "OBJCOPY",
    "OPT_LEVEL",
    "OUT_DIR",
    "PATH",
    "PROFILE",
    "RANLIB",
    "SDKROOT",
    "SOURCE_DATE_EPOCH",
    "STRIP",
    "TARGET",
    "TZ",
    "UCRTVersion",
    "UniversalCRTSdkDir",
    "VCINSTALLDIR",
    "VCToolsInstallDir",
    "VCToolsVersion",
    "VSINSTALLDIR",
    "WindowsSdkDir",
    "WindowsSDKVersion",
];

fn is_relevant_environment_name(name: &str) -> bool {
    // Scheduling, terminal and rustup-proxy bookkeeping never reach rustc's
    // output. A crate that reads one with env!/option_env! is still tracked
    // exactly through dep-info.
    //
    // CARGO_HOLD_* belong to cargo-hold, a scheduler that runs one heavy
    // build at a time in front of Cargo. It marks everything inside a hold
    // with the holder's label, by default the name of the folder Cargo was
    // started in, so keying it gave every checkout folder a cache of its own.
    const EXCLUDED_CARGO_CONTROL: &[&str] = &[
        "CARGO_BUILD_JOBS",
        "CARGO_HOLD_ACTIVE",
        "CARGO_HOLD_LABEL",
        "CARGO_HOLD_SKIP",
        "CARGO_INCREMENTAL",
        "CARGO_MAKEFLAGS",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_TERM_COLOR",
        "CARGO_TERM_PROGRESS_WHEN",
        "CARGO_TERM_PROGRESS_WIDTH",
        "CARGO_TERM_QUIET",
        "CARGO_TERM_VERBOSE",
        "CLIPPY_TERMINAL_WIDTH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_AUTO_INSTALL",
        "RUSTUP_TOOLCHAIN_SOURCE",
        "RUST_BACKTRACE",
        "RUST_LOG",
        "RUST_MIN_STACK",
        "RUST_RECURSION_COUNT",
        "RUST_TEST_THREADS",
    ];
    const PREFIXES: &[&str] = &[
        "AR_", "CARGO_", "CC_", "CFLAGS", "CLIPPY_", "CPPFLAGS", "CXX_", "CXXFLAGS", "DEP_",
        "LDFLAGS", "RUST",
    ];
    const BELLOWS_CONTROL: &[&str] = &[
        "BELLOWS_AUTH_TOKEN",
        "BELLOWS_READ_ONLY",
        "BELLOWS_DEMO_COMPILE_DELAY_MS",
        "BELLOWS_EVENT_LOG",
        "BELLOWS_LOCAL_ONLY",
        "BELLOWS_MAX_WAIT_MS",
        "BELLOWS_SERVER",
        "BELLOWS_STATE_DIR",
    ];
    !BELLOWS_CONTROL.contains(&name)
        && !EXCLUDED_CARGO_CONTROL.contains(&name)
        && (EXACT_ENVIRONMENT.contains(&name)
            || PREFIXES.iter().any(|prefix| name.starts_with(prefix)))
}

fn hash_field(hasher: &mut blake3::Hasher, name: &str, bytes: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn validate_candidate(
    candidate: &ActionCandidate,
    identity: &Identity,
) -> std::result::Result<(), String> {
    validate_candidate_manifest(candidate).map_err(|error| error.to_string())?;
    let width = candidate
        .env
        .iter()
        .find(|input| input.name == DIAGNOSTIC_WIDTH_INPUT);
    let width_matches = match width {
        Some(input) => [ANY_DIAGNOSTIC_WIDTH, identity.diagnostic_width()]
            .iter()
            .any(|value| {
                input.value_digest.as_deref() == Some(digest_bytes(value.as_bytes()).as_str())
            }),
        None => identity.diagnostic_width.is_none(),
    };
    if !width_matches {
        return Err(
            "environment changed: diagnostic width (cached diagnostics were rendered for another terminal width)"
                .into(),
        );
    }
    // Environment first: it is free, and a checkout pin rejects before any
    // large link input is read. Named variables before pins, so a miss names
    // the variable whose embedded path differs.
    let (pins, named): (Vec<_>, Vec<_>) = candidate
        .env
        .iter()
        .filter(|input| input.name != DIAGNOSTIC_WIDTH_INPUT)
        .partition(|input| input.name.starts_with(PIN_PREFIX));
    for input in named.into_iter().chain(pins) {
        let actual = if input.name.starts_with(PIN_PREFIX) {
            match identity.pin_value(&input.name) {
                Some(value) => EnvInput::capture(&input.name, Some(value)),
                None => return Err(format!("environment changed: {}", input.name)),
            }
        } else {
            let value = identity.env_value(&input.name);
            if input.normalized {
                match value {
                    Some(value) => EnvInput::capture_normalized(
                        &input.name,
                        &identity.root_normalizer.normalize(&value),
                    ),
                    None => EnvInput::capture(&input.name, None),
                }
            } else {
                EnvInput::capture(&input.name, value.as_deref())
            }
        };
        if actual.value_digest != input.value_digest {
            return Err(if input.name.starts_with(PIN_PREFIX) {
                format!(
                    "environment changed: {} (the cached output embeds another checkout's path)",
                    input.name
                )
            } else {
                format!("environment changed: {}", input.name)
            });
        }
    }
    for input in &candidate.files {
        let localized = identity.normalizer.localize(&input.path);
        let path = absolute_path(Path::new(&localized), &identity.workspace);
        if outside_checkout_roots(&input.path)
            && foreign_checkout(&path, identity.checkout.as_deref())
        {
            return Err(format!(
                "input changed: {} belongs to another checkout",
                input.path
            ));
        }
        let actual = build_script::input_digest(&identity.digests, &path)
            .map_err(|_| format!("input disappeared: {}", input.path))?;
        if actual != input.digest {
            return Err(format!("input changed: {}", input.path));
        }
    }
    for input in &candidate.host_files {
        if foreign_checkout(Path::new(&input.path), identity.checkout.as_deref()) {
            return Err(format!(
                "input changed: {} belongs to another checkout",
                input.path
            ));
        }
        let actual = build_script::input_digest(&identity.digests, Path::new(&input.path))
            .map_err(|_| format!("input disappeared: {}", input.path))?;
        if actual != input.digest {
            return Err(format!("input changed: {}", input.path));
        }
    }
    Ok(())
}

fn explain_candidates(identity: &Identity, index: &CandidateIndex) -> String {
    if index.candidates.is_empty() {
        return "no candidate for this static identity".into();
    }
    index
        .candidates
        .iter()
        .filter_map(|candidate| validate_candidate(candidate, identity).err())
        .next()
        .unwrap_or_else(|| "candidate artifacts unavailable or restore failed; see corrupt event for the blob/path error".into())
}

fn try_candidates(
    remote: &Remote,
    l1: Option<&Store>,
    invocation: &Invocation,
    identity: &Identity,
    index: &CandidateIndex,
) -> Result<Option<ExitStatus>> {
    for candidate in &index.candidates {
        if let Err(reason) = validate_candidate(candidate, identity) {
            record_event(
                "candidate_rejected",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &reason,
            );
            continue;
        }
        // The use is recorded while the blobs download rather than after, so
        // it costs no round trip of its own. A candidate that then fails its
        // restore keeps a fresher time than it earned; that only delays its
        // collection.
        let restored = thread::scope(|scope| {
            let used = scope.spawn(|| remote.record_use(candidate));
            let restored = restore(remote, l1, invocation, identity, candidate);
            let _ = used.join();
            restored
        });
        match restored {
            Ok(()) => {
                if let Some(store) = l1 {
                    let _ = store.put_candidate(for_local_store(candidate, identity), 8);
                }
                record_event(
                    "hit",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    &hit_detail("restored remote compiler result", invocation, candidate),
                );
                return Ok(Some(success_status()));
            }
            Err(error) => record_event(
                "corrupt",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("candidate rejected during restore: {error:#}"),
            ),
        }
    }
    Ok(None)
}

fn try_l1_candidates(
    store: &Store,
    remote: Option<&Remote>,
    invocation: &Invocation,
    identity: &Identity,
    index: &CandidateIndex,
) -> Result<Option<ExitStatus>> {
    for candidate in &index.candidates {
        if let Err(reason) = validate_candidate(candidate, identity) {
            record_event(
                "candidate_rejected",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("local cache: {reason}"),
            );
            continue;
        }
        // Both stores collect least recently used records first. The remote
        // use is recorded while the outputs are written, not after.
        let restored = thread::scope(|scope| {
            let used = remote.map(|remote| scope.spawn(|| remote.record_use(candidate)));
            let restored = restore_l1(store, invocation, identity, candidate);
            if let Some(used) = used {
                let _ = used.join();
            }
            restored
        });
        match restored {
            Ok(()) => {
                let _ = store.record_use(&candidate.static_key, &candidate.action_key);
                record_event(
                    "l1_hit",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    &hit_detail(
                        "restored runner-local compiler result",
                        invocation,
                        candidate,
                    ),
                );
                return Ok(Some(success_status()));
            }
            Err(error) => record_event(
                "corrupt",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("L1 candidate rejected: {error:#}"),
            ),
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn success_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn success_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

fn hit_detail(prefix: &str, invocation: &Invocation, candidate: &ActionCandidate) -> String {
    let mut detail = format!("{prefix} ({} output", invocation.kind.name());
    if !candidate.proc_macros.is_empty() {
        detail.push_str(&format!(
            "; proc macros: {}",
            candidate.proc_macros.join(", ")
        ));
    }
    detail.push(')');
    detail
}

fn restore(
    remote: &Remote,
    l1: Option<&Store>,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
) -> Result<()> {
    restore_with(invocation, identity, candidate, |digests| {
        let blobs = remote.blobs(digests)?;
        if let Some(store) = l1 {
            for (digest, bytes) in digests.iter().zip(&blobs) {
                let _ = store.put_blob(digest, bytes);
            }
        }
        Ok(blobs)
    })
}

/// Reads several blobs from a store, in request order.
fn read_blobs(store: &Store, digests: &[&str]) -> Result<Vec<Vec<u8>>> {
    digests
        .iter()
        .map(|digest| store.read_blob(digest))
        .collect()
}

fn restore_l1(
    store: &Store,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
) -> Result<()> {
    restore_with(invocation, identity, candidate, |digests| {
        read_blobs(store, digests)
    })
    .context("L1")
}

/// Verify the manifest describes exactly this unit's outputs, fetch and verify
/// every blob (in one batch, so a remote fetch costs about one round trip),
/// then write each output atomically with its mode.
fn restore_with(
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
    fetch: impl FnOnce(&[&str]) -> Result<Vec<Vec<u8>>>,
) -> Result<()> {
    validate_candidate_manifest(candidate)?;
    let supplied = candidate
        .artifacts
        .iter()
        .map(|artifact| artifact.file_name.as_str())
        .collect::<BTreeSet<_>>();
    for expected in &invocation.expected_names {
        let rmeta_may_be_folded_into_rlib = expected.ends_with(".rmeta")
            && invocation
                .expected_names
                .iter()
                .any(|name| name.ends_with(".rlib"));
        if !supplied.contains(expected.as_str()) && !rmeta_may_be_folded_into_rlib {
            bail!("manifest is missing expected output {expected}")
        }
    }
    for artifact in &candidate.artifacts {
        let allowed = match invocation.kind {
            OutputKind::Library => invocation.expected_names.contains(&artifact.file_name),
            OutputKind::Linked => invocation.owns(&artifact.file_name),
        };
        if !allowed {
            bail!("manifest contains unexpected output {}", artifact.file_name)
        }
    }
    let digests = candidate
        .artifacts
        .iter()
        .map(|artifact| artifact.digest.as_str())
        .chain([
            candidate.stdout.digest.as_str(),
            candidate.stderr.digest.as_str(),
        ])
        .collect::<Vec<_>>();
    let mut blobs = fetch(&digests)?;
    if blobs.len() != digests.len() {
        bail!(
            "blob fetch returned {} of {} blobs",
            blobs.len(),
            digests.len()
        )
    }
    let stderr_blob = blobs.pop().expect("stderr requested");
    let stdout_blob = blobs.pop().expect("stdout requested");
    let downloaded = candidate.artifacts.iter().zip(blobs).collect::<Vec<_>>();
    if stdout_blob.len() as u64 != candidate.stdout.len
        || stderr_blob.len() as u64 != candidate.stderr.len
    {
        bail!("compiler stream length does not match manifest")
    }
    for (artifact, stored) in downloaded {
        let bytes = if artifact.file_name.ends_with(".d") {
            localize_env_deps(
                &transform_dep_info(&stored, &identity.normalizer, true),
                candidate,
                identity,
            )
        } else {
            stored
        };
        let destination = invocation.out_dir.join(&artifact.file_name);
        install_output(&destination, &bytes)?;
        set_file_executable(&destination, artifact.executable)?;
        if !artifact.file_name.ends_with(".d") {
            identity.digests.remember(&destination, &artifact.digest);
        }
    }
    let stdout = transform_compiler_stream(&stdout_blob, &identity.normalizer, true);
    let stderr = transform_compiler_stream(&stderr_blob, &identity.normalizer, true);
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

/// Replace an output file. Windows refuses to replace an executable image
/// that is running (a test binary still executing, or a loaded DLL) but
/// allows renaming it; move it aside and sweep leftovers on later restores.
fn install_output(destination: &Path, bytes: &[u8]) -> Result<()> {
    match atomic_write(destination, bytes) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error) if destination.exists() => {
            let parent = destination.parent().context("output has no parent")?;
            let aside = parent.join(format!(
                ".bellows-replaced-{}-{}-{}",
                std::process::id(),
                now_ms(),
                destination
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("output")
            ));
            fs::rename(destination, &aside).with_context(|| {
                format!("replace in-use output {}: {error:#}", destination.display())
            })?;
            atomic_write(destination, bytes)?;
            sweep_replaced(parent);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sweep_replaced(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".bellows-replaced-"))
            {
                // Still-running images stay locked and are retried later.
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

struct Captured {
    candidate: ActionCandidate,
    blobs: BTreeMap<String, Vec<u8>>,
    /// The outputs embed a checkout root; the candidate carries pin inputs.
    pinned: bool,
}

fn compile_and_capture(
    invocation: &Invocation,
    identity: &Identity,
) -> Result<(ExitStatus, Option<Captured>)> {
    if let Some(delay) = env::var("BELLOWS_DEMO_COMPILE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        thread::sleep(Duration::from_millis(delay.min(10_000)));
    }
    // Incremental policy: publish only a from-scratch incremental compile.
    // A compile that reused an existing session could carry state from
    // another source version; its result stays in this checkout.
    //
    // Cargo shares one incremental directory between all units, where a
    // crate's lib, test harness and binaries all create `{crate}-*` sessions
    // concurrently. Each unit gets its own subdirectory instead, named so
    // `cargo clean -p` still removes it, which makes "from scratch" exact.
    let unit_incremental = invocation.incremental.as_deref().map(|dir| {
        absolute_path(dir, &identity.workspace).join(format!(
            "{}-bellows{}",
            invocation.crate_name,
            invocation
                .unit
                .strip_prefix(&invocation.crate_name)
                .unwrap_or_default()
        ))
    });
    let reused_session = unit_incremental
        .as_deref()
        .is_some_and(|dir| fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()));
    let mut args = Vec::with_capacity(invocation.args.len() + 6);
    let mut rewrite_next = false;
    for arg in &invocation.args {
        let replacement = unit_incremental
            .as_ref()
            .map(|dir| format!("incremental={}", dir.display()));
        if std::mem::take(&mut rewrite_next) && arg.starts_with("incremental=") {
            args.push(replacement.unwrap_or_else(|| arg.clone()));
        } else if arg.starts_with("-Cincremental=") {
            args.push(format!(
                "-C{}",
                replacement.unwrap_or_else(|| arg[2..].to_owned())
            ));
        } else {
            rewrite_next = arg == "-C";
            args.push(arg.clone());
        }
    }
    for (spelling, virtual_root) in remap_plan(identity) {
        args.push("--remap-path-prefix".into());
        args.push(format!("{spelling}={virtual_root}"));
    }
    // rustc writes the exact linker command to a file, leaving the stdout
    // Cargo reads untouched.
    let link_record = if invocation.kind == OutputKind::Linked {
        fs::create_dir_all(&invocation.out_dir).context("create output directory")?;
        let record = tempfile::Builder::new()
            .prefix(".bellows-link-")
            .tempfile_in(&invocation.out_dir)
            .context("create link-args record")?
            .into_temp_path();
        args.push(format!("--print=link-args={}", record.display()));
        Some(record)
    } else {
        None
    };
    let started = std::time::SystemTime::now();
    let (mut command, _response_file) = rustc_command(invocation, &args)?;
    let mut child = command
        .envs(&identity.virtual_env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start rustc")?;
    let stdout = child.stdout.take().context("capture rustc stdout")?;
    let stderr = child.stderr.take().context("capture rustc stderr")?;
    let roots = virtual_roots(identity);
    let stderr_roots = roots.clone();
    let stdout_thread = thread::spawn(move || tee_devirtualized(stdout, std::io::stdout(), &roots));
    let stderr_thread =
        thread::spawn(move || tee_devirtualized(stderr, std::io::stderr(), &stderr_roots));
    let status = child.wait()?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("stdout relay panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("stderr relay panicked"))??;
    if !status.success() {
        return Ok((status, None));
    }
    let link_command = match &link_record {
        Some(record) => Some(fs::read_to_string(record).context("read link-args record")?),
        None => None,
    };
    drop(link_record);
    let result = if reused_session {
        Err(anyhow::Error::new(NotStored(
            "incremental compile reused an existing session; result stays in this checkout".into(),
        )))
    } else {
        capture_outputs(
            invocation,
            identity,
            stdout,
            stderr,
            started,
            link_command.as_deref(),
        )
    };
    let captured = match result {
        Ok(captured) => Some(captured),
        Err(error) => {
            if let Some(reason) = error.downcast_ref::<NotStored>() {
                record_event(
                    "not_stored",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("compiled but not stored: {reason}"),
                );
            } else {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("capture skipped after successful rustc: {error:#}"),
                );
            }
            None
        }
    };
    Ok((status, captured))
}

fn not_stored(reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(NotStored(reason.into()))
}

// rustc's JSON diagnostics/artifact notifications contain escaped strings.
// Replacing raw bytes can inject unescaped Windows backslashes into JSON.
fn transform_compiler_stream(bytes: &[u8], normalizer: &PathNormalizer, localize: bool) -> Vec<u8> {
    fn visit(value: &mut serde_json::Value, normalizer: &PathNormalizer, localize: bool) {
        match value {
            serde_json::Value::String(text) => {
                *text = if localize {
                    normalizer.localize(text)
                } else {
                    normalizer.normalize(text)
                };
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, normalizer, localize);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    visit(value, normalizer, localize);
                }
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if let Ok(mut value @ serde_json::Value::Object(_)) = serde_json::from_slice(line) {
            visit(&mut value, normalizer, localize);
            result.extend(serde_json::to_vec(&value).expect("serialize compiler JSON"));
            if line.ends_with(b"\r\n") {
                result.extend_from_slice(b"\r\n");
            } else if line.ends_with(b"\n") {
                result.push(b'\n');
            }
        } else {
            result.extend(if localize {
                normalizer.localize_bytes(line)
            } else {
                normalizer.normalize_bytes(line)
            });
        }
    }
    result
}

fn transform_dep_info(bytes: &[u8], normalizer: &PathNormalizer, localize: bool) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    bellows_core::rewrite_dep_info(text, |path| {
        if localize {
            normalizer.localize(path)
        } else {
            normalizer.normalize(path)
        }
    })
    .into_bytes()
}

/// Whether `path` may have changed after rustc read it. Inputs are hashed when
/// rustc has finished; a file written during the compile (an editor save, a
/// `git checkout` into the same worktree) would otherwise be recorded with the
/// artifact compiled from its earlier content, and every later build of the new
/// content would restore the old code. A safe miss: anything modified at or
/// after the compile started, less the slack its timestamps need, is not
/// stored. The change time (Unix ctime, NTFS ChangeTime) moves on every write
/// or rename and no tool can set it, so it also catches a replacement that
/// keeps an older modification time (`cp -p`, `rsync -t`, archive extraction).
/// A timestamp that cannot be read counts as changed.
fn changed_during_compile(path: &Path, started: std::time::SystemTime) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    [metadata.modified().ok(), change_time(path, &metadata)]
        .into_iter()
        .any(|stamp| match stamp {
            Some(stamp) => stamp >= started - timestamp_slack(stamp),
            None => true,
        })
}

#[cfg(unix)]
fn change_time(_path: &Path, metadata: &fs::Metadata) -> Option<std::time::SystemTime> {
    use std::os::unix::fs::MetadataExt;
    let seconds = u64::try_from(metadata.ctime()).ok()?;
    let nanos = u32::try_from(metadata.ctime_nsec()).ok()?;
    Some(std::time::UNIX_EPOCH + Duration::new(seconds, nanos))
}

#[cfg(windows)]
fn change_time(path: &Path, _metadata: &fs::Metadata) -> Option<std::time::SystemTime> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct FileBasicInfo {
        creation_time: i64,
        last_access_time: i64,
        last_write_time: i64,
        change_time: i64,
        file_attributes: u32,
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
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // also opens directories
    const FILE_BASIC_INFO_CLASS: i32 = 0;
    // 100 ns intervals between 1601-01-01 and the Unix epoch.
    const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = FileBasicInfo::default();
    // SAFETY: `file` is an open handle and `info` is a FILE_BASIC_INFO-sized
    // buffer that outlives the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FILE_BASIC_INFO_CLASS,
            (&mut info as *mut FileBasicInfo).cast(),
            std::mem::size_of::<FileBasicInfo>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let since_epoch = u64::try_from(info.change_time - UNIX_EPOCH_AS_FILETIME).ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_nanos(since_epoch.checked_mul(100)?))
}

#[cfg(not(any(unix, windows)))]
fn change_time(_path: &Path, _metadata: &fs::Metadata) -> Option<std::time::SystemTime> {
    None
}

/// How far a file timestamp may trail the clock. Kernels stamp files from a
/// coarse clock (a scheduler tick on Linux, about 15.6 ms on Windows); some
/// filesystems keep whole seconds (ext3, HFS+) or two (FAT), and their stamps
/// have no sub-second part.
fn timestamp_slack(stamp: std::time::SystemTime) -> Duration {
    let whole_second = stamp
        .duration_since(std::time::UNIX_EPOCH)
        .is_ok_and(|since| since.subsec_nanos() == 0);
    if whole_second {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(50)
    }
}

/// Outputs of a linked unit: every regular file named for this unit that
/// rustc wrote during the compile. The predicted primary outputs must be
/// among them; directory outputs (`.dSYM`) are not modeled.
fn discover_linked_outputs(
    invocation: &Invocation,
    started: std::time::SystemTime,
) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    // Allow for filesystems whose timestamps are coarser than the clock.
    let threshold = started - Duration::from_secs(2);
    for entry in fs::read_dir(&invocation.out_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !invocation.owns(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let fresh = metadata.modified().is_ok_and(|time| time >= threshold);
        if !fresh {
            continue;
        }
        if !metadata.is_file() {
            return Err(not_stored(format!(
                "rustc produced a non-file output {name} (split debuginfo directories are not modeled)"
            )));
        }
        names.insert(name);
    }
    for expected in &invocation.expected_names {
        if !names.contains(expected) {
            bail!("rustc succeeded but expected output {expected} is absent")
        }
    }
    Ok(names)
}

/// rustc records each `env!` value in dep-info. A result shared through a
/// normalized (checkout-relative) value must carry this checkout's value, as
/// rustc would have written it here, or Cargo would compare against the
/// producer's path.
fn localize_env_deps(dep_info: &[u8], candidate: &ActionCandidate, identity: &Identity) -> Vec<u8> {
    let normalized = candidate
        .env
        .iter()
        .filter(|input| input.normalized)
        .map(|input| input.name.as_str())
        .collect::<BTreeSet<_>>();
    if normalized.is_empty() {
        return dep_info.to_vec();
    }
    let Ok(text) = std::str::from_utf8(dep_info) else {
        return dep_info.to_vec();
    };
    let mut result = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let rewritten = body
            .strip_prefix("# env-dep:")
            .and_then(|rest| rest.split_once('='))
            .filter(|(name, _)| normalized.contains(name))
            .and_then(|(name, _)| {
                identity
                    .env_value(name)
                    .map(|value| format!("# env-dep:{name}={}", escape_dep_env(&value)))
            });
        result.push_str(rewritten.as_deref().unwrap_or(body));
        result.push_str(ending);
    }
    result.into_bytes()
}

/// rustc's escaping of `# env-dep:` values.
fn escape_dep_env(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\\' => escaped.push_str("\\\\"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn capture_outputs(
    invocation: &Invocation,
    identity: &Identity,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    started: std::time::SystemTime,
    link_command: Option<&str>,
) -> Result<Captured> {
    let names = match invocation.kind {
        OutputKind::Library => invocation.expected_names.clone(),
        OutputKind::Linked => discover_linked_outputs(invocation, started)?,
    };
    // Each root is scanned separately: a registry crate's "workspace" is its
    // registry directory, identical in every checkout, and pinning it must
    // not also pin the per-checkout target directory.
    let scanners = ["$CHECKOUT", "$WORKSPACE", "$TARGET"].map(|token| {
        (
            token,
            leak::Scanner::new(&identity.root_normalizer.spellings(token)),
        )
    });
    let mut leaked = BTreeSet::<&str>::new();
    let mut blobs = BTreeMap::new();
    let mut artifacts = Vec::new();
    let mut dep_text = None;
    for name in &names {
        let path = invocation.out_dir.join(name);
        if !path.is_file() {
            if name.ends_with(".rmeta") && names.iter().any(|n| n.ends_with(".rlib")) {
                continue;
            }
            bail!(
                "rustc succeeded but expected output {} is absent",
                path.display()
            )
        }
        let raw = fs::read(&path)?;
        let stored = if name.ends_with(".d") {
            let normalized = transform_dep_info(&raw, &identity.normalizer, false);
            dep_text = Some(String::from_utf8(raw).context("dep-info is not UTF-8")?);
            normalized
        } else {
            if name.ends_with(".rlib") {
                check_bundled_members(invocation, &raw, &identity.workspace)?;
            }
            // Linker-written companions (program databases, export files,
            // import libraries) record the linker's working directory and
            // module paths; see `leaks_in_program_database`.
            let lower = name.to_ascii_lowercase();
            let program_database = invocation.kind == OutputKind::Linked
                && [".pdb", ".exp", ".lib"]
                    .iter()
                    .any(|extension| lower.ends_with(extension));
            for (token, scanner) in &scanners {
                let leaks = if program_database {
                    scanner.leaks_in_program_database(&raw)
                } else {
                    scanner.leaks(&raw)
                };
                if !leaked.contains(token) && leaks {
                    leaked.insert(token);
                }
            }
            raw
        };
        let digest = digest_bytes(&stored);
        if !name.ends_with(".d") {
            identity.digests.remember(&path, &digest);
        }
        let executable = is_executable(&path)?;
        blobs.insert(digest.clone(), stored);
        artifacts.push(Artifact {
            file_name: name.clone(),
            digest,
            executable,
        });
    }
    let dep_text = dep_text.context("rustc did not produce dep-info")?;
    let (dep_files, dep_env) = parse_dep_info(&dep_text);
    let mut file_paths = invocation.explicit_inputs.clone();
    file_paths.extend(dep_files.into_iter().map(PathBuf::from));
    file_paths.extend(resolve_static_libraries(invocation, &identity.workspace)?);
    let mut files = Vec::new();
    let mut host_files = Vec::new();
    let mut record = |absolute: &Path, host: bool| -> Result<()> {
        let normalized = identity.normalizer.normalize(&absolute.to_string_lossy());
        if outside_checkout_roots(&normalized)
            && foreign_checkout(absolute, identity.checkout.as_deref())
        {
            return Err(not_stored(format!(
                "reads {} from another checkout",
                absolute.display()
            )));
        }
        // Cargo's own outputs (dependency artifacts, build-script OUT_DIRs)
        // are written by earlier units of this build under its target-dir
        // lock, and are complete before a unit that reads them starts.
        let produced_by_cargo = ["$TARGET", "$PROFILE"]
            .iter()
            .any(|root| normalized.starts_with(root));
        if !produced_by_cargo && changed_during_compile(absolute, started) {
            return Err(not_stored(format!(
                "{} changed during the compile",
                absolute.display()
            )));
        }
        let digest = identity.digests.file(absolute)?;
        if bellows_core::validate_normalized_input_path(&normalized).is_ok() {
            files.push(FileInput {
                path: normalized,
                digest,
            });
        } else if host {
            host_files.push(FileInput {
                path: absolute.to_string_lossy().into_owned(),
                digest,
            });
        } else {
            bail!(
                "compiler input outside every normalized root: {}",
                absolute.display()
            )
        }
        Ok(())
    };
    file_paths.sort();
    file_paths.dedup();
    for path in file_paths {
        let absolute = absolute_path(&path, &identity.workspace);
        if !absolute.is_file() {
            bail!(
                "rustc dependency disappeared before capture: {}",
                absolute.display()
            )
        }
        let absolute = canonical_compiler_input(&absolute, &identity.own_roots)
            .with_context(|| format!("canonicalize rustc dependency {}", absolute.display()))?;
        record(&absolute, true)?;
    }
    if let Some(text) = link_command {
        let command = link::parse_link_command(text).context("parse linker command")?;
        let sysroot = sysroot(&invocation.rustc)?;
        let outputs = |path: &Path| {
            path.parent() == Some(invocation.out_dir.as_path())
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| invocation.owns(name))
        };
        let sysroot = sysroot.canonicalize().unwrap_or(sysroot);
        let inputs = link::link_inputs(
            &command,
            &link::LinkContext {
                cwd: &identity.workspace,
                msvc: invocation.naming.msvc,
                sysroot: &sysroot,
                outputs: &outputs,
            },
        )?;
        for path in inputs.files {
            // Toolchain files are identified by `rustc -vV`; symlinked system
            // libraries (libfoo.so -> libfoo.so.1) are recorded by content.
            let absolute = absolute_path(&path, &identity.workspace);
            let resolved = absolute.canonicalize().unwrap_or(absolute);
            if resolved.starts_with(&sysroot) {
                continue;
            }
            record(&resolved, true)?;
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    host_files.sort_by(|a, b| a.path.cmp(&b.path));
    host_files.dedup_by(|a, b| a.path == b.path);
    let mut env_inputs = dep_env
        .into_iter()
        .map(|(name, _)| {
            let value = identity.env_value(&name);
            match value {
                Some(value) => {
                    let normalized = identity.root_normalizer.normalize(&value);
                    // Compare normalized only when no output embeds a root
                    // this value names (the profile directory is inside the
                    // target directory).
                    let names_leaked_root = leaked.iter().any(|token| {
                        normalized.contains(token)
                            || (*token == "$TARGET" && normalized.contains("$PROFILE"))
                    });
                    if normalized != value && !names_leaked_root {
                        EnvInput::capture_normalized(&name, &normalized)
                    } else {
                        EnvInput::capture(&name, Some(&value))
                    }
                }
                None => EnvInput::capture(&name, None),
            }
        })
        .collect::<Vec<_>>();
    let pinned = !leaked.is_empty();
    env_inputs.extend(
        identity
            .pins
            .iter()
            .filter(|(name, _)| leaked.iter().any(|token| name.ends_with(token)))
            .map(|(name, value)| EnvInput::capture(name, Some(value))),
    );
    let width = if has_rendered_diagnostics(&stdout) || has_rendered_diagnostics(&stderr) {
        identity.diagnostic_width()
    } else {
        ANY_DIAGNOSTIC_WIDTH
    };
    env_inputs.push(EnvInput::capture(DIAGNOSTIC_WIDTH_INPUT, Some(width)));
    env_inputs.sort_by(|a, b| a.name.cmp(&b.name));
    env_inputs.dedup_by(|a, b| a.name == b.name);
    let normalized_stdout = transform_compiler_stream(&stdout, &identity.normalizer, false);
    let normalized_stderr = transform_compiler_stream(&stderr, &identity.normalizer, false);
    let stdout_digest = digest_bytes(&normalized_stdout);
    let stderr_digest = digest_bytes(&normalized_stderr);
    let stdout_len = normalized_stdout.len() as u64;
    let stderr_len = normalized_stderr.len() as u64;
    blobs.insert(stdout_digest.clone(), normalized_stdout);
    blobs.insert(stderr_digest.clone(), normalized_stderr);
    let action_key = compiler_action_key(&identity.static_key, &files, &host_files, &env_inputs);
    let pinned_checkouts = pinned_checkouts(identity, &env_inputs);
    let candidate = ActionCandidate {
        protocol: PROTOCOL_VERSION,
        static_key: identity.static_key.clone(),
        action_key,
        crate_name: invocation.crate_name.clone(),
        created_ms: now_ms(),
        files,
        host_files,
        env: env_inputs,
        artifacts,
        stdout: StreamArtifact {
            digest: stdout_digest,
            len: stdout_len,
        },
        stderr: StreamArtifact {
            digest: stderr_digest,
            len: stderr_len,
        },
        proc_macros: invocation.proc_macros.clone(),
        pinned_checkouts,
    };
    validate_candidate_manifest(&candidate).map_err(|error| {
        if error.to_string().contains("cardinality") {
            not_stored(format!("{error:#}"))
        } else {
            error
        }
    })?;
    Ok(Captured {
        candidate,
        blobs,
        pinned,
    })
}

fn sysroot(rustc: &Path) -> Result<PathBuf> {
    let output = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .context("run rustc --print sysroot")?;
    if !output.status.success() {
        bail!("rustc --print sysroot failed")
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

/// rustc bundles `-l static` archives, and archives named by `#[link(kind =
/// "static")]` in source, into the rlib. Only archives resolved from `-l`
/// are inputs; any other native member means an unmodeled bundled library.
fn check_bundled_members(invocation: &Invocation, rlib: &[u8], workspace: &Path) -> Result<()> {
    let members = archive::member_names(rlib).context("read rlib members")?;
    let foreign = members
        .into_iter()
        .filter(|name| name != "lib.rmeta" && !name.ends_with(".rcgu.o") && !name.starts_with('/'))
        .collect::<BTreeSet<_>>();
    if foreign.is_empty() {
        return Ok(());
    }
    let mut declared = BTreeSet::new();
    for library in resolve_static_libraries(invocation, workspace).unwrap_or_default() {
        if let Ok(bytes) = fs::read(&library)
            && let Ok(names) = archive::member_names(&bytes)
        {
            declared.extend(names);
        }
    }
    let undeclared = foreign
        .iter()
        .filter(|name| !declared.contains(*name))
        .collect::<Vec<_>>();
    // `#[link(kind = "raw-dylib")]` makes rustc synthesize import members
    // named after the DLL from the crate's own source, which the key already
    // covers. A bundled `kind = "static"` import library could carry the same
    // names, but rustc must then read it from a native search directory, so a
    // DLL-named member is accepted only when no archive there supplies it.
    let is_dll = |name: &str| name.to_ascii_lowercase().ends_with(".dll");
    let searched = if undeclared.iter().any(|name| is_dll(name)) {
        searched_archive_members(&invocation.native_search)
    } else {
        BTreeSet::new()
    };
    if let Some(unknown) = undeclared
        .into_iter()
        .find(|name| !is_dll(name) || searched.contains(&name.to_ascii_lowercase()))
    {
        return Err(not_stored(format!(
            "rlib bundles native member {unknown} that no -l static library supplies (#[link] in source)"
        )));
    }
    Ok(())
}

/// Lower-cased member names of every archive directly in the native search
/// directories (unreadable entries are skipped: they cannot be bundled).
fn searched_archive_members(dirs: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(bytes) = fs::read(&path)
                && let Ok(members) = archive::member_names(&bytes)
            {
                names.extend(members.into_iter().map(|name| name.to_ascii_lowercase()));
            }
        }
    }
    names
}

/// Each root, spelled the way rustc sees it, and the checkout-independent
/// path rustc embeds in its place, least specific first (rustc applies the
/// last matching mapping).
///
/// A virtual path must mean the same directory in every compile of a
/// session: a dependency's metadata is read by its consumers, which reverse
/// the mapping to render source snippets. Cargo runs rustc from a path
/// dependency's own directory, so the working directory is remapped through
/// its place in the session checkout (`/bellows/checkout/crates/x`) or in
/// Cargo's home (`/bellows/cargo-home/registry/…`) rather than one shared
/// `/bellows/workspace`. rustc reverses a mapping only when exactly one
/// mapping produces the virtual path, so each root is remapped from a single
/// spelling: Cargo's (rustc's working directory and its ancestors, or the
/// as-given form), never the canonical one when they differ (Windows 8.3
/// names such as `RUNNER~1`, symlinked directories). Other spellings stay in
/// the normalizer and the leak scan, so an occurrence of them pins rather
/// than shares.
fn remap_plan(identity: &Identity) -> Vec<(String, String)> {
    let cwd = env::current_dir().ok();
    let usable = |spellings: Vec<String>| {
        spellings
            .into_iter()
            .filter(|spelling| {
                let verbatim = spelling.starts_with(r"\\?\");
                let forward = cfg!(windows) && spelling.contains('/');
                !(verbatim || forward)
            })
            .collect::<Vec<_>>()
    };
    // Cargo's spelling: one that differs from its own canonical form. On
    // Windows canonicalize() adds a verbatim prefix, and the normalizer also
    // records that path's ordinary drive form; neither is Cargo's spelling, so
    // compare against the ordinary form (an 8.3 name or a symlink still differs).
    let as_given = |spellings: &[String]| {
        spellings
            .iter()
            .find(|spelling| {
                Path::new(spelling)
                    .canonicalize()
                    .is_ok_and(|canonical| ordinary_spelling(&canonical) != **spelling)
            })
            .or_else(|| spellings.first())
            .cloned()
    };
    let canonical = |spellings: &[String]| {
        spellings
            .iter()
            .find_map(|spelling| Path::new(spelling).canonicalize().ok())
    };
    let relative = |path: &Path, root: &Path| {
        path.strip_prefix(root).ok().map(|rest| {
            rest.components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
    };
    // Native separators below the virtual root, as rustc joins them.
    let under = |base: &str, parts: Vec<String>| {
        std::iter::once(base.to_owned())
            .chain(parts)
            .collect::<Vec<_>>()
            .join(std::path::MAIN_SEPARATOR_STR)
    };
    let cargo_home = usable(identity.normalizer.spellings("$CARGO_HOME"));
    let checkout = usable(identity.root_normalizer.spellings("$CHECKOUT"));
    let workspace = &identity.workspace;
    let checkout_root = canonical(&checkout);
    let workspace_virtual = match (&checkout_root, canonical(&cargo_home)) {
        (Some(root), _) if relative(workspace, root).is_some() => {
            under("/bellows/checkout", relative(workspace, root).unwrap())
        }
        (_, Some(root)) if relative(workspace, &root).is_some() => {
            under("/bellows/cargo-home", relative(workspace, &root).unwrap())
        }
        // Without a session checkout the working directory is the checkout.
        (None, _) => "/bellows/checkout".to_owned(),
        _ => "/bellows/workspace".to_owned(),
    };
    // rustc's working directory is exactly the wrapper's; the checkout is the
    // matching ancestor of it.
    let workspace_spelling = cwd
        .as_ref()
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .or_else(|| as_given(&usable(identity.root_normalizer.spellings("$WORKSPACE"))));
    let checkout_spelling = checkout_root
        .as_ref()
        .and_then(|root| given_spelling(workspace, root))
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| as_given(&checkout));
    vec![
        (as_given(&cargo_home), "/bellows/cargo-home".to_owned()),
        (checkout_spelling, "/bellows/checkout".to_owned()),
        (workspace_spelling, workspace_virtual),
        (
            as_given(&usable(identity.root_normalizer.spellings("$TARGET"))),
            "/bellows/target".to_owned(),
        ),
        (
            as_given(&usable(identity.root_normalizer.spellings("$PROFILE"))),
            "/bellows/profile".to_owned(),
        ),
    ]
    .into_iter()
    .filter_map(|(spelling, virtual_root)| spelling.map(|spelling| (spelling, virtual_root)))
    .collect()
}

/// Remapping keeps checkout paths out of objects and debuginfo, but rustc
/// also applies it to diagnostics. Messages are rewritten back to this
/// checkout's real paths, as rustc spells them, so compiler output (and
/// snapshot tests of it, such as trybuild) reads exactly as without Bellows.
/// Stored streams are then normalized from the real paths and localized
/// again on every replay.
fn virtual_roots(identity: &Identity) -> Vec<(String, String)> {
    let mut roots = remap_plan(identity)
        .into_iter()
        .map(|(real, virtual_root)| (virtual_root, real))
        .collect::<Vec<_>>();
    // Longest virtual prefix first: `/bellows/checkout/crates/x` before
    // `/bellows/checkout`.
    roots.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    roots.dedup_by(|a, b| a.0 == b.0);
    roots
}

fn devirtualize_line(line: &[u8], roots: &[(String, String)]) -> Vec<u8> {
    let mentions = |bytes: &[u8]| {
        roots
            .iter()
            .any(|(virtual_root, _)| memchr::memmem::find(bytes, virtual_root.as_bytes()).is_some())
    };
    if !mentions(line) {
        return line.to_vec();
    }
    let replace = |text: &str| {
        roots
            .iter()
            .fold(text.to_owned(), |text, (virtual_root, real)| {
                text.replace(virtual_root, real)
            })
    };
    fn visit(value: &mut serde_json::Value, replace: &dyn Fn(&str) -> String) {
        match value {
            serde_json::Value::String(text) => *text = replace(text),
            serde_json::Value::Array(values) => values.iter_mut().for_each(|v| visit(v, replace)),
            serde_json::Value::Object(values) => {
                values.values_mut().for_each(|v| visit(v, replace))
            }
            _ => {}
        }
    }
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    let ending = &line[body.len()..];
    if let Ok(mut value @ serde_json::Value::Object(_)) = serde_json::from_slice(body) {
        visit(&mut value, &replace);
        let mut result = serde_json::to_vec(&value).expect("serialize compiler JSON");
        result.extend_from_slice(ending);
        return result;
    }
    match std::str::from_utf8(line) {
        Ok(text) => replace(text).into_bytes(),
        Err(_) => line.to_vec(),
    }
}

fn tee_devirtualized(
    mut reader: impl Read,
    mut writer: impl Write,
    roots: &[(String, String)],
) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line = devirtualize_line(&pending[..=end], roots);
            writer.write_all(&line)?;
            captured.extend_from_slice(&line);
            pending.drain(..=end);
        }
        writer.flush()?;
    }
    if !pending.is_empty() {
        let line = devirtualize_line(&pending, roots);
        writer.write_all(&line)?;
        writer.flush()?;
        captured.extend_from_slice(&line);
    }
    Ok(captured)
}

fn tee(mut reader: impl Read, mut writer: impl Write) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        writer.flush()?;
        captured.extend_from_slice(&buffer[..count]);
    }
    Ok(captured)
}

fn publish(remote: &Remote, captured: Captured) -> Result<String> {
    validate_candidate_manifest(&captured.candidate).context("validate captured candidate")?;
    remote.put_blobs(captured.blobs)?;
    let action_key = captured.candidate.action_key.clone();
    remote.put_candidate(&captured.candidate)?;
    Ok(action_key)
}

/// The checkouts a candidate carrying these inputs can only be reused from:
/// for each root it is pinned to, the checkout containing that root. A root
/// outside every checkout names none (a registry package, a target directory
/// shared by several checkouts), so such a pin is never judged by whether a
/// directory exists. A target directory inside a checkout names the checkout,
/// not itself: `cargo clean` removes it, and the next build puts it back.
pub(crate) fn pinned_checkouts(identity: &Identity, inputs: &[EnvInput]) -> Vec<PinnedCheckout> {
    let mut checkouts = inputs
        .iter()
        .filter(|input| input.name.starts_with(PIN_PREFIX))
        .filter_map(|input| identity.pin_value(&input.name))
        // The root's canonical path, before any other spelling of it.
        .filter_map(|root| checkout_root(Path::new(root.lines().next()?)))
        .filter_map(|checkout| PinnedCheckout::locate(&checkout))
        .collect::<Vec<_>>();
    checkouts.sort();
    checkouts.dedup();
    checkouts.truncate(bellows_core::MAX_PINNED_CHECKOUTS);
    checkouts
}

/// A candidate as a server stores it: without the checkouts it is pinned to,
/// which mean something only on the machine that holds them.
fn for_server(candidate: &ActionCandidate) -> ActionCandidate {
    ActionCandidate {
        pinned_checkouts: Vec::new(),
        ..candidate.clone()
    }
}

/// A candidate restored from a server, as this machine's local store keeps
/// it: pinned to this compile's checkouts. Its pins matched this compile's
/// roots, or it would not have been restored.
pub(crate) fn for_local_store(candidate: &ActionCandidate, identity: &Identity) -> ActionCandidate {
    ActionCandidate {
        pinned_checkouts: pinned_checkouts(identity, &candidate.env),
        ..candidate.clone()
    }
}

fn cache_captured(store: &Store, captured: &Captured) -> Result<()> {
    for (digest, bytes) in &captured.blobs {
        store.put_blob(digest, bytes)?;
    }
    store.put_candidate(captured.candidate.clone(), 8)?;
    Ok(())
}

fn is_executable(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

fn state_dir(workspace: &Path) -> PathBuf {
    env::var_os("BELLOWS_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(".bellows"))
}

fn event_log_path() -> PathBuf {
    env::var_os("BELLOWS_EVENT_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let workspace = env::var_os("BELLOWS_WORKSPACE")
                .map(PathBuf::from)
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            state_dir(&workspace).join("events.jsonl")
        })
}

fn record_event(
    kind: &str,
    crate_name: &str,
    static_key: Option<&str>,
    action_key: Option<&str>,
    detail: &str,
) {
    record_event_duration(kind, crate_name, static_key, action_key, detail, None);
}

fn record_event_duration(
    kind: &str,
    crate_name: &str,
    static_key: Option<&str>,
    action_key: Option<&str>,
    detail: &str,
    duration_ms: Option<u64>,
) {
    let event = Event {
        timestamp_ms: now_ms(),
        kind: kind.into(),
        crate_name: crate_name.into(),
        static_key: static_key.map(str::to_owned),
        action_key: action_key.map(str::to_owned),
        detail: detail.into(),
        reason: Some(diagnostics::reason_code(kind, detail).into()),
        session_id: env::var("BELLOWS_SESSION_ID").ok(),
        workspace: env::var("BELLOWS_WORKSPACE").ok(),
        duration_ms,
    };
    if let Err(error) = diagnostics::append(&event_log_path(), &event) {
        eprintln!("Bellows diagnostics unavailable: {error:#}");
    }
    // Cargo captures a build script's stderr; decisions are recorded only.
    if !build_script::SILENT.load(std::sync::atomic::Ordering::Relaxed)
        && terminal::Output::from_env().prints(kind)
    {
        eprintln!(
            "{}",
            terminal::status(terminal::stderr_color(), kind, crate_name, detail)
        );
    }
}

fn doctor(server: &str, token: Option<&str>) -> Result<()> {
    report_foreign_target_outputs(terminal::stdout_color());
    let remote = Remote::new(server, token.map(str::to_owned))?;
    let health = remote.health().context("connect to bellowsd")?;
    validate_protocol(health.protocol)?;
    let rustc = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("run rustc -vV")?;
    let compiler = String::from_utf8_lossy(&rustc.stdout);
    let color = terminal::stdout_color();
    println!("{}", terminal::heading(color, "Bellows · Doctor"));
    println!(
        "{}",
        terminal::success(
            color,
            "bellowsd",
            &format!("v{} · {server}", health.version)
        )
    );
    println!(
        "{}",
        terminal::success(color, "protocol", &health.protocol.to_string())
    );
    println!(
        "{}",
        terminal::success(
            color,
            "compiler",
            compiler.lines().next().unwrap_or("rustc")
        )
    );
    println!(
        "{}",
        terminal::success(color, "fallback", "official rustc enabled")
    );
    let l1 = if env::var("BELLOWS_L1").as_deref() == Ok("0") {
        "off (BELLOWS_L1=0)"
    } else {
        "on"
    };
    println!("{}", terminal::success(color, "local cache", l1));
    println!(
        "{}",
        terminal::success(
            color,
            "output",
            &format!("{} (BELLOWS_OUTPUT)", terminal::Output::from_env().name())
        )
    );
    Ok(())
}

/// Records restored before checkouts were identified from git roots could
/// leave build-script outputs and dep-info in a target directory that name
/// another checkout's files. Cargo keeps such outputs fresh (it watches the
/// other checkout), so report them with the remedy.
fn report_foreign_target_outputs(color: bool) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let Some(checkout) = git_root(&canonical_base(cwd.clone())) else {
        return;
    };
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(|dir| absolute_path(Path::new(&dir), &cwd))
        .unwrap_or_else(|| checkout.join("target"));
    let packages = foreign_target_outputs(&target, &checkout);
    if packages.is_empty() {
        println!(
            "{}",
            terminal::success(color, "target", "no outputs name another checkout")
        );
        return;
    }
    println!(
        "{}",
        terminal::attention(
            color,
            "target",
            &format!(
                "outputs name another checkout's files; rebuild them with `cargo clean -p {}`",
                packages.into_iter().collect::<Vec<_>>().join(" -p ")
            ),
        )
    );
}

/// Package names whose build-script output or dep-info in `target` depends
/// on files in a different git checkout. Paths in this checkout's own git
/// metadata (its worktree directory and the shared refs) are its own.
fn foreign_target_outputs(target: &Path, checkout: &Path) -> BTreeSet<String> {
    let git = GitLayout::of(checkout);
    let names_other = |dependencies: &[String]| {
        dependencies.iter().any(|dependency| {
            let dependency = Path::new(dependency);
            dependency.is_absolute()
                && !git.owns(dependency)
                && foreign_checkout(dependency, Some(checkout))
                || git.other_worktree(dependency)
        })
    };
    let mut foreign = BTreeSet::new();
    let mut files = Vec::new();
    let _ = collect_target_records(target, 0, &mut files);
    for path in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let (package, dependencies) = if name == "output" {
            let package = path
                .parent()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                .map(strip_unit_hash)
                .unwrap_or_default();
            let dependencies = text
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("cargo::rerun-if-changed=")
                        .or_else(|| line.strip_prefix("cargo:rerun-if-changed="))
                })
                .map(str::to_owned)
                .collect::<Vec<_>>();
            (package, dependencies)
        } else {
            let package = strip_unit_hash(name.trim_end_matches(".d")).replace('_', "-");
            (package, parse_dep_info(&text).0)
        };
        if names_other(&dependencies) {
            foreign.insert(package);
        }
    }
    foreign
}

/// Where a checkout's git metadata lives: a clone's `.git` directory, or a
/// linked worktree's `<common>/worktrees/<name>` plus the shared `<common>`.
struct GitLayout {
    common: Option<PathBuf>,
    own_worktree: Option<PathBuf>,
}

impl GitLayout {
    fn of(checkout: &Path) -> Self {
        let dot_git = checkout.join(".git");
        if dot_git.is_dir() {
            return Self {
                common: Some(canonical_base(dot_git)),
                own_worktree: None,
            };
        }
        let gitdir = fs::read_to_string(&dot_git).ok().and_then(|text| {
            text.trim()
                .strip_prefix("gitdir:")
                .map(|path| canonical_base(absolute_path(Path::new(path.trim()), checkout)))
        });
        Self {
            common: gitdir
                .as_ref()
                .and_then(|g| g.parent()?.parent().map(Path::to_path_buf)),
            own_worktree: gitdir,
        }
    }

    fn worktrees(&self) -> Option<PathBuf> {
        self.common.as_ref().map(|common| common.join("worktrees"))
    }

    /// Another linked worktree's metadata (its HEAD or index).
    fn other_worktree(&self, path: &Path) -> bool {
        self.worktrees()
            .is_some_and(|worktrees| path.starts_with(worktrees))
            && !self
                .own_worktree
                .as_ref()
                .is_some_and(|own| path.starts_with(own))
    }

    /// This checkout's metadata, including refs shared by all worktrees.
    fn owns(&self, path: &Path) -> bool {
        self.common
            .as_ref()
            .is_some_and(|common| path.starts_with(common))
            && !self.other_worktree(path)
    }
}

/// `build/<unit>/output` and `deps/*.d` records below a Cargo target directory.
fn collect_target_records(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        match path.file_name().and_then(OsStr::to_str) {
            Some("deps") => {
                for record in fs::read_dir(&path)?.flatten() {
                    if record.path().extension().is_some_and(|ext| ext == "d") {
                        files.push(record.path());
                    }
                }
            }
            Some("build") => {
                for unit in fs::read_dir(&path)?.flatten() {
                    let output = unit.path().join("output");
                    if output.is_file() {
                        files.push(output);
                    }
                }
            }
            Some("incremental" | "out" | "examples" | "doc") => {}
            _ => collect_target_records(&path, depth + 1, files)?,
        }
    }
    Ok(())
}

fn strip_unit_hash(name: &str) -> String {
    match name.rsplit_once('-') {
        Some((stem, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            stem.to_owned()
        }
        _ => name.to_owned(),
    }
}

fn validate_protocol(server_protocol: u32) -> Result<()> {
    if server_protocol != PROTOCOL_VERSION {
        bail!(
            "protocol mismatch: client {PROTOCOL_VERSION}, server {}",
            server_protocol
        )
    }
    Ok(())
}

#[derive(Serialize)]
struct CombinedStats {
    remote: ServerStats,
    events: BTreeMap<String, u64>,
    diagnostics: diagnostics::Summary,
    /// Local statistics only: the configured budget and the stores it covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<BudgetStats>,
}

#[derive(Serialize)]
struct BudgetStats {
    /// `unset`, `unbounded` or `limited`.
    state: &'static str,
    /// The budget each local store is held to (`BELLOWS_MAX_MB`).
    max_bytes: Option<u64>,
    /// What a build's automatic collection frees a store down to.
    collect_to_bytes: Option<u64>,
    max_executable_bytes: Option<u64>,
    /// The store `bellows run` keeps beside this one, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    wrapper_store: Option<StoreUsage>,
}

#[derive(Serialize)]
struct StoreUsage {
    path: String,
    blob_bytes: u64,
}

impl BudgetStats {
    fn new(budget: Budget, wrapper_store: Option<StoreUsage>) -> Self {
        let (state, max_bytes, max_executable_bytes) = match budget {
            Budget::Unset => ("unset", None, None),
            Budget::Unbounded => ("unbounded", None, None),
            Budget::Limited {
                max_bytes,
                max_executable_bytes,
            } => ("limited", Some(max_bytes), max_executable_bytes),
        };
        Self {
            state,
            max_bytes,
            collect_to_bytes: max_bytes.map(Budget::collect_to),
            max_executable_bytes,
            wrapper_store,
        }
    }

    fn describe(&self) -> String {
        match (self.max_bytes, self.collect_to_bytes) {
            (Some(max), Some(collect_to)) => {
                let mut text = format!(
                    "{} for each store (BELLOWS_MAX_MB); a build that ends above it collects to {}",
                    human_bytes(max),
                    human_bytes(collect_to)
                );
                if let Some(executables) = self.max_executable_bytes {
                    text.push_str(&format!(
                        ", executables to {} (BELLOWS_MAX_EXECUTABLE_MB)",
                        human_bytes(executables)
                    ));
                }
                text
            }
            _ if self.state == "unbounded" => "unbounded (BELLOWS_MAX_MB=0)".into(),
            _ => {
                "none: only `bellows gc` collects; set BELLOWS_MAX_MB (mebibytes) to cap each store"
                    .into()
            }
        }
    }
}

fn show_stats(
    server: &str,
    token: Option<&str>,
    selection: &diagnostics::Selection,
    json: bool,
) -> Result<()> {
    let remote = Remote::new(server, token.map(str::to_owned))?.stats()?;
    let diagnostics = diagnostics::summarize(&diagnostics::read_selected(selection)?);
    let events = diagnostics.decisions.clone();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CombinedStats {
                remote,
                events,
                diagnostics,
                budget: None,
            })?
        );
    } else {
        let color = terminal::stdout_color();
        println!("{}", terminal::heading(color, "Bellows · Cache statistics"));
        println!("{}", terminal::section(color, "Remote cache"));
        println!(
            "{}",
            terminal::key_value(color, "compiler actions", remote.candidates)
        );
        println!(
            "{}",
            terminal::key_value(color, "declared actions", remote.declared_actions)
        );
        println!(
            "{}",
            terminal::key_value(color, "archives", remote.archives)
        );
        println!("{}", terminal::key_value(color, "blobs", remote.blobs));
        println!(
            "{}",
            terminal::key_value(color, "stored", human_bytes(remote.blob_bytes))
        );
        println!(
            "{}",
            terminal::key_value(color, "active leases", remote.active_leases)
        );
        println!("{}", terminal::section(color, "This workspace"));
        diagnostics::print_grouped(&diagnostics, color);
        diagnostics::print_details(&diagnostics, 6);
    }
    Ok(())
}

fn show_local_stats(selection: &diagnostics::Selection, json: bool) -> Result<()> {
    let state = local_state_dir(selection.cache_dir.as_deref())?;
    let store = local_store(Some(&state), true)?;
    let remote = collect_local_stats(&store)?;
    let diagnostics = diagnostics::summarize(&diagnostics::read_selected(selection)?);
    let events = diagnostics.decisions.clone();
    // The store `bellows run` keeps in the same state directory is held to
    // the same budget, so its size belongs beside this one's.
    let wrapper_root = state.join(format!("l1-v{PROTOCOL_VERSION}"));
    let wrapper_store = if wrapper_root.join("blobs").is_dir() {
        Some(StoreUsage {
            blob_bytes: Store::open_for_access(&wrapper_root)?.blob_bytes()?,
            path: wrapper_root.display().to_string(),
        })
    } else {
        None
    };
    let budget = BudgetStats::new(Budget::from_env()?, wrapper_store);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CombinedStats {
                remote,
                events,
                diagnostics,
                budget: Some(budget),
            })?
        );
        return Ok(());
    }
    let color = terminal::stdout_color();
    println!("{}", terminal::heading(color, "Bellows · Local cache"));
    println!(
        "{}",
        terminal::key_value(color, "path", store.root().display())
    );
    println!(
        "{}",
        terminal::key_value(color, "compiler actions", remote.candidates)
    );
    println!(
        "{}",
        terminal::key_value(color, "declared actions", remote.declared_actions)
    );
    println!("{}", terminal::key_value(color, "blobs", remote.blobs));
    println!(
        "{}",
        terminal::key_value(color, "stored", human_bytes(remote.blob_bytes))
    );
    println!(
        "{}",
        terminal::key_value(color, "budget", budget.describe())
    );
    if let Some(wrapper) = &budget.wrapper_store {
        println!(
            "{}",
            terminal::key_value(
                color,
                "bellows run store",
                format!("{} in {}", human_bytes(wrapper.blob_bytes), wrapper.path)
            )
        );
    }
    println!(
        "{}",
        terminal::section(color, "Selected decisions (retained log)")
    );
    diagnostics::print_grouped(&diagnostics, color);
    diagnostics::print_details(&diagnostics, 6);
    Ok(())
}

fn collect_local_stats(store: &Store) -> Result<ServerStats> {
    let (blobs, blob_bytes) = count_store_files(&store.root().join("blobs"), false)?;
    let (action_indexes, _) = count_store_files(&store.root().join("actions"), true)?;
    let (declared_actions, _) = count_store_files(&store.root().join("declared"), true)?;
    let (archives, _) = count_store_files(&store.root().join("archives"), true)?;
    let mut candidates = 0;
    visit_store_files(&store.root().join("actions"), &mut |path| {
        let index: CandidateIndex = serde_json::from_slice(&fs::read(path)?)?;
        candidates += index.candidates.len() as u64;
        Ok(())
    })?;
    Ok(ServerStats {
        blobs,
        blob_bytes,
        action_indexes,
        candidates,
        active_leases: 0,
        declared_actions,
        archives,
    })
}

fn count_store_files(root: &Path, only_json: bool) -> Result<(u64, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    visit_store_files(root, &mut |path| {
        if !only_json
            || path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            count += 1;
            bytes += fs::metadata(path)?.len();
        }
        Ok(())
    })?;
    Ok((count, bytes))
}

fn visit_store_files(root: &Path, visitor: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            visit_store_files(&path, visitor)?;
        } else {
            visitor(&path)?;
        }
    }
    Ok(())
}

fn explain(
    selection: &diagnostics::Selection,
    limit: usize,
    json: bool,
    summary: bool,
) -> Result<()> {
    let events = diagnostics::read_selected(selection)?;
    if summary {
        let summary = diagnostics::summarize(&events);
        if json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            diagnostics::print_summary(&summary, limit);
        }
        return Ok(());
    }
    let selected = events
        .into_iter()
        .rev()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "miss" | "bypass" | "fallback" | "candidate_rejected" | "corrupt"
            )
        })
        .take(limit)
        .collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
    } else if selected.is_empty() {
        println!(
            "{}",
            terminal::success(
                terminal::stdout_color(),
                "clean",
                "no matching cache decisions in the retained log (use --local, --session, or --latest to select a build)",
            )
        );
    } else {
        let color = terminal::stdout_color();
        println!(
            "{}",
            terminal::heading(color, "Bellows · Recent cache decisions")
        );
        for event in selected {
            println!(
                "{}",
                terminal::status(
                    color,
                    &event.kind,
                    &event.crate_name,
                    &format!("{} · {}", event.timestamp_ms, event.detail),
                )
            );
        }
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn run_archive(command: ArchiveCommands) -> Result<()> {
    match command {
        ArchiveCommands::Publish {
            name,
            path,
            connection,
        } => {
            let root = path
                .canonicalize()
                .with_context(|| format!("open archive root {}", path.display()))?;
            if !root.is_dir() {
                bail!("archive root must be a directory")
            }
            let remote = Remote::new(&connection.server, connection.token)?;
            let files = collect_tree(&root, &root)?;
            if files.is_empty() {
                bail!("cannot publish an empty archive")
            }
            let mut artifacts = Vec::new();
            let mut blobs = Vec::new();
            for (artifact, bytes) in files {
                blobs.push((artifact.digest.clone(), bytes));
                artifacts.push(artifact);
            }
            remote.put_blobs(blobs)?;
            artifacts.sort_by(|left, right| left.file_name.cmp(&right.file_name));
            let manifest = ArchiveManifest {
                protocol: PROTOCOL_VERSION,
                name: name.clone(),
                tree_digest: tree_digest(&artifacts),
                created_ms: now_ms(),
                producer_action: None,
                files: artifacts,
            };
            remote.put_archive(&manifest)?;
            println!(
                "{}",
                terminal::status(
                    terminal::stdout_color(),
                    "published",
                    &name,
                    &format!(
                        "{} · {} files",
                        &manifest.tree_digest[..12],
                        manifest.files.len()
                    ),
                )
            );
        }
        ArchiveCommands::Restore {
            name,
            path,
            connection,
        } => {
            let remote = Remote::new(&connection.server, connection.token)?;
            let manifest = remote
                .archive(&name)?
                .with_context(|| format!("archive {name} does not exist"))?;
            validate_archive_manifest(&manifest)?;
            let digests = manifest
                .files
                .iter()
                .map(|artifact| artifact.digest.as_str())
                .collect::<Vec<_>>();
            let staged = manifest.files.iter().zip(remote.blobs(&digests)?);
            fs::create_dir_all(&path)?;
            let restore_root = path.canonicalize()?;
            for (artifact, bytes) in staged {
                let relative = validate_relative_path(&artifact.file_name)?;
                let destination = safe_destination(&restore_root, &relative)?;
                atomic_write(&destination, &bytes)?;
                set_file_executable(&destination, artifact.executable)?;
            }
            println!(
                "{}",
                terminal::status(
                    terminal::stdout_color(),
                    "restored",
                    &name,
                    &format!(
                        "{} · {} files",
                        &manifest.tree_digest[..12],
                        manifest.files.len()
                    ),
                )
            );
        }
    }
    Ok(())
}

fn run_declared_action(args: DeclaredRunArgs, remote_execution: bool) -> Result<()> {
    let workspace = env::current_dir()?.canonicalize()?;
    let platform = PlatformIdentity::detect()?;
    validate_declared_command(&args.command)?;
    let inputs_with_bytes = collect_declared_inputs(&workspace, &args.inputs)?;
    let inputs = inputs_with_bytes
        .iter()
        .map(|(artifact, _)| artifact.clone())
        .collect::<Vec<_>>();
    let outputs = args
        .outputs
        .iter()
        .map(|path| normalize_declared_path(&workspace, path))
        .collect::<Result<Vec<_>>>()?;
    bellows_core::validate_output_roots(&inputs, &outputs)?;
    let environment = declared_environment(&args.environment)?;
    let key = declared_action_key(
        &args.name,
        &platform,
        &args.command,
        &environment,
        &inputs,
        &outputs,
    );
    let request = ExecuteRequest {
        key: key.clone(),
        name: args.name.clone(),
        platform: platform.clone(),
        command: args.command.clone(),
        environment: environment.clone(),
        inputs: inputs.clone(),
        outputs: outputs.clone(),
    };
    if args.local {
        return run_local_declared_action(
            &workspace,
            args.cache_dir.as_deref(),
            request,
            &inputs_with_bytes,
        );
    }
    let remote = Remote::new(&args.connection.server, args.connection.token)?;
    if let Some(record) = remote.declared(&key)? {
        restore_declared_record(&remote, &workspace, &record)?;
        println!(
            "{}",
            terminal::status(terminal::stdout_color(), "hit", &args.name, &key[..12],)
        );
        return Ok(());
    }

    remote.put_blobs(
        inputs_with_bytes
            .iter()
            .map(|(artifact, bytes)| (artifact.digest.clone(), bytes.clone())),
    )?;
    let record = if remote_execution {
        let response = remote.execute(&request)?;
        println!(
            "{}",
            terminal::status(
                terminal::stdout_color(),
                if response.cache_hit {
                    "hit"
                } else {
                    "executed"
                },
                &args.name,
                &key[..12],
            )
        );
        response.record
    } else {
        let (record, blobs) = execute_local_declared(&workspace, request, &inputs_with_bytes)?;
        remote.put_blobs(blobs)?;
        remote.put_declared(&record)?;
        println!(
            "{}",
            terminal::status(terminal::stdout_color(), "miss", &args.name, &key[..12],)
        );
        record
    };
    restore_declared_record(&remote, &workspace, &record)?;
    Ok(())
}

fn run_local_declared_action(
    workspace: &Path,
    cache_dir: Option<&Path>,
    request: ExecuteRequest,
    inputs: &[(Artifact, Vec<u8>)],
) -> Result<()> {
    let store = local_store(cache_dir, true)?;
    if let Some(record) = store.read_declared(&request.key)? {
        match load_declared_record_local(&store, &record) {
            Ok(loaded) => {
                materialize_declared_record(workspace, &record, loaded)?;
                println!(
                    "{}",
                    terminal::status(
                        terminal::stdout_color(),
                        "hit",
                        &request.name,
                        &request.key[..12],
                    )
                );
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    terminal::warning(
                        terminal::stderr_color(),
                        "local action",
                        &format!("stale cached result will be rebuilt: {error:#}"),
                    )
                );
                store.remove_declared(&request.key)?;
            }
        }
    }

    let name = request.name.clone();
    let key = request.key.clone();
    let (record, blobs) = execute_local_declared(workspace, request, inputs)?;
    for (digest, bytes) in blobs {
        store.put_blob(&digest, &bytes)?;
    }
    if let Err(error) = store.put_declared(&record) {
        eprintln!(
            "{}",
            terminal::warning(
                terminal::stderr_color(),
                "local action",
                &format!("result was produced but its record raced publication: {error:#}"),
            )
        );
    }
    restore_declared_record_local(&store, workspace, &record)?;
    println!(
        "{}",
        terminal::status(terminal::stdout_color(), "miss", &name, &key[..12],)
    );
    Ok(())
}

fn declared_environment(names: &[String]) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for name in names {
        if matches!(
            name.as_str(),
            "PATH" | "HOME" | "CARGO_HOME" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN"
        ) {
            bail!("declared environment may not override {name}")
        }
        let value =
            env::var(name).with_context(|| format!("declared environment {name} is absent"))?;
        values.insert(name.clone(), value);
    }
    Ok(values)
}

fn collect_declared_inputs(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<(Artifact, Vec<u8>)>> {
    let mut files = Vec::new();
    for path in paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            workspace.join(path)
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("open declared input {}", path.display()))?;
        if !canonical.starts_with(workspace) {
            bail!("declared input escapes workspace: {}", path.display())
        }
        files.extend(collect_tree(workspace, &canonical)?);
    }
    files.sort_by(|left, right| left.0.file_name.cmp(&right.0.file_name));
    files.dedup_by(|left, right| left.0.file_name == right.0.file_name);
    Ok(files)
}

fn collect_tree(root: &Path, path: &Path) -> Result<Vec<(Artifact, Vec<u8>)>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!(
            "symlinks are not allowed in declared trees: {}",
            path.display()
        )
    }
    if metadata.is_dir() {
        let mut files = Vec::new();
        for entry in fs::read_dir(path)? {
            files.extend(collect_tree(root, &entry?.path())?);
        }
        return Ok(files);
    }
    if !metadata.is_file() {
        bail!(
            "declared tree entry is not a regular file: {}",
            path.display()
        )
    }
    let relative = path.strip_prefix(root)?;
    let relative = validate_relative_path(&relative.to_string_lossy())?;
    let bytes = fs::read(path)?;
    Ok(vec![(
        Artifact {
            file_name: relative.to_string_lossy().into_owned(),
            digest: digest_bytes(&bytes),
            executable: is_executable(path)?,
        },
        bytes,
    )])
}

fn normalize_declared_path(workspace: &Path, path: &Path) -> Result<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace)
            .with_context(|| format!("declared output escapes workspace: {}", path.display()))?
    } else {
        path
    };
    Ok(validate_relative_path(&relative.to_string_lossy())?
        .to_string_lossy()
        .into_owned())
}

fn execute_local_declared(
    destination_workspace: &Path,
    request: ExecuteRequest,
    inputs: &[(Artifact, Vec<u8>)],
) -> Result<(DeclaredActionRecord, BTreeMap<String, Vec<u8>>)> {
    let state = state_dir(destination_workspace);
    fs::create_dir_all(state.join("sandboxes"))?;
    let temp = tempfile::Builder::new()
        .prefix("action-")
        .tempdir_in(state.join("sandboxes"))?;
    let workspace = temp.path().join("workspace");
    let cargo_home = workspace.join(".bellows-cargo-home");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&cargo_home)?;
    for (artifact, bytes) in inputs {
        let destination = workspace.join(validate_relative_path(&artifact.file_name)?);
        atomic_write(&destination, bytes)?;
        set_file_executable(&destination, artifact.executable)?;
    }

    let started = Instant::now();
    let output = run_sandbox_command(&workspace, &request)?;
    let duration_ms = started.elapsed().as_millis() as u64;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        bail!("declared command failed with {}", output.status)
    }
    let mut blobs = BTreeMap::new();
    for (artifact, bytes) in inputs {
        blobs.insert(artifact.digest.clone(), bytes.clone());
    }
    let mut outputs = Vec::new();
    for declaration in &request.outputs {
        let path = workspace.join(validate_relative_path(declaration)?);
        for (artifact, bytes) in collect_tree(&workspace, &path)? {
            blobs.insert(artifact.digest.clone(), bytes);
            outputs.push(artifact);
        }
    }
    outputs.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    outputs.dedup_by(|left, right| left.file_name == right.file_name);
    if outputs.is_empty() {
        bail!("declared command produced no outputs")
    }
    let normalizer = PathNormalizer::new(vec![("$SANDBOX".into(), workspace)]);
    let stdout = normalizer.normalize_bytes(&output.stdout);
    let stderr = normalizer.normalize_bytes(&output.stderr);
    let stdout_digest = digest_bytes(&stdout);
    let stderr_digest = digest_bytes(&stderr);
    blobs.insert(stdout_digest.clone(), stdout.clone());
    blobs.insert(stderr_digest.clone(), stderr.clone());
    let record = DeclaredActionRecord {
        protocol: PROTOCOL_VERSION,
        key: request.key,
        name: request.name,
        created_ms: now_ms(),
        platform: request.platform,
        command: request.command,
        environment: request.environment,
        inputs: request.inputs,
        output_paths: request.outputs,
        outputs,
        stdout: StreamArtifact {
            digest: stdout_digest,
            len: stdout.len() as u64,
        },
        stderr: StreamArtifact {
            digest: stderr_digest,
            len: stderr.len() as u64,
        },
        duration_ms,
        executor: "local-sandbox".into(),
    };
    Ok((record, blobs))
}

fn run_sandbox_command(workspace: &Path, request: &ExecuteRequest) -> Result<std::process::Output> {
    let mut command = Command::new(&request.command[0]);
    command
        .args(&request.command[1..])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", env::var("PATH").unwrap_or_default())
        .env("HOME", "/homeless-shelter")
        .env("CARGO_HOME", ".bellows-cargo-home")
        .env("CARGO_NET_OFFLINE", "true")
        .envs(bellows_core::execution::platform_environment())
        .envs(&request.environment);
    command.env("RUSTUP_HOME", rustup_home());
    if let Some(value) = &request.platform.rustup_toolchain {
        command.env("RUSTUP_TOOLCHAIN", value);
    }
    let program = Path::new(&request.command[0])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if program == "cargo" {
        bellows_core::execution::configure_cargo_remapping(
            &mut command,
            workspace,
            &request.environment,
        )?;
    } else if program == "rustc" {
        command
            .arg("--remap-path-prefix")
            .arg(format!("{}=/bellows/action", workspace.display()));
    }
    command.output().context("run declared sandbox command")
}

fn restore_declared_record(
    remote: &Remote,
    workspace: &Path,
    record: &DeclaredActionRecord,
) -> Result<()> {
    validate_declared_record(record)?;
    let digests = record
        .outputs
        .iter()
        .map(|artifact| artifact.digest.as_str())
        .chain([record.stdout.digest.as_str(), record.stderr.digest.as_str()])
        .collect::<Vec<_>>();
    let mut blobs = remote.blobs(&digests)?;
    let stderr = blobs.pop().context("stderr blob missing")?;
    let stdout = blobs.pop().context("stdout blob missing")?;
    let staged = record.outputs.iter().zip(blobs).collect::<Vec<_>>();
    if stdout.len() as u64 != record.stdout.len || stderr.len() as u64 != record.stderr.len {
        bail!("declared stream length does not match manifest")
    }
    restore::declared_outputs(workspace, record, &staged)?;
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

struct LoadedDeclared<'a> {
    artifacts: Vec<(&'a Artifact, Vec<u8>)>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn load_declared_record_local<'a>(
    store: &Store,
    record: &'a DeclaredActionRecord,
) -> Result<LoadedDeclared<'a>> {
    validate_declared_record(record)?;
    let mut artifacts = Vec::with_capacity(record.outputs.len());
    for artifact in &record.outputs {
        artifacts.push((artifact, store.read_blob(&artifact.digest)?));
    }
    let stdout = store.read_blob(&record.stdout.digest)?;
    let stderr = store.read_blob(&record.stderr.digest)?;
    if stdout.len() as u64 != record.stdout.len || stderr.len() as u64 != record.stderr.len {
        bail!("declared stream length does not match manifest")
    }
    Ok(LoadedDeclared {
        artifacts,
        stdout,
        stderr,
    })
}

fn materialize_declared_record(
    workspace: &Path,
    record: &DeclaredActionRecord,
    loaded: LoadedDeclared<'_>,
) -> Result<()> {
    // Destination failures aren't corrupt cache entries: keep the record and
    // report the real filesystem error instead of deleting it and recompiling.
    restore::declared_outputs(workspace, record, &loaded.artifacts)?;
    std::io::stdout().write_all(&loaded.stdout)?;
    std::io::stderr().write_all(&loaded.stderr)?;
    Ok(())
}

fn restore_declared_record_local(
    store: &Store,
    workspace: &Path,
    record: &DeclaredActionRecord,
) -> Result<()> {
    materialize_declared_record(
        workspace,
        record,
        load_declared_record_local(store, record)?,
    )
}

fn safe_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(segment) = component else {
            bail!("unsafe restore path: {}", relative.display())
        };
        current.push(segment);
        if index + 1 == components.len() {
            if fs::symlink_metadata(&current)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                bail!("restore destination is a symlink: {}", current.display())
            }
        } else if current.exists() {
            let metadata = fs::symlink_metadata(&current)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("unsafe restore parent: {}", current.display())
            }
        } else {
            fs::create_dir(&current)?;
        }
    }
    Ok(current)
}

fn set_file_executable(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

fn run_analysis(command: AnalyzeCommands) -> Result<()> {
    match command {
        AnalyzeCommands::Snapshot { name } => create_workspace_snapshot(&name),
        AnalyzeCommands::Compare {
            before,
            after,
            json,
        } => compare_workspace_snapshots(&before, &after, json),
    }
}

const SURFACE_CAVEAT: &str = "Advisory syntactic public surface only; generic and #[inline] bodies, default trait methods, exported macros, generated code, and compiler metadata may affect downstream crates without changing this digest. Never use this result to authorize a cache hit.";

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    resolve: Option<CargoResolve>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    manifest_path: String,
}

#[derive(Debug, Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Debug, Deserialize)]
struct CargoNode {
    id: String,
    dependencies: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkspaceSnapshot {
    protocol: u32,
    name: String,
    created_ms: u64,
    workspace: String,
    caveat: String,
    packages: BTreeMap<String, PackageSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PackageSnapshot {
    id: String,
    name: String,
    root: String,
    source_digest: String,
    syntactic_surface_digest: String,
    dependencies: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImpactReport {
    before: String,
    after: String,
    caveat: String,
    source_changed: Vec<String>,
    syntactic_surface_changed: Vec<String>,
    private_implementation_candidates: Vec<String>,
    affected_downstream: Vec<String>,
}

fn snapshot_path(name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid snapshot name")
    }
    let workspace = env::current_dir()?.canonicalize()?;
    Ok(state_dir(&workspace)
        .join("snapshots")
        .join(format!("{name}.json")))
}

fn create_workspace_snapshot(name: &str) -> Result<()> {
    let workspace = env::current_dir()?.canonicalize()?;
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .context("run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)?;
    let members = metadata
        .workspace_members
        .into_iter()
        .collect::<BTreeSet<_>>();
    let dependency_map = metadata
        .resolve
        .map(|resolve| {
            resolve
                .nodes
                .into_iter()
                .map(|node| (node.id, node.dependencies))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut packages = BTreeMap::new();
    for package in metadata
        .packages
        .into_iter()
        .filter(|package| members.contains(&package.id))
    {
        let manifest = PathBuf::from(&package.manifest_path);
        let root = manifest
            .parent()
            .context("manifest has no parent")?
            .canonicalize()?;
        let rust_files = collect_rust_sources(&root)?;
        let mut source_hasher = blake3::Hasher::new();
        let mut public_lines = Vec::new();
        for path in &rust_files {
            let relative = path.strip_prefix(&root)?;
            let bytes = fs::read(path)?;
            hash_field(&mut source_hasher, &relative.to_string_lossy(), &bytes);
            let source = String::from_utf8_lossy(&bytes);
            for line in source.lines().map(str::trim) {
                if is_syntactic_public_line(line) {
                    public_lines.push(format!("{}:{line}", relative.display()));
                }
            }
        }
        for control in [&manifest, &root.join("build.rs")] {
            if control.is_file() {
                hash_field(
                    &mut source_hasher,
                    &control
                        .strip_prefix(&root)
                        .unwrap_or(control)
                        .to_string_lossy(),
                    &fs::read(control)?,
                );
            }
        }
        public_lines.sort();
        let dependencies = dependency_map
            .get(&package.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|dependency| members.contains(dependency))
            .collect();
        packages.insert(
            package.id.clone(),
            PackageSnapshot {
                id: package.id,
                name: package.name,
                root: root
                    .strip_prefix(&workspace)
                    .unwrap_or(&root)
                    .to_string_lossy()
                    .into_owned(),
                source_digest: source_hasher.finalize().to_hex().to_string(),
                syntactic_surface_digest: digest_bytes(public_lines.join("\n").as_bytes()),
                dependencies,
            },
        );
    }
    let snapshot = WorkspaceSnapshot {
        protocol: PROTOCOL_VERSION,
        name: name.into(),
        created_ms: now_ms(),
        workspace: workspace.to_string_lossy().into_owned(),
        caveat: SURFACE_CAVEAT.into(),
        packages,
    };
    let path = snapshot_path(name)?;
    atomic_write(&path, &serde_json::to_vec_pretty(&snapshot)?)?;
    println!(
        "{}",
        terminal::status(
            terminal::stdout_color(),
            "captured",
            name,
            &format!("{} packages", snapshot.packages.len()),
        )
    );
    Ok(())
}

fn collect_rust_sources(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "target" || name == ".git")
            {
                return Ok(());
            }
            for entry in fs::read_dir(path)? {
                visit(&entry?.path(), files)?;
            }
        } else if metadata.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        {
            files.push(path.to_path_buf());
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn is_syntactic_public_line(line: &str) -> bool {
    line.starts_with("pub ")
        || line.starts_with("pub async ")
        || line.starts_with("pub const ")
        || line.starts_with("pub unsafe ")
        || line == "#[macro_export]"
}

fn compare_workspace_snapshots(before: &str, after: &str, json: bool) -> Result<()> {
    let before_snapshot: WorkspaceSnapshot =
        serde_json::from_slice(&fs::read(snapshot_path(before)?)?)?;
    let after_snapshot: WorkspaceSnapshot =
        serde_json::from_slice(&fs::read(snapshot_path(after)?)?)?;
    let mut source_changed = BTreeSet::new();
    let mut surface_changed = BTreeSet::new();
    for (id, package) in &after_snapshot.packages {
        match before_snapshot.packages.get(id) {
            Some(previous) => {
                if previous.source_digest != package.source_digest {
                    source_changed.insert(id.clone());
                }
                if previous.syntactic_surface_digest != package.syntactic_surface_digest {
                    surface_changed.insert(id.clone());
                }
            }
            None => {
                source_changed.insert(id.clone());
                surface_changed.insert(id.clone());
            }
        }
    }
    let mut reverse = BTreeMap::<String, Vec<String>>::new();
    for (id, package) in &after_snapshot.packages {
        for dependency in &package.dependencies {
            reverse
                .entry(dependency.clone())
                .or_default()
                .push(id.clone());
        }
    }
    let mut affected = surface_changed.clone();
    let mut frontier = surface_changed.iter().cloned().collect::<Vec<_>>();
    while let Some(changed) = frontier.pop() {
        for downstream in reverse.get(&changed).into_iter().flatten() {
            if affected.insert(downstream.clone()) {
                frontier.push(downstream.clone());
            }
        }
    }
    let package_names = |ids: &BTreeSet<String>| {
        ids.iter()
            .filter_map(|id| {
                after_snapshot
                    .packages
                    .get(id)
                    .map(|package| package.name.clone())
            })
            .collect::<Vec<_>>()
    };
    let private = source_changed
        .difference(&surface_changed)
        .cloned()
        .collect::<BTreeSet<_>>();
    let report = ImpactReport {
        before: before.into(),
        after: after.into(),
        caveat: SURFACE_CAVEAT.into(),
        source_changed: package_names(&source_changed),
        syntactic_surface_changed: package_names(&surface_changed),
        private_implementation_candidates: package_names(&private),
        affected_downstream: package_names(&affected),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let color = terminal::stdout_color();
        println!(
            "{}",
            terminal::heading(color, &format!("Bellows · Impact · {before} → {after}"))
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "source changed",
                display_names(&report.source_changed)
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "surface changed",
                display_names(&report.syntactic_surface_changed),
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "relink candidates",
                display_names(&report.private_implementation_candidates),
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "affected downstream",
                display_names(&report.affected_downstream),
            )
        );
        println!("{}", terminal::section(color, "Caveat"));
        println!("  {}", report.caveat);
    }
    Ok(())
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
#[path = "../tests/src/main_test.rs"]
mod tests;
#[test]
fn compiler_json_streams_escape_localized_windows_paths() {
    let normalizer = PathNormalizer::new(vec![(
        "$TARGET".into(),
        PathBuf::from(r"C:\cache with spaces"),
    )]);
    let message = serde_json::json!({
        "$message_type": "artifact",
        "artifact": r"C:\cache with spaces\libfixture.rlib",
        "nested": [{"rendered": "a quoted \"message\"\nsecond line"}]
    });
    let mut original = serde_json::to_vec(&message).unwrap();
    original.extend_from_slice(b"\r\n");
    let stored = transform_compiler_stream(&original, &normalizer, false);
    assert!(String::from_utf8_lossy(&stored).contains("$TARGET"));
    let restored = transform_compiler_stream(&stored, &normalizer, true);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&restored).unwrap(),
        message
    );
    assert!(restored.ends_with(b"\r\n"));
}
