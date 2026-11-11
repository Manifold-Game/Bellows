# Diagnosing Bellows builds

`bellows cargo`, `bellows local`, and `bellows run` print a build ID and a short
summary on stderr after the wrapped command finishes. Cargo's stdout, including
JSON output, remains available for consumers. A failed cache setup warns and
runs the requested command normally; the command's exit status is preserved.

Start with the most recent wrapped build in the current workspace:

```bash
bellows explain --local --latest --summary
bellows explain --local --latest --crate manifold_render
bellows stats --local --latest --json
```

For a custom cache, add `--cache-dir /path/to/cache`. For a server-backed build,
omit `--local`; server stats still require the configured server. `explain`
itself only reads local diagnostics and works offline. Without explicit state
configuration, `explain` selects the newer of the workspace and default local
logs. `BELLOWS_EVENT_LOG` overrides log selection for both stats and explain.

Inspect an earlier or concurrent build using its printed ID:

```bash
bellows explain --local --session 1789850000000-12345 --summary --json
bellows explain --local --session 1789850000000-12345 --crate manifold_render --limit 100
```

`--latest` selects the most recently *started* wrapped build in this workspace,
even when an older concurrent build finishes after it. `--session` selects that
exact build. Without either option, queries include the retained log history.
An ordinary Cargo no-op build correctly reports zero compiler-cache decisions.
Direct `RUSTC_WRAPPER` use without a Bellows parent produces ungrouped events;
omit `--latest` to inspect those, or wrap the Cargo command with `bellows run`.

## Reading the output

`bellows stats` groups this workspace's decisions by what they mean:

```text
This workspace
  reused             1386  libraries 940 · linked outputs 344 · build-script runs 102
  rebuilt              40  environment differs 40
  not cacheable         3  by design: compiler probes 3
  problems              0  none
  stored               40  pinned to checkout 40
  not stored            2  compiled, kept local: incremental session reuse 2
```

*Reused* work is grouped by what was restored: rlib/rmeta libraries, linked
outputs (executables, test harnesses, `cdylib`/`dylib`, procedural-macro
crates and build-script executables), and build-script runs. *Rebuilt* work
was compiled because no usable entry existed; its causes use the reason codes
below. *Not cacheable* work is outside the transparent cache's correctness
boundary by design (see bypasses below) and never indicates a fault.
*Problems* are fallbacks and corrupt entries, the only decisions that call
for action. *Stored* results are either **shareable** across checkouts or
**pinned to checkout**, because their bytes embed the workspace, target, or
session checkout path (for example a test that bakes
`env!("CARGO_MANIFEST_DIR")`). A pinned result is reused only at the same
path. *Not stored* work compiled normally but was deliberately kept local:
an incremental compile that reused a session, a build script that ran over an
existing `OUT_DIR`, a library that bundles a native archive from a `#[link]`
attribute, or an output Bellows cannot represent (a directory, or too many
files). Examples follow for rebuilt work, problems, and not-stored work.

`BELLOWS_OUTPUT` controls the live stream: `events` (default) prints one line per
decision, `summary` prints only problems plus each wrapped build's closing line,
and `quiet` prints errors only. All decisions are recorded regardless. CI logs
are clearest with `summary` followed by `bellows stats`.

`bellows doctor` also inspects the current checkout's Cargo target directory
(`CARGO_TARGET_DIR`, or `target/` at the git root). It reports packages whose
build-script `output` or dep-info depends on files in a *different* git
checkout, including another worktree's git `HEAD`. Results restored by
Bellows before 0.3.1 could leave such outputs behind. Cargo keeps them fresh
because it watches the other checkout's files, so doctor names the
`cargo clean -p <package>` that rebuilds them. A checkout's own git metadata,
including refs shared between worktrees, is not reported.

Configuration is not a miss cause. `bellows doctor` reports the local cache
(`off (BELLOWS_L1=0)` or `on`) and the output level once; an individual miss
names the local cache only when it failed to open, alongside that fallback.

## What a miss means

A miss explanation starts with `not cached yet:` (no earlier build of this crate
variant was seen here), `not cached: differs from an earlier build of this
crate:` followed by what differs, or `not cached: built before, but its entry
was evicted or never saved`. Logs written before this wording used
`first observed identity`, `static identity changed:` and `previously observed
identity`; both forms map to the same reason codes.

