use anyhow::{Context, Result, bail};
use bellows_lease::protocol::{AdminAction, ClientEvent, Hello, Reply, Request, StatusReport};
use bellows_lease::{default_socket, default_state_dir, now_ms, parse_duration_secs};
use clap::{Args, Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// Exit code when the lease is refused or lost before the command ran.
const EX_TEMPFAIL: i32 = 75;

#[derive(Parser)]
#[command(name = "lease", version, about = "Run commands under a machine lease")]
struct Cli {
    #[arg(long, env = "LEASE_SOCKET", global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Wait for the machine, run the command, release on exit.
    Run(RunArgs),
    /// Like `run`, but the command may run nested `lease run`s on the same
    /// machine without queueing (one batch, never interleaved).
    Hold(RunArgs),
    /// Holders and queues.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Record a command about to run on MACHINE without its lease (for
    /// wrappers that skip the lease), and warn about it.
    Unleased {
        machine: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        reason: String,
    },
    /// Note that a CI job started sharing a machine (from a runner hook).
    CiStart {
        machine: String,
        #[arg(long)]
        job: String,
    },
    CiStop {
        machine: String,
        #[arg(long)]
        job: String,
    },
    /// Administrative overrides (the coordinator's).
    Admin {
        #[arg(long, env = "LEASE_STATE_DIR")]
        state_dir: Option<PathBuf>,
        #[command(subcommand)]
        action: AdminCommand,
    },
    /// The grant/release audit log.
    Log {
        #[arg(long, env = "LEASE_STATE_DIR")]
        state_dir: Option<PathBuf>,
        /// Only entries newer than this (e.g. 2h).
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Args)]
struct RunArgs {
    machine: String,
    /// Expected duration (e.g. 20m); overruns past 1.5× are flagged.
    #[arg(long)]
    est: String,
    #[arg(long, default_value_t = 0)]
    prio: i32,
    /// The pull request this serves; its merge-queue position ranks it.
    #[arg(long)]
    pr: Option<u32>,
    #[arg(long)]
    label: String,
    /// Never wait: if the machine is busy or the daemon is down, run the
    /// command anyway, unleased, with a warning and an `unleased` log entry.
    /// Only this flag turns it on: no inherited environment variable does.
    #[arg(long)]
    advisory: bool,
    /// Run the command as given. By default it runs under `bellows run
    /// --machine-state`, so every Cargo build in the job uses the compile
    /// cache and the machine's one Bellows state directory. Only this flag
    /// turns that off: no inherited environment variable does.
    #[arg(long)]
    no_bellows: bool,
    #[arg(required = true, last = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum AdminCommand {
    Reorder {
        id: u64,
        position: usize,
    },
    Pause {
        machine: String,
    },
    Resume {
        machine: String,
    },
    /// End a session at its next command boundary.
    Preempt {
        id: u64,
    },
    /// Grant only requests whose label contains LABEL, for a while.
    Reserve {
        machine: String,
        #[arg(long)]
        label: String,
        #[arg(long = "for", default_value = "30m")]
        duration: String,
    },
    Unreserve {
        machine: String,
    },
    /// Drop a queued request, or revoke a lease (its job is killed).
    Cancel {
        id: u64,
    },
}

pub fn main() -> i32 {
    let cli = Cli::parse();
    let socket = cli.socket.clone().unwrap_or_else(default_socket);
    match run(cli.command, &socket) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("lease: {error:#}");
            EX_TEMPFAIL
        }
    }
}

