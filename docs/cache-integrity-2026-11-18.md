# Incremental sessions and the wrapper (2026-11-18 Manifold failures)

Status: **approved 2026-11-18 and implemented in 0.3.18** (branch `fix/cache-integrity`
from `main` at `cb12c12`, 0.3.17). The owner decided both open questions: the lease
wraps every job in Bellows by default, and status lines stay out of Cargo's saved
output. Implementation notes are at the end.

Scope, as revised by the orchestrator:

1. Did Bellows cause the E0463 in `~/patina-logs/c14-e0463-evidence/README.md`, and if so,
   how is it fixed?
2. Why did `RUSTC_WRAPPER` seem set before the 11-18 reboot and not after? Should the
   install set it durably, or should scripts always call `bellows run --`?

The link failure after the reboot is rustc's own incremental state, not Bellows, and is
out of scope here. The evidence files are untouched.

## 1. The E0463 is a Bellows defect

### Cause

A library unit can be compiled in two incremental sessions:

- **Bellows' own per-unit session**, `{crate}-bellows{extra}`, used when Bellows compiles
  a miss. Bellows adds `--remap-path-prefix` to that compile.
- **Cargo's session**, used by any other compile of the same unit: a plain `cargo` build
  or a Bellows passthrough. These compiles have no remapping.

From the same source, the two produce **different metadata with different SVHs**.

When a session's metadata has not changed, rustc reuses it: it **hard-links the saved
`metadata.rmeta` from the session into `deps/`, keeping the file's old mtime**. So when a
unit switches sessions, rustc can put back the other session's older rmeta under its old
mtime.

Cargo rebuilds a dependent when its dependency is rebuilt in the same invocation.
Otherwise it compares mtimes. So a later invocation keeps dependents that were built
against an SVH that no longer exists. Bellows' store served nothing wrong: the fault is
in how Bellows manages incremental sessions.

### Lane-1's timeline (from its Bellows event log)

| When | What happened |
|---|---|
| 11-14 11:37:54 | `manifold_schema` misses and is compiled in Bellows' session, then stored (SVH c780…). |
| 11-14 15:15:44 | `manifold_l10n_types` misses and is stored, built against that rmeta. |
| 11-17 15:12:43 | The log's last event. |
| 11-17 22:30 | `manifold_schema` recompiles. No events were logged, so this was a **plain `cargo` build**. It reused Cargo's 11-13 session, which brought back the 11-13 rmeta (SVH 7c15…) with its 11-13 mtime. |
| 11-18 | Later invocations compare 11-13 < 11-14 and keep `l10n_types` Fresh. `mod-types` fails with E0463. |

This settles the question the evidence README left open: why the 11-17 compile used
Cargo's session.

### Reproduced

Setup: a workspace `a` ← `b` ← `c`, dev profile with `incremental = true`, rustc 1.96.1,
bellows 0.3.17.

1. `cargo build`
2. Touch `a`, then `bellows cargo build`
3. Touch `a`, then `cargo build -p a`. The step-1 rmeta comes back: inode 6298, with its
   step-1 mtime and 3 links.
4. Touch `c`, then `cargo build`. `b` is Fresh, and **`c` fails with E0460**: "found
   possibly newer version of crate `a` which `b` depends on".

Deleting Cargo's session for `a` after step 2 fixes it: step 3 writes a new rmeta, step 4
rebuilds `b`, and the build exits 0.

**Bellows alone** reaches the same broken state:

1. Build `a`.
2. Edit `a`, then build. The session is reused.
3. Revert `a`, then build. L1 restores `a`, `b` and `c` (inode 7165, 12:53:32).
4. Edit `a` again, then `bellows cargo build -p a`. Bellows' own session puts back inode
   7126 (12:53:30) over the restored file.
5. Build. `b` is Fresh against the other version of `a`. In this run Bellows then
   restored a cached `c`, which hid the error.

Scripts and logs are in the side-session notes folder (`bellows-integrity/`).

### Fix: incremental coherence

The invariant: **rustc may reuse metadata from a session only if that session's
`metadata.rmeta` is the file now in `deps/`**, meaning the same inode or the same bytes.

