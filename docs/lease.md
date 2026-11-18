# Machine leases (`leased`, `lease`)

`bellows-lease` schedules shared build machines: each machine runs one leased
job at a time, granted in order of priority, then merge-queue position, then
arrival. It is a separate daemon from `bellowsd` and has nothing to do with the
compiler cache.

```sh
leased --merge-queue-repo owner/name &      # one per scheduling host
lease run laptop --est 20m --label "PR 587 clippy" -- cargo clippy --workspace
lease hold desktop --est 30m --label "PR 579 desktop half" -- ./batch.sh
lease status
lease log --since 2h
```

## Jobs run under Bellows

Since 0.3.18 every job runs as `bellows run --machine-state -- COMMAND`, so
each Cargo build in it uses the compile cache and the machine's one Bellows
state directory (`BELLOWS_STATE_DIR`, else `~/.cache/bellows`), whichever
checkout it builds. Builds in a target directory are then either all wrapped
or deliberately not, rather than wrapped only when a script remembered to
say so. The job prints Bellows' decisions and its closing summary, and says
so when `bellowsd` does not answer.

- `--no-bellows` runs the command as given. Only the flag does this: no
  inherited environment variable can switch the cache off for a queue.
- A job already inside a Bellows build (a nested `lease run` in a wrapped
  `lease hold`) is not wrapped again.
- A step inside a job that must build without the cache sets
  `RUSTC_WRAPPER=` for that command.
- With no `bellows` on `PATH`, `lease run` refuses the job before queueing
  it (exit 75) and says to install Bellows or pass `--no-bellows`.

## A lease is held by a connection

`lease run` connects to the daemon's Unix socket, waits for its grant, runs the
command in a new process group and reports the group to the daemon. The lease
lasts exactly as long as that connection:

- When the command exits, the client kills anything it left in its group and
  releases the lease.
- When the client is interrupted (`SIGINT`, `SIGTERM`, `SIGHUP`), the
  administrator revokes the lease, or the daemon goes away, the client stops
  the job (`SIGTERM`, then `SIGKILL` after 5 s).
- When the client itself dies (even by `SIGKILL`), the job dies with it: the
  job's leader has `PR_SET_PDEATHSIG`, and the daemon kills the whole process
  group when the connection ends without a reported exit (`killed_orphan` in
  the log).

So a free machine never has a leased job still running on it. Restart `leased`
only when `lease status` shows no holders: a restart ends every lease.

`lease run` exits with the command's code, or 75 if the lease was refused or
lost before the command ran.

`--advisory` never waits: on a busy machine, or with the daemon down, the
command runs anyway, unleased. It says so on stderr in a banner and records an
`unleased` entry in the log, which carries the reason and the withdrawn
request's id. If the daemon is down, the client appends the entry to the log
itself. Only the flag turns this on: the environment variable `LEASE_ADVISORY`
no longer does, so an inherited setting cannot quietly skip the queue.
Wrappers that skip the lease on purpose record the run with
`lease unleased MACHINE --label TEXT --reason TEXT`.

## Sessions

`lease hold` grants a session and passes its command `LEASE_TOKEN` and
`LEASE_MACHINE`. A nested `lease run` on the same machine with that token runs
at once instead of queueing, so a multi-command batch is never interleaved with
other requests. `lease admin preempt ID` ends a session at its next command
boundary: running commands finish, and the next nested run is refused.

## CI sharing

`lease ci-start laptop --job ID` (from a CI runner's job-started hook) and
`lease ci-stop laptop --job ID` (job-completed) mark a CI job sharing the
machine. While one is active, grants on that machine still happen but carry the
reduced job count (`--ci-jobs`, default 12 instead of `--jobs` 24), exported to
the command as `LEASE_JOBS` and `CARGO_BUILD_JOBS`. A registration nobody
stops expires after three hours.

## Administration

`lease admin` reads the daemon's admin token from its state directory
(`$XDG_STATE_HOME/bellows-lease/admin.token`, mode 0600):
`reorder ID POS`, `pause MACHINE`, `resume MACHINE`, `preempt ID`,
`reserve MACHINE --label TEXT --for 30m`, `unreserve MACHINE`, and `cancel ID`
(drops a queued request, or revokes a lease and stops its job). Every request,
grant, release, overrun (past 1.5× the estimate), CI registration and
administrative action is appended to `log.jsonl` in the same directory.

Leases are cooperative for commands started outside `lease`: they run, but
nothing schedules them. `lease status` lists, by working directory, any
`rustc` on its own host that runs neither under a lease (`LEASE_ID` in its
environment) nor under a CI runner (`Runner.Worker`).

## Terminals

The job runs in its own process group. Started from an interactive terminal,
`lease run` hands that group the terminal while the job runs, so the job can
read input and receives ^C directly, and takes it back afterwards.