fn run(command: Commands, socket: &Path) -> Result<i32> {
    match command {
        Commands::Run(args) => lease_and_run(socket, args, false),
        Commands::Hold(args) => lease_and_run(socket, args, true),
        Commands::Status { json } => {
            let report = match exchange(socket, &Hello::Status)? {
                Reply::Status(report) => report,
                other => bail!("unexpected reply {other:?}"),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_status(&report);
            }
            Ok(0)
        }
        Commands::Unleased {
            machine,
            label,
            reason,
        } => {
            warn_unleased(socket, &machine, &label, None, &reason);
            Ok(0)
        }
        Commands::CiStart { machine, job } => simple(socket, &Hello::CiStart { machine, job }),
        Commands::CiStop { machine, job } => simple(socket, &Hello::CiStop { machine, job }),
        Commands::Admin { state_dir, action } => {
            let state_dir = state_dir.unwrap_or_else(default_state_dir);
            let secret = std::fs::read_to_string(state_dir.join("admin.token"))
                .context("read the admin token (only the daemon's owner can)")?
                .trim()
                .to_owned();
            let action = match action {
                AdminCommand::Reorder { id, position } => AdminAction::Reorder { id, position },
                AdminCommand::Pause { machine } => AdminAction::Pause { machine },
                AdminCommand::Resume { machine } => AdminAction::Resume { machine },
                AdminCommand::Preempt { id } => AdminAction::Preempt { id },
                AdminCommand::Reserve {
                    machine,
                    label,
                    duration,
                } => AdminAction::Reserve {
                    machine,
                    label,
                    until_ms: now_ms() + parse_duration_secs(&duration)? * 1000,
                },
                AdminCommand::Unreserve { machine } => AdminAction::Unreserve { machine },
                AdminCommand::Cancel { id } => AdminAction::Cancel { id },
            };
            simple(socket, &Hello::Admin { secret, action })
        }
        Commands::Log { state_dir, since } => {
            let state_dir = state_dir.unwrap_or_else(default_state_dir);
            let cutoff = match since {
                Some(since) => now_ms().saturating_sub(parse_duration_secs(&since)? * 1000),
                None => 0,
            };
            let text = std::fs::read_to_string(state_dir.join("log.jsonl")).unwrap_or_default();
            for line in text.lines() {
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let at = entry.get("at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                if at < cutoff {
                    continue;
                }
                let field = |name: &str| {
                    entry
                        .get(name)
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string())
                        })
                        .filter(|value| value != "null")
                        .unwrap_or_default()
                };
                println!(
                    "{:>8}s ago  {:<14} {:>5}  {:<8} {:<40} {}",
                    now_ms().saturating_sub(at) / 1000,
                    field("event"),
                    field("id"),
                    field("machine"),
                    field("label"),
                    field("detail")
                );
            }
            Ok(0)
        }
    }
}

fn connect(socket: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket)
        .with_context(|| format!("connect to leased at {} (is it running?)", socket.display()))
}

fn write_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    Ok(())
}

fn exchange(socket: &Path, hello: &Hello) -> Result<Reply> {
    let mut stream = connect(socket)?;
    write_line(&mut stream, hello)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).context("decode the daemon's reply")
}

fn simple(socket: &Path, hello: &Hello) -> Result<i32> {
    match exchange(socket, hello)? {
        Reply::Ok => Ok(0),
        Reply::Refused { reason } => {
            eprintln!("lease: refused: {reason}");
            Ok(1)
        }
        other => bail!("unexpected reply {other:?}"),
    }
}

fn print_status(report: &StatusReport) {
    for machine in &report.machines {
        let mut flags = Vec::new();
        if machine.paused {
            flags.push("PAUSED".to_owned());
        }
        if let Some(label) = &machine.reserved_for {
            flags.push(format!("reserved for {label:?}"));
        }
        if !machine.ci_jobs.is_empty() {
            flags.push(format!("CI: {}", machine.ci_jobs.join(", ")));
        }
        println!(
            "{} (jobs {}) {}",
            machine.machine,
            machine.jobs,
            flags.join(" · ")
        );
        match &machine.holder {
            Some(holder) => println!(
                "  held  #{:<4} {:<40} {}m of ~{}m{}{}",
                holder.id,
                holder.label,
                holder.held_secs / 60,
                holder.estimate_secs / 60,
                if holder.session { " · session" } else { "" },
                if holder.preempting {
                    " · PREEMPTING"
                } else {
                    ""
                },
            ),
            None => println!("  free"),
        }
        for (position, queued) in machine.queue.iter().enumerate() {
            println!(
                "  {:>2}.   #{:<4} {:<40} prio {} {}waited {}m, ~{}m",
                position + 1,
                queued.id,
                queued.label,
                queued.priority,
                queued
                    .merge_queue_position
                    .map(|position| format!("merge queue #{} · ", position + 1))
                    .or_else(|| queued.pr.map(|pr| format!("PR #{pr} · ")))
                    .unwrap_or_default(),
                queued.waited_secs / 60,
                queued.estimate_secs / 60,
            );
        }
    }
    let unleased = unleased_rustc();
    if !unleased.is_empty() {
        println!(
            "this host: {} rustc not started under `lease` or CI:",
            unleased.values().sum::<usize>()
        );
        for (cwd, count) in unleased {
            println!("  {count:>3} in {}", cwd.display());
        }
    }
}