a. **Record the current rmeta.** Before Bellows restores a library unit or runs rustc for
   it (a miss or a passthrough), it records `deps/lib{unit}.rmeta`: inode, link count and
   mtime. It reads the digest from the digest cache only when a comparison needs it.

b. **Reset a stale Bellows session.** Bellows deletes its own unit session when either:
   - the latest finalized session has a `metadata.rmeta` that is neither that file nor
     the same bytes, or `deps/` has no rmeta (checked before a compile);
   - a restore writes bytes that differ from the session's (checked on restore).

   The next compile then starts from scratch, which also makes it publishable. In normal
   edit loops the session holds the current file, so incremental reuse is unchanged.

c. **Remove other sessions holding the replaced file.** When Bellows replaces the rmeta
   and the old file had other links, it removes every session directory that holds one:
   - Cargo's `incremental/{crate}-{id}` (the E0463 case), or Bellows' own.
   - They are found by inode under the profile's `incremental/`, and the search stops
     after `links - 1` matches.
   - The inner directory in Bellows' unit directory has the same name as Cargo's
     directory for that unit (seen in the repro), so the search checks that name first.

   This is safe because Cargo never compiles one unit twice at the same time.

d. **Repair a stale mtime after a compile.** After any compile Bellows runs: if the rmeta
   is older than the compile's start and its bytes differ from the recorded file, Bellows
   sets its mtime to now and records a `stale_metadata` event. This should never fire once
   a–c are in place. It repairs state left by older Bellows versions and by plain builds.

**Cost:**
- one `stat` per compile or restore of a library unit;
- a directory scan only when the replaced rmeta has extra links;
- incremental reuse is lost only when a session really is stale.

**Limit:** Bellows cannot fix a plain `cargo` build on its own. With this fix it no
longer leaves a stale session behind, and the next wrapped compile repairs whatever a
plain build left (d).

Docs: update the "Incremental edit loops" section of `local-builds.md`.

### Tests

- **Functional (real cargo and rustc):**
  - `mixed_plain_and_wrapped_builds_keep_dependents_consistent`: the repro above. It
    fails with E0460 on 0.3.17.
  - `a_restore_resets_a_stale_unit_session`: the Bellows-only sequence, with `c` compiled
    for real.
- **Unit, session finder:** matches by inode, stops at the link count, and skips other
  units' sessions and `-working` sessions.
- **Unit, mtime guard:** touches the rmeta when its mtime is old and its bytes changed;
  leaves it alone when the bytes are the same.
- **Edit loop:** no reset when the session holds the current rmeta. This guards against
  a speed regression.

All builds and tests run under `lease run laptop --prio 10`, starting with
`~/.local/bin/power-check`. Release as 0.3.18.

## 2. The wrapper was never set durably; nothing was lost at the reboot

### Findings

- **No durable source sets it.** I checked shell profiles, fish universal variables,
  `environment.d`, `/etc/environment`, `/etc/profile.d`, the systemd user environment,
  `~/.cargo/config.toml`, the lane's `.cargo/config.toml`, and the Claude and lease
  configuration. None sets `RUSTC_WRAPPER`. The client session's environment had it
  empty at 11-17 21:25, before the reboot.
- **Lane-1's wrapped builds came from explicit `bellows run --` calls.** Every one of its
  ~53,000 events from 11-14 to 11-17 carries a build-session ID, which only a parent
  `bellows run` or `bellows cargo` sets. That is 187 sessions:

  | Day | Sessions |
  |---|---|
  | 11-14 | 129 |
  | 11-15 | 49 |
  | 11-16 | 6 |
  | 11-17 | 3 |

  By 11-16 the only wrapped runs were the `*-final-wide.sh` scripts
  (`bellows run -- python3 scripts/affected-tests.py`). Per-stage scripts called plain
  `cargo`. **The lane was mixed all along**, which is exactly how the E0463 happened.
- **Wrapped builds stopped 21 hours before the reboot.** The last one ended at
  11-17 15:12:43. The c14 scripts written after that called plain `cargo`.
- **What made the wrapper look active:** Cargo saves each unit's compiler stderr and
  replays it whenever the unit is fresh. The wrapper prints its decisions to that
  stderr, so 2,582 of lane-1's `test-fast/.fingerprint/*/output-*` files hold Bellows
  lines. Every build printed about 591 of them, even with `RUSTC_WRAPPER=` set.