| Reason | Evidence and next step |
|---|---|
| `input_changed` | Names the dependency or explicit compiler input whose digest changed. |
| `input_changed` (another checkout) | `… belongs to another checkout`: the record read files in a different git worktree, so it never serves this one. |
| `input_missing` | Names a previously recorded input that can no longer be read. |
| `environment_changed` | Names the changed environment variable. A path-valued `env!` dependency compares normalized (`$TARGET/…`) only when no output embeds the path; otherwise literally. `@bellows:root:$WORKSPACE`, `$TARGET` or `$CHECKOUT` means the cached output embeds another checkout's path. |
| `compiler_changed` | The exact compiler version identity changed. |
| `identity_changed` | Lists protocol or argument-group differences with old/new hash prefixes. |
| `cold_identity` | No earlier diagnostic identity exists for this crate/source locally. Bellows cannot attribute the miss to a particular edit yet. |
| `entry_missing` | This static identity was observed before, but no usable record remains. Possible eviction or failed/never-completed publication is stated explicitly rather than guessed. |
| `artifact_unavailable` | The `corrupt` event identifies the missing/corrupt blob or restore path; the miss links to that event. |
| `remote_unavailable` | The accompanying fallback contains the request failure. Compilation continues locally. |

Static-key changes are compared against up to eight recent identities for the
same crate/source. The comparison closest to the current one supplies the
explanation; it is identified by key prefix. These snapshots are diagnostic
evidence only and never authorize cache hits. Compiler identity, flags, and
environment values are stored as hashes. Paths and variable/flag names remain
readable, while flag/environment values are not copied into the identity log.
On a fresh CI runner with no diagnostic history, Bellows reports that limitation.
Preserve the client state directory if cross-run identity comparisons are wanted.

A different Cargo target-directory path can legitimately produce
`environment_changed: OUT_DIR`, followed by `input_changed` for crates that
depend on the rebuilt metadata. The Manifold canary demonstrated this with
`serde_core` and `manifold_mod_types`. Clearing and reusing the same target path
avoids that particular difference; moving to a new path must retain exact
environment validation.

Concurrent builds can share a static key while requiring different candidates.
After the first owner finishes, a `lease_acquired` event means the waiting build
has taken the released lease to compile its own variant. Rejected candidates
are not logged again on every wait poll. A `wait_timeout` fallback identifies
an owner that did not supply a usable result or release its lease within the
configured wait budget.

Bypasses are distinct from misses. Reasons include `native_inputs` (`-L all=`,
`crate=` or bare `-L` search paths, which can supply crates, and unknown `-l`
kinds), `linked_output` (`staticlib`), `unstable_flags` (`-Z` flags other than
`unstable-options`, `force-unstable-if-unmarked`, `share-generics`, `threads`,
`macro-backtrace`), `compiler_probe`, and `unsupported_outputs`. Earlier logs
may also contain `incremental` and `proc_macro`, which are no longer bypasses.
They describe work outside the transparent cache's correctness boundary.

Hit details name the output kind and, for crates that load them directly, the
procedural macros used: `restored runner-local compiler result (library
output; proc macros: serde_derive, thiserror_impl)`. Build-script decisions
appear under the crate name `build-script:<package>`.

| Not stored | Meaning |
|---|---|
| `incremental_session` | The compile reused an incremental session, so it stays in this checkout. |
| `native_inputs` | The rlib bundles native members that no `-l static` library supplied. |
| `unsupported_outputs` | rustc produced a directory output (split debuginfo), or a build script ran over an existing `OUT_DIR` or produced a symlink. |
| `too_many_outputs` | The output set exceeds the candidate limits. |
`symlink_input` identifies a compiler input reached through a symlink or
junction that the record could not account for: one below the compile's
working directory or checkout, one that leads out of them, or a `..` straight
after one. If it is discovered in dep-info after compilation, capture is
skipped safely. A link at or above the compile's own working directory or
checkout is followed and is not reported (see
[linked outputs](linked-outputs.md#changes-made-during-implementation)).

## Structured output and retention

`explain --json` remains a JSON event array. Events add optional `reason`,
`session_id`, `workspace`, and `duration_ms` fields; older event records remain
readable. The existing `kind`, `detail`, `crate_name`, `static_key`, and
`action_key` fields are retained.

`explain --summary --json` returns counts grouped by decision and reason, all
affected crate names, and up to three example details per group. Text summaries
show the most common groups; `--limit` controls their number. `stats --json`
retains its `remote` and `events` fields and adds a `diagnostics` summary.
`elapsed_ms` is wrapped-command wall time for one selected build.
`compiler_process_ms` sums wrapper durations, including lookup/restore/compile
work, and may exceed wall time because compilers run concurrently. It is not an
estimate of time saved.

The event log rotates at 16 MiB and retains two previous files (`events.jsonl.1`
and `.2`). Readers include all three under the same cross-process lock used by
writers. Old sessions eventually age out; summaries cover retained events.
Malformed lines are counted and skipped with a warning. Diagnostic failures
never change the compiler's result.