/// Compilers running on this host outside any lease or CI job, by working
/// directory. A leased job's environment carries `LEASE_ID`; a CI job runs
/// under the runner's `Runner.Worker`.
fn unleased_rustc() -> std::collections::BTreeMap<PathBuf, usize> {
    let mut found = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if proc_comm(pid).as_deref() != Some("rustc") {
            continue;
        }
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        if environ
            .split(|&b| b == 0)
            .any(|var| var.starts_with(b"LEASE_ID="))
        {
            continue;
        }
        let mut ancestor = proc_ppid(pid);
        let mut ci = false;
        while let Some(parent) = ancestor.filter(|&p| p > 1) {
            if proc_comm(parent).as_deref() == Some("Runner.Worker") {
                ci = true;
                break;
            }
            ancestor = proc_ppid(parent);
        }
        if !ci {
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_default();
            *found.entry(cwd).or_default() += 1;
        }
    }
    found
}

fn proc_comm(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_owned())
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`; comm may contain spaces and parentheses.
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::SeqCst);
}

/// The command a job runs: under `bellows run`, unless the caller opted out
/// or the job is already inside a Bellows build (a nested `lease run` in a
/// `lease hold` session that was wrapped), where a second wrap would only
/// start a second build session.
fn job_command(command: &[String], no_bellows: bool) -> Result<Vec<String>> {
    let wrapped_already = std::env::var_os("BELLOWS_SESSION_ID").is_some_and(|id| !id.is_empty())
        && std::env::var_os("RUSTC_WRAPPER").is_some_and(|wrapper| {
            Path::new(&wrapper).file_stem() == Some(std::ffi::OsStr::new("bellows"))
        });
    if no_bellows || wrapped_already {
        return Ok(command.to_vec());
    }
    let bellows = find_program("bellows").context(
        "`bellows` is not on PATH: install it, or pass --no-bellows to run the job without the compile cache",
    )?;
    let mut wrapped = vec![
        bellows.to_string_lossy().into_owned(),
        "run".into(),
        "--machine-state".into(),
        "--".into(),
    ];
    wrapped.extend(command.iter().cloned());
    Ok(wrapped)
}

fn find_program(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn lease_and_run(socket: &Path, args: RunArgs, session: bool) -> Result<i32> {
    // Decided before queueing: a job that cannot start should not wait first.
    let command = job_command(&args.command, args.no_bellows)?;
    // Inside a session on the same machine, the session's token runs us at once.
    let token = (std::env::var("LEASE_MACHINE").ok().as_deref() == Some(args.machine.as_str()))
        .then(|| std::env::var("LEASE_TOKEN").ok())
        .flatten();
    let mut stream = match connect(socket) {
        Ok(stream) => stream,
        Err(error) if args.advisory => {
            let reason = format!("advisory: the lease daemon is unreachable ({error:#})");
            warn_unleased(socket, &args.machine, &args.label, None, &reason);
            return run_unleased(&command);
        }
        Err(error) => return Err(error),
    };
    write_line(
        &mut stream,
        &Hello::Request(Request {
            machine: args.machine.clone(),
            label: args.label.clone(),
            priority: args.prio,
            pr: args.pr,
            estimate_secs: parse_duration_secs(&args.est)?,
            session,
            token,
            client_pid: std::process::id(),
        }),
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    // SAFETY: async-signal-safe handler that only stores an integer.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
    // While queued, wake regularly so a signal (^C, `timeout`) ends the wait:
    // a blocking read is restarted after the handler runs, so would wait on.
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut line = Vec::new();
    let (id, jobs, session_token) = loop {
        if SIGNAL.load(Ordering::SeqCst) != 0 {
            eprintln!(
                "lease: interrupted while waiting for {}; request withdrawn",
                args.machine
            );
            return Ok(EX_TEMPFAIL);
        }
        // `read_until` keeps a partly read line across timeouts.
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                eprintln!("lease: the daemon closed the connection before granting");
                return Ok(EX_TEMPFAIL);
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        if !line.ends_with(b"\n") {
            continue;
        }
        let reply = serde_json::from_slice::<Reply>(&line)?;
        line.clear();
        match reply {
            Reply::Queued {
                id,
                position,
                holder,
            } if args.advisory => {
                // Closing the connection withdraws the request.
                drop(reader);
                drop(stream);
                let reason = format!(
                    "advisory: {} is busy (position {}{})",
                    args.machine,
                    position + 1,
                    holder
                        .map(|label| format!(", held by {label:?}"))
                        .unwrap_or_default()
                );
                warn_unleased(socket, &args.machine, &args.label, Some(id), &reason);
                return run_unleased(&command);
            }
            Reply::Queued {
                id,
                position,
                holder,
            } => eprintln!(
                "lease: #{id} waiting for {} (position {}{})",
                args.machine,
                position + 1,
                holder
                    .map(|label| format!(", held by {label:?}"))
                    .unwrap_or_default()
            ),
            Reply::Granted {
                id,
                jobs,
                token,
                nested,
            } => {
                if !nested {
                    eprintln!("lease: #{id} granted {} ({jobs} jobs)", args.machine);
                }
                break (id, jobs, token);
            }
            Reply::Refused { reason } => {
                eprintln!("lease: refused: {reason}");
                return Ok(EX_TEMPFAIL);
            }
            _ => {}
        }
    };
    stream.set_read_timeout(None)?;

    let (program, rest) = command.split_first().context("missing command")?;
    let mut command = Command::new(program);
    command
        .args(rest)
        .env("LEASE_ID", id.to_string())
        .env("LEASE_MACHINE", &args.machine)
        .env("LEASE_JOBS", jobs.to_string())
        .env("CARGO_BUILD_JOBS", jobs.to_string())
        .process_group(0);
    if let Some(token) = &session_token {
        command.env("LEASE_TOKEN", token);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe; the child dies with this client.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            write_line(&mut stream, &ClientEvent::Done { code: 127 })?;
            bail!("start {program}: {error}")
        }
    };
    let pgid = child.id() as i32;
    write_line(&mut stream, &ClientEvent::Spawned { pgid })?;
    let terminal = ForegroundTerminal::hand_to(pgid);

    let revoked = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    {
        let (revoked, lost) = (Arc::clone(&revoked), Arc::clone(&lost));
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        lost.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(_) => match serde_json::from_str::<Reply>(&line) {
                        Ok(Reply::Revoked { reason }) => {
                            eprintln!("lease: #{id} revoked: {reason}");
                            revoked.store(true, Ordering::SeqCst);
                        }
                        Ok(Reply::Preempting) => eprintln!(
                            "lease: #{id} preempted: the session ends at its next command"
                        ),
                        _ => {}
                    },
                }
            }
        });
    }

    let mut stopping: Option<Instant> = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let signal = SIGNAL.load(Ordering::SeqCst);
        let reason = if revoked.load(Ordering::SeqCst) {
            Some("the lease was revoked")
        } else if lost.load(Ordering::SeqCst) {
            Some("the lease daemon went away, so the lease is no longer held")
        } else if signal != 0 {
            Some("interrupted")
        } else {
            None
        };
        match (reason, stopping) {
            (Some(reason), None) => {
                eprintln!("lease: #{id}: {reason}; stopping the job");
                kill_group(pgid, libc::SIGTERM);
                stopping = Some(Instant::now());
            }
            (_, Some(since)) if since.elapsed() > Duration::from_secs(5) => {
                kill_group(pgid, libc::SIGKILL);
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(terminal);
    // Nothing the job left behind outlives the lease.
    kill_group(pgid, libc::SIGKILL);
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    });
    let _ = write_line(&mut stream, &ClientEvent::Done { code });
    Ok(code)
}

/// The job runs in its own process group, so from an interactive shell it
/// would be a background job: stopped as soon as it read the terminal. While
/// it runs, its group owns the terminal instead (it gets ^C directly).
struct ForegroundTerminal;

impl ForegroundTerminal {
    fn hand_to(pgid: i32) -> Option<Self> {
        // SAFETY: plain terminal syscalls on stdin; SIGTTOU is ignored so this
        // (soon background) process may take the terminal back later.
        unsafe {
            if libc::isatty(0) != 1 || libc::tcgetpgrp(0) != libc::getpgrp() {
                return None;
            }
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            if libc::tcsetpgrp(0, pgid) != 0 {
                return None;
            }
            // In case the job already read the terminal and was stopped.
            libc::kill(-pgid, libc::SIGCONT);
        }
        Some(Self)
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
        }
    }
}

/// Says loudly that a command is about to share MACHINE with whoever holds
/// it, and records that as `unleased` in the lease log: through the daemon,
/// or, when it is down, by appending to its log directly.
fn warn_unleased(socket: &Path, machine: &str, label: &str, id: Option<u64>, reason: &str) {
    let recorded = exchange(
        socket,
        &Hello::Unleased {
            machine: machine.to_owned(),
            label: label.to_owned(),
            id,
            reason: reason.to_owned(),
        },
    )
    .and_then(|reply| match reply {
        Reply::Ok => Ok(()),
        other => bail!("unexpected reply {other:?}"),
    })
    .or_else(|_| append_unleased(machine, label, id, reason));
    let rule = "=".repeat(72);
    eprintln!("lease: {rule}");
    eprintln!("lease: WARNING: running UNLEASED on {machine}: {label}");
    eprintln!("lease:   {reason}");
    eprintln!("lease:   It competes with the lease holder for the machine; timeouts and");
    eprintln!("lease:   slowness in this run are not evidence of a flake.");
    match recorded {
        Ok(()) => eprintln!("lease:   Recorded as `unleased` in `lease log`."),
        Err(error) => eprintln!("lease:   NOT recorded in `lease log`: {error:#}"),
    }
    eprintln!("lease: {rule}");
}

fn append_unleased(machine: &str, label: &str, id: Option<u64>, reason: &str) -> Result<()> {
    let state_dir = std::env::var_os("LEASE_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_dir);
    let mut line = serde_json::to_vec(&serde_json::json!({
        "at_ms": now_ms(),
        "event": "unleased",
        "id": id,
        "machine": machine,
        "label": label,
        "detail": reason,
    }))?;
    line.push(b'\n');
    let path = state_dir.join("log.jsonl");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;
    // One write of one line: appends from several writers do not interleave.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(&line))
        .with_context(|| format!("append to {}", path.display()))
}

/// Run the command as if no lease existed (advisory mode only).
fn run_unleased(command: &[String]) -> Result<i32> {
    // SAFETY: restoring default dispositions; signals now reach us as usual.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    let (program, rest) = command.split_first().context("missing command")?;
    let status = Command::new(program)
        .args(rest)
        .status()
        .with_context(|| format!("start {program}"))?;
    Ok(status.code().unwrap_or(128))
}

fn kill_group(pgid: i32, signal: libc::c_int) {
    if pgid > 1 {
        // SAFETY: plain syscall; a group that is already gone is harmless.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}