### Recommendation: `bellows run --` at the job launch, not a durable `RUSTC_WRAPPER`

A bare, durable `RUSTC_WRAPPER` is the wrong default here:

- **No storage budget.** The budget (`BELLOWS_MAX_MB`) and the 20 GiB notice run only
  when a `bellows run`/`cargo`/`local` parent finishes. Bare wrappers never collect, so
  each checkout's `.bellows/l1-v6` grows without bound.
- **No warning when the server is down.** "Server unreachable" is printed by the parent.
  Bare wrappers fall back to plain rustc silently, crate by crate. That is how the
  overnight bees built uncached on 2026-10-05.
- **It wraps every Rust build on the machine.** That includes other projects and
  `cargo install`, and the exact diagnostic and trybuild snapshot tests that the
  machine's notes say to run without Bellows.

Instead:

- **Wrap each leased job once, at launch:**
  `lease run laptop … -- bellows run -- bash script.sh`. Every `cargo` inside is
  wrapped, the job gets one build session, the budget is enforced at the end, and an
  unreachable server is reported. A step that must run unwrapped sets `RUSTC_WRAPPER=`.
- **State directory:** `bellows run` puts its store in the launch directory's
  `.bellows`. Set `BELLOWS_STATE_DIR` to one machine-wide directory in the same launch
  line, which matches the one-capped-cache rule.
- **Owner's call:** a lease option that runs every job on a build machine under
  `bellows run` (with an opt-out) would remove the per-script convention entirely.
- **Owner's call:** stop the wrapper writing decision lines to rustc's stderr, which
  Cargo saves and replays. The parent commands would print them live from the build
  session's log instead; with a bare wrapper they would go only to `bellows explain`.
  Then an unwrapped build can no longer look wrapped. This changes what a wrapped build
  prints and when.

**Until 0.3.18 is installed:** keep each target directory either wrapped or plain.
Lane-1 is mixed now. Before switching it back to `bellows run`, run
`cargo clean --profile test-fast` there.

## Dropped from scope (available if wanted)

- Structure checks on store and restore (rlib members, rmeta header) and installing a
  restore as a set.
- `bellows evict --file/--crate` for L1, `store-v6` and the server (needs a new admin
  endpoint).
- **Not checked:**
  - `CARGO_LOG` is part of compile keys (seen in the repro).
  - Whether a crate's key covers the rmetas it loads transitively. In the Bellows-only
    run, `c` hit while `a`'s rmeta had changed under a stale `b`.
  - Incremental directories of obsolete units are never removed: `cargo clean -p
    manifold-client-state` freed 93.8 GiB in lane-1.

## Implementation notes (0.3.18)

- `crates/bellows-cli/src/incremental.rs` holds the coherence rules; `main.rs` calls them
  around a miss compile, a restore and a passthrough.
- rustc leaves the previous session beside the new one until its next compile, so only a
  per-crate directory's **latest finished** session counts. It is chosen by the base-36
  timestamp in its name. Judging every session would have reset edit loops.
- Session resets are recorded (`session_reset`) but not printed. A repaired mtime is
  printed (`REPAIRED`).
- Status lines: wrappers only record. `bellows cargo`/`local`/`run` follow the build's
  event log (across rotations) and print them.
- `bellows run --machine-state` uses the `bellows cargo` state directory.
  `lease run`/`hold` wrap jobs with it unless `--no-bellows` is given, or the job is
  already inside a Bellows build. With no `bellows` on `PATH`, the job is refused before
  it queues.
- Tests:
  - Fail with E0460 on 0.3.17, pass now:
    `mixed_plain_and_wrapped_builds_keep_dependents_consistent` and
    `a_restore_resets_a_stale_unit_session`.
  - Fails on 0.3.17: `status_lines_stay_out_of_cargos_saved_compiler_output`.
  - Passes on both, as a guard against slower edit loops:
    `an_edit_loop_keeps_its_incremental_session`.
  - Unit tests for the session rules and the log tail.
  - Lease tests use a stand-in `bellows`.

