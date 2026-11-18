//! End to end: a real daemon, real clients, real process groups.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    socket: PathBuf,
    state: PathBuf,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lease.sock");
        let state = dir.path().join("state");
        let child = Command::new(env!("CARGO_BIN_EXE_leased"))
            .arg("--socket")
            .arg(&socket)
            .arg("--state-dir")
            .arg(&state)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "leased did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Jobs run under `bellows run` by default. This stand-in records how
        // it was called and runs the job itself in its place (`exec`), so
        // process groups and exit codes are the job's.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bellows = bin.join("bellows");
        std::fs::write(
            &bellows,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_BELLOWS_LOG\"\n\
             [ \"$1 $2 $3\" = 'run --machine-state --' ] || exit 99\nshift 3\nexec \"$@\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bellows, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            child,
            socket,
            state,
            dir,
        }
    }

    fn lease(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lease"));
        // A test run under `lrun` or `lease hold` inherits that lease
        // (LEASE_ID, LEASE_MACHINE, LEASE_ADVISORY, ...); a client that sees
        // one runs at once as part of it, so these clients would never queue.
        // Nor may it see the Bellows build the tests run in, if any: a job
        // inside one is not wrapped again.
        for (name, _) in std::env::vars_os() {
            let name_text = name.to_string_lossy();
            if name_text.starts_with("LEASE_")
                || name_text.starts_with("BELLOWS_")
                || name_text == "RUSTC_WRAPPER"
            {
                command.env_remove(name);
            }
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.dir.path().join("bin")).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        command
            .env("LEASE_SOCKET", &self.socket)
            .env("LEASE_STATE_DIR", &self.state)
            .env("PATH", path)
            .env("FAKE_BELLOWS_LOG", self.bellows_log());
        command
    }

    /// Each `bellows` invocation, one line of arguments each.
    fn bellows_log(&self) -> PathBuf {
        self.dir.path().join("bellows.log")
    }

    fn bellows_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.bellows_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("log.jsonl")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_shell(daemon: &Daemon, label: &str, script: &str) -> Child {
    daemon
        .lease()
        .args([
            "run", "laptop", "--est", "1m", "--label", label, "--", "sh", "-c", script,
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn leases_on_one_machine_never_overlap() {
    let daemon = Daemon::start();
    let trace = daemon.dir.path().join("trace");
    let script = |name: &str| {
        format!(
            "echo start-{name} >> {t}; sleep 0.4; echo end-{name} >> {t}",
            t = trace.display()
        )
    };
    let mut clients = ["a", "b", "c"]
        .map(|name| run_shell(&daemon, name, &script(name)))
        .into_iter()
        .collect::<Vec<_>>();
    for client in &mut clients {
        assert!(client.wait().unwrap().success());
    }
    let lines = std::fs::read_to_string(&trace).unwrap();
    let lines = lines.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 6, "{lines:?}");
    for pair in lines.chunks(2) {
        let name = pair[0].strip_prefix("start-").expect("a start");
        assert_eq!(pair[1], format!("end-{name}"), "jobs overlapped: {lines:?}");
    }
    let log = daemon.log();
    assert_eq!(log.matches("\"event\":\"grant\"").count(), 3, "{log}");
    assert_eq!(log.matches("\"event\":\"release\"").count(), 3, "{log}");
}

#[test]
fn a_killed_client_takes_its_job_with_it_and_frees_the_machine() {
    let daemon = Daemon::start();
    let pid_file = daemon.dir.path().join("job.pid");
    let mut client = run_shell(
        &daemon,
        "doomed",
        &format!("sleep 60 & echo $! > {}; wait", pid_file.display()),
    );
    wait_until("the job to start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let job: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(job));
    // SIGKILL: the client gets no chance to clean up.
    client.kill().unwrap();
    client.wait().unwrap();
    wait_until("the orphaned job to die", || !alive(job));
    let mut next = run_shell(&daemon, "next", "true");
    assert!(next.wait().unwrap().success(), "the machine was freed");
    assert!(daemon.log().contains("killed_orphan"));
}

#[test]
fn a_session_runs_its_nested_leases_without_queueing() {
    let daemon = Daemon::start();
    let lease = env!("CARGO_BIN_EXE_lease");
    let status = daemon
        .lease()
        .args(["hold", "laptop", "--est", "1m", "--label", "batch", "--", "sh", "-c"])
        .arg(format!(
            "{lease} run laptop --est 1m --label step1 -- true && {lease} run laptop --est 1m --label step2 -- true"
        ))
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let log = daemon.log();
    assert_eq!(log.matches("grant_nested").count(), 2, "{log}");
}

#[test]
fn status_admin_and_ci_sharing() {
    let daemon = Daemon::start();
    let ok = |args: &[&str]| daemon.lease().args(args).status().unwrap().success();
    assert!(ok(&["ci-start", "laptop", "--job", "run-1/gpu"]));
    let status = daemon.lease().args(["status", "--json"]).output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["machines"][0]["jobs"], 12);
    assert!(ok(&["ci-stop", "laptop", "--job", "run-1/gpu"]));
    assert!(ok(&["admin", "pause", "desktop"]));
    let report: serde_json::Value = serde_json::from_slice(
        &daemon
            .lease()
            .args(["status", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(report["machines"][1]["paused"], true);
    assert_eq!(report["machines"][0]["jobs"], 24);
    // The job sees the grant's job count.
    let out = daemon
        .lease()
        .args([
            "run",
            "laptop",
            "--est",
            "1m",
            "--label",
            "jobs",
            "--",
            "sh",
            "-c",
            "echo $CARGO_BUILD_JOBS",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "24");
    // A wrong secret is refused.
    let refused = daemon
        .lease()
        .env("LEASE_STATE_DIR", Path::new("/nonexistent"))
        .args(["admin", "resume", "desktop"])
        .status()
        .unwrap();
    assert!(!refused.success());
}

#[test]
fn an_exit_code_passes_through() {
    let daemon = Daemon::start();
    let mut client = run_shell(&daemon, "fails", "exit 7");
    assert_eq!(client.wait().unwrap().code(), Some(7));
}

#[test]
fn advisory_runs_never_wait() {
    let daemon = Daemon::start();
    let started_file = daemon.dir.path().join("holder");
    let mut holder = run_shell(
        &daemon,
        "holder",
        &format!("touch {}; sleep 30", started_file.display()),
    );
    wait_until("the holder to start", || started_file.exists());
    let started = Instant::now();
    let out = daemon
        .lease()
        .args([
            "run",
            "laptop",
            "--est",
            "1m",
            "--label",
            "advisory",
            "--advisory",
            "--",
            "sh",
            "-c",
            "echo ran",
        ])
        .output()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "advisory waited"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("running UNLEASED on laptop"), "{stderr}");
    assert!(stderr.contains("Recorded as `unleased`"), "{stderr}");
    wait_until("the advisory request to be withdrawn", || {
        daemon.log().contains("abandon")
    });
    let unleased = log_events(&daemon, "unleased");
    assert_eq!(unleased.len(), 1, "{}", daemon.log());
    assert_eq!(unleased[0]["label"], "advisory");
    assert!(unleased[0]["id"].is_u64(), "names the withdrawn request");
    assert!(
        unleased[0]["detail"]
            .as_str()
            .unwrap()
            .contains("held by \"holder\""),
        "{}",
        daemon.log()
    );
    holder.kill().unwrap();
    holder.wait().unwrap();

    // With the daemon down, an advisory run still runs, and the log still
    // gets its entry.
    let socket = daemon.dir.path().join("nowhere.sock");
    let out = daemon
        .lease()
        .env("LEASE_SOCKET", &socket)
        .args([
            "run",
            "laptop",
            "--est",
            "1m",
            "--label",
            "down",
            "--advisory",
            "--",
            "true",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unreachable"), "{stderr}");
    assert!(stderr.contains("Recorded as `unleased`"), "{stderr}");
    let unleased = log_events(&daemon, "unleased");
    assert_eq!(unleased.len(), 2, "{}", daemon.log());
    assert_eq!(unleased[1]["label"], "down");
}

#[test]
fn a_signal_ends_a_wait_at_once() {
    let daemon = Daemon::start();
    let started_file = daemon.dir.path().join("holder");
    let mut holder = run_shell(
        &daemon,
        "holder",
        &format!("touch {}; sleep 30", started_file.display()),
    );
    wait_until("the holder to start", || started_file.exists());
    let mut waiter = run_shell(&daemon, "waiter", "true");
    wait_until("the waiter to queue", || {
        daemon.log().contains("\"label\":\"waiter\"")
    });
    let signalled = Instant::now();
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(waiter.id() as i32, libc::SIGTERM);
    }
    let status = waiter.wait().unwrap();
    assert!(
        signalled.elapsed() < Duration::from_secs(5),
        "a queued client ignored SIGTERM"
    );
    assert_eq!(status.code(), Some(75));
    wait_until("the request to be withdrawn", || {
        daemon.log().contains("abandon")
    });
    holder.kill().unwrap();
    holder.wait().unwrap();
}

#[test]
fn only_the_flag_makes_a_run_advisory() {
    // An inherited LEASE_ADVISORY (or one a wrapper failed to clear) no longer
    // lets a run skip the queue.
    let daemon = Daemon::start();
    let started_file = daemon.dir.path().join("holder");
    let mut holder = run_shell(
        &daemon,
        "holder",
        &format!("touch {}; sleep 30", started_file.display()),
    );
    wait_until("the holder to start", || started_file.exists());
    let ran_file = daemon.dir.path().join("ran");
    let mut waiter = daemon
        .lease()
        .env("LEASE_ADVISORY", "1")
        .args(["run", "laptop", "--est", "1m", "--label", "waiter", "--"])
        .args(["touch", ran_file.to_str().unwrap()])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the waiter to queue", || {
        daemon.log().contains("\"label\":\"waiter\"")
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(!ran_file.exists(), "LEASE_ADVISORY=1 skipped the queue");
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(waiter.wait().unwrap().success());
    assert!(ran_file.exists());
    assert!(log_events(&daemon, "unleased").is_empty());
}

#[test]
fn wrappers_record_runs_that_skip_the_lease() {
    let daemon = Daemon::start();
    let out = daemon
        .lease()
        .args(["unleased", "desktop", "--label", "pc-run x/y: cargo test"])
        .args(["--reason", "LEASE_MODE=off"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("running UNLEASED on desktop"), "{stderr}");
    let unleased = log_events(&daemon, "unleased");
    assert_eq!(unleased.len(), 1, "{}", daemon.log());
    assert_eq!(unleased[0]["machine"], "desktop");
    assert_eq!(unleased[0]["detail"], "LEASE_MODE=off");
    assert!(unleased[0]["id"].is_null());
    let shown = daemon.lease().arg("log").output().unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown.contains("unleased") && shown.contains("LEASE_MODE=off"),
        "{shown}"
    );
}

#[test]
fn jobs_run_under_bellows_by_default() {
    let daemon = Daemon::start();
    let mut client = run_shell(&daemon, "wrapped", "exit 3");
    assert_eq!(client.wait().unwrap().code(), Some(3));
    assert_eq!(
        daemon.bellows_calls(),
        ["run --machine-state -- sh -c exit 3"]
    );
}

#[test]
fn no_bellows_runs_the_command_as_given() {
    let daemon = Daemon::start();
    let status = daemon
        .lease()
        .args(["run", "laptop", "--est", "1m", "--label", "plain"])
        .args(["--no-bellows", "--", "sh", "-c", "exit 4"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(4));
    assert!(daemon.bellows_calls().is_empty());
}

#[test]
fn a_job_inside_a_bellows_build_is_not_wrapped_again() {
    let daemon = Daemon::start();
    let status = daemon
        .lease()
        .args(["run", "laptop", "--est", "1m", "--label", "nested"])
        .args(["--", "sh", "-c", "exit 0"])
        .env("BELLOWS_SESSION_ID", "outer")
        .env("RUSTC_WRAPPER", "/usr/bin/bellows")
        .status()
        .unwrap();
    assert!(status.success());
    assert!(daemon.bellows_calls().is_empty());
}

#[test]
fn a_job_without_bellows_on_path_is_refused_before_it_queues() {
    let daemon = Daemon::start();
    let out = daemon
        .lease()
        .args(["run", "laptop", "--est", "1m", "--label", "nowhere"])
        .args(["--", "true"])
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--no-bellows"), "{stderr}");
    assert!(
        log_events(&daemon, "request").is_empty(),
        "{}",
        daemon.log()
    );
}

fn log_events(daemon: &Daemon, event: &str) -> Vec<serde_json::Value> {
    daemon
        .log()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["event"] == event)
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn status_flags_unleased_compilers() {
    let daemon = Daemon::start();
    // A process named `rustc` that stays for a while: the shell under that
    // name, waiting for a `sleep`. It cannot be `sleep` itself under that
    // name: the coreutils of Ubuntu 26.04 refuse to run as anything but
    // themselves. The trailing `:` keeps the shell from replacing itself
    // with `sleep`, which would rename the process.
    const WAIT: &str = "sleep 30; :";
    let fake = daemon.dir.path().join("rustc");
    std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    let unleased_dir = daemon.dir.path().join("unleased");
    let leased_dir = daemon.dir.path().join("leased");
    std::fs::create_dir_all(&unleased_dir).unwrap();
    std::fs::create_dir_all(&leased_dir).unwrap();
    // Orphaned (reparented away from this test), as under a GitHub runner
    // this test's own ancestry includes Runner.Worker, which counts as CI.
    let pid_file = daemon.dir.path().join("unleased.pid");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "\"{}\" -c '{WAIT}' & echo $! > \"{}\"",
            fake.display(),
            pid_file.display()
        ))
        .current_dir(&unleased_dir)
        .env_remove("LEASE_ID")
        .status()
        .unwrap();
    assert!(status.success());
    let unleased: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut leased = daemon
        .lease()
        .args(["run", "laptop", "--est", "1m", "--label", "leased", "--"])
        .arg(&fake)
        .args(["-c", WAIT])
        .current_dir(&leased_dir)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the leased compiler to start", || {
        daemon.log().contains("\"grant\"")
    });
    std::thread::sleep(Duration::from_millis(300));
    let out = daemon.lease().arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&unleased_dir.display().to_string()), "{text}");
    assert!(!text.contains(&leased_dir.display().to_string()), "{text}");
    // SAFETY: plain kill of the orphan this test started.
    unsafe {
        libc::kill(unleased, libc::SIGKILL);
    }
    leased.kill().unwrap();
    leased.wait().unwrap();
}
