# Bellows

**Cargo-native remote builds. Keep Cargo. Keep official rustc. Skip work the
fleet already did.**

Bellows is a working five-phase remote-build demonstrator for Rust. It combines
a Cargo-compatible compiler cache, immutable test archives, declared Cargo
action caching, authenticated remote execution, and advisory compiler-aware
impact analysis over one verified content-addressed store.

```text
cold runner A                 bellowsd                 fresh runner B
    rustc  ── publish ──>  action index + CAS  <── restore ── Cargo
      │                           │                              │
 official compiler         verified BLAKE3 blobs          local final link
```

## Try the five-phase showcase

Requirements are Rust 1.92+ and `curl`. Then run:

```bash
./scripts/five-phase-demo.sh
```

The showcase proves, with machine-checked assertions:

1. Runner-local L1 and fresh-runner remote compiler hits.
2. Compile-once/test-many archives restored to two independent executors.
3. Cached final links, build-script output, and a nested Cargo generator.
4. Authenticated, toolchain-attested, single-flight remote Cargo execution.
5. Advisory private/public-surface change analysis and downstream closure.
6. Publish-once archive safety, precise invalidation, reference-aware GC, and
   publication-race protection.

For the shorter compiler-cache correctness suite, run `./scripts/demo.sh`. It
proves these eight Phase 1 behaviors in a disposable workspace:

1. A cold runner compiles libraries with official `rustc` and publishes them.
2. A fresh runner restores `.rmeta`, `.rlib`, and dep-info remotely.
3. Restarting `bellowsd` retains the same durable cache.
4. Editing a transitive `mod` file produces a safe miss naming that exact file.
5. Two concurrent cold runners compile each identical library action once.
6. Wrappers publishing through a standalone `bellows relay` have their
   uploads drained on SIGTERM, and a fresh runner reuses them.
7. Corrupted remote blobs fail integrity checks and are rebuilt, never used.
8. Stopping `bellowsd` still leaves a successful ordinary Cargo build.

Nothing from the demo is retained outside its temporary directory.

## Manual use

For native Windows, use the [setup and qualification instructions](docs/windows.md).
The Windows CI job runs the same `scripts/windows-check.ps1` checks.

### Local builds

No daemon is required for a persistent local cache:

```bash
bellows cargo run --release -p flagship
```

`bellows cargo …` is shorthand for `bellows local -- cargo …`. Use the
longer form only when wrapping a non-Cargo command or selecting `--cache-dir`
explicitly.

Local mode stores verified compiler artifacts in the platform user cache
(`$XDG_CACHE_HOME/bellows`, `~/.cache/bellows`, or the Windows local app-data
directory). Override it with `--cache-dir` or `BELLOWS_STATE_DIR`. It never
constructs a network client, does not require `bellowsd`, and preserves Cargo's
incremental setting. Incremental dev-profile compiles are looked up like any
other; on a miss rustc keeps its incremental session, and only from-scratch
results are published (see [local builds](docs/local-builds.md)).

```bash
bellows stats --local
bellows gc --local --max-mb 20000
```

Each wrapped build prints its build ID, elapsed time, hit/miss/bypass counts,
and the most common reasons. Inspect that build without lifetime-log noise:

```bash
bellows explain --local --latest --summary
bellows explain --local --latest --crate manifold_render
bellows stats --local --latest --json
```

Misses identify changed source/dependency paths, environment variable names,
compiler versions, or flag groups. Identity comparisons store hashes rather
than environment or argument values. See [diagnosing builds](docs/diagnostics.md)
for session selection, JSON fields, and log retention.

Use a local declared action for deterministic whole outputs such as a final
link or nested Cargo product:

```bash
bellows action run --local --name generated-component-v1 \
  --input Cargo.toml --input Cargo.lock --input src \
  --output target/generated \
  -- cargo build --release --locked --offline --target-dir target/generated
```

The local store is shared across target directories, branches, and worktrees.
Cargo remains the first-level build graph: an intact `target/` is still the
fastest no-op build, while Bellows restores eligible work when Cargo needs an
artifact again. See [local build acceleration](docs/local-builds.md) for the
correctness boundary and the Flagship workflow.

### Shared service

Build and start the server:

```bash
cargo build --workspace --bins
./target/debug/bellowsd --listen 127.0.0.1:7878 --data-dir .bellows/server
```

In another terminal, from any Rust workspace:

```bash
/path/to/Bellows/target/debug/bellows doctor
/path/to/Bellows/target/debug/bellows run -- cargo build
rm -rf target
/path/to/Bellows/target/debug/bellows run -- cargo build
/path/to/Bellows/target/debug/bellows stats
/path/to/Bellows/target/debug/bellows explain
```

The second build uses a completely empty local target directory. Bellows checks
its verified runner-local L1 before the organization cache; set `BELLOWS_L1=0`
when measuring remote behavior in isolation.

For a server across a network, `bellows run` puts a loopback relay between
Cargo's wrapper processes and the server: one warm connection pool for the
whole build, and uploads sent in the background, so a warm restore costs about
two round trips per crate. Start `bellows relay` yourself when Cargo runs with
`RUSTC_WRAPPER` set directly, as in CI. See
[far servers](docs/operations.md#far-servers-the-relay).

For authentication, start `bellowsd` with `--auth-token` or
`BELLOWS_AUTH_TOKEN` and give the client the same environment variable. A
non-loopback listener refuses to start without authentication. Server data is
durable across restarts. See the [operator runbook](docs/operations.md) before
running a shared service.

## What is safe to cache today

The transparent wrapper caches Rust libraries and metadata, final links
(binaries, `--test` harnesses, examples, `cdylib`/`dylib`, procedural-macro
crates, build-script executables) and build-script runs, whenever it can prove
an unambiguous action identity. Its key covers:

- exact `rustc -vV` identity;
- normalized file-location arguments, exact literal flags, and relevant environment;
- primary sources and explicitly supplied `--extern` artifacts, including
  procedural-macro `.so`/`.dll` files;
- all transitive files learned from rustc dep-info;
- all `env!` and `option_env!` values reported as dep-info env dependencies;
- for links, every file the linker reads, taken from rustc's own
  `--print link-args`: transitive rlibs, objects, linker scripts, every
  candidate a `-l` could resolve to, C runtime objects, and the linker programs;
- Cargo's unique `-C extra-filename` output identity.

See [linked outputs](docs/linked-outputs.md) for the design and the failure
modes each rule guards against.

The static key includes compiler-, Cargo-, target-, and native-toolchain
environment variables, but deliberately excludes per-run CI metadata such as
`GITHUB_RUN_ID`. Arbitrary variables actually consumed through `env!` or
`option_env!` remain safe because rustc reports and Bellows revalidates them in
the candidate manifest.

A static command identity retains several dependency manifests, allowing old
branches to become hits again. Every candidate is revalidated before restore.
Inputs are hashed after rustc (or a build script) finishes, so a source,
registry or toolchain file changed at or after the run started (an editor
save, a `git checkout` into the same worktree) is never recorded with that
result: the run stays local. Modification and change time both count, so a
`cp -p` that keeps an old modification time is caught too. Files in the target directory are exempt; Cargo
writes them under its target-dir lock before the units that read them start.
Every output is scanned for the workspace, target, and session checkout paths
after a remapped compile. A result that embeds one is pinned to that checkout;
otherwise path-valued `env!` values compare relative to those roots, so
worktrees share it.
Inputs reached through symlinks conservatively bypass publication until their
resolution can be modeled. Unmodeled emit kinds and custom output destinations
also bypass, so a successful hit always supplies the requested output set.
Search paths that can supply crates (bare `-L`, `-L all=`, `-L crate=`), custom
sysroots and target JSON files, and `target-cpu=native` bypass. `-L native=`
and `-L framework=` are accepted: static archives bundled into an rlib are
resolved and hashed, and any other bundled native member stops publication. Literal flag values such as `--cfg`
strings are never path-normalized.
Every downloaded blob is rehashed. Dep-info and compiler streams are normalized
against workspace, target, Cargo, Rustup, and home roots, then localized on the
receiving runner.

The transparent rustc wrapper still bypasses static libraries, compiler probes,
response files, most unstable `-Z` flags, profile/plugin codegen inputs, and
ambiguous emit sets. Link arguments and `-C linker=` are part of every key and
only matter to linked outputs. Nested Cargo output that should be declared
explicitly can use `bellows action run`, whose input/output manifest and
isolated offline sandbox become the correctness boundary. A bypass is visible,
not a silent correctness bet.

Injected `--remap-path-prefix` flags make outputs checkout-independent. rustc
also applies them to diagnostics, so Bellows rewrites messages back to the
receiving checkout's real paths, live and when a cached result replays.
Cargo metadata can itself encode path-dependency package IDs, so fleets should
still use consistent checkout layouts for the best hit rate.

## Commands

| Command | Purpose |
|---|---|
| `bellows cargo <args>` | Run Cargo through the daemonless local cache |
| `bellows run -- <command>` | Run Cargo-compatible work with the wrapper installed |
| `bellows relay [--env-file F]` | Relay wrappers to a far server over warm connections, uploading in the background |
| `bellows local -- <command>` | Run with a daemonless, durable user-level cache |
| `bellows doctor` | Verify protocol, server, compiler, and fallback |
| `bellows stats [--local]` | Show CAS size and decisions grouped as reused, rebuilt (with causes), not cacheable by design, and problems |
| `bellows explain` | Show recent invalidation and fallback reasons |
| `bellows archive publish/restore` | Distribute immutable compile-once/test-many trees |
| `bellows action run` | Cache a declared local Cargo/rustc action and final outputs |
| `bellows remote run` | Schedule the same declared action on an authenticated executor |
| `bellows analyze snapshot/compare` | Explain advisory source/API and downstream impact |
| `bellows gc [--local] [--dry-run]` | Evict least recently used records and their unreferenced blobs to a storage budget |
| `bellowsd` | Run the durable HTTP CAS/action-cache service |

Configuration is available as flags or `BELLOWS_SERVER`, `BELLOWS_AUTH_TOKEN`,
`BELLOWS_DATA_DIR`, `BELLOWS_LISTEN`, `BELLOWS_MAX_BLOB_MB`, and
`BELLOWS_MAX_REQUESTS` environment variables. The default per-blob upload
ceiling is 512 MiB and the default concurrent request ceiling is 128.
Client connection and overall request timeouts can be bounded with
`BELLOWS_CONNECT_TIMEOUT_MS` (100–30,000) and `BELLOWS_REQUEST_TIMEOUT_MS`
(1,000–600,000). `BELLOWS_MAX_WAIT_MS` bounds single-flight waiting from zero
to 600,000 ms. `BELLOWS_FETCH_CONCURRENCY` (default 16) bounds one wrapper's
parallel blob requests; `BELLOWS_RELAY`, `BELLOWS_RELAY_UPLOADS`,
`BELLOWS_RELAY_SPOOL_MB` and `BELLOWS_RELAY_DRAIN_SECS` configure the relay.

Terminal output uses aligned, Cargo-style status labels and semantic color.
Color is automatic for interactive terminals and supported CI providers,
including GitHub Actions. Set `BELLOWS_COLOR=always|never|auto` to override
detection; `NO_COLOR` and `TERM=dumb` disable color in automatic mode. JSON
output remains free of styling and safe to pipe into other tools.

`BELLOWS_OUTPUT` sets how much of each build's decision stream is printed:
`events` (default) prints one line per cache decision; `summary` prints only
problems as they happen (fallbacks and corrupt entries) plus each wrapped
build's closing summary, which suits CI logs; `quiet` prints errors only. Every
decision is still recorded for `bellows stats` and `bellows explain`.
Configuration such as `BELLOWS_L1=0` is reported once by `bellows doctor`, never
as the reason for an individual miss.

## Manifold

Manifold can replace its `setup-sccache` seam while retaining its stable,
clippy, browser-stable, threaded-nightly, and GPU target namespaces. The
composite action in this repository performs that setup for a preinstalled
Bellows binary. See [the Manifold integration guide](docs/manifold.md).

The first gains come from sharing stable Rust library work across fresh cloud
runners and suppressing simultaneous cold duplication. GPU execution remains
on the RTX 5080; Bellows only supplies portable compiler products there.
The [validation record](docs/validation.md) includes the two-run smoke,
full-pipeline cold/warm WarpBuild dogfood, browser parity, production canary,
RTX 5080 evidence, and the exact local acceptance commands.

## Architecture and scope

The workspace contains:

- `bellows-core`: protocol, manifests, normalization, digesting, and filesystem CAS;
- `bellows`: user command, rustc wrapper, cache client, diagnostics, and fallback;
- `bellowsd`: authenticated Axum service, durable action index, CAS, and leases.

The [product concept](bellows-concept.md) describes the commercial and
compiler-internal destination. The [five-phase plan](five-phase-plan.md) records
the implemented demonstrator's correctness boundaries and acceptance gates.

Bellows' compiler cache is hardened for Manifold's trusted single-tenant use,
but it is not yet a generally managed multi-tenant service. In particular, it
does not provide TLS termination, tenant isolation, account quotas,
microVM sandboxing, or a stable public protocol. Remote execution is disabled by
default, requires bearer authentication, and provides trusted-single-tenant
process/workspace isolation—not a hostile multi-tenant security boundary.
Declared Cargo actions require `--locked --offline`, a synthetic credential-free
`CARGO_HOME`, and path-only or explicitly vendored dependencies.
They preserve Cargo's environment/configuration rustflags, adding path remapping
through a compiler wrapper. Restoring a declared output directory replaces its
complete tree, including removal of obsolete files. Input/output overlap is
rejected before execution; overlapping output declarations share one replacement.
The client and server reject path-qualified cargo/rustc wrappers, absolute
arguments, and parent traversal. Build scripts invoking additional host tools
remain a trusted-worker-image boundary until tracing or worker attestation is
expanded beyond the Rust/Cargo/linker toolchain.

Phase 5 computes a syntactic public-surface heuristic. Its JSON embeds the
generic/inline/default-method/macro caveat, and it never authorizes a cache hit;
stable downstream skipping still requires compiler-produced metadata.
Linux and native Windows x64 MSVC have acceptance lanes. See the
[Windows qualification scope](docs/windows.md); passing these fixtures does not
establish parity for every downstream workflow or Windows configuration.

The wire and on-disk formats are versioned but not yet stable. Review the
[protocol policy](docs/protocol.md) before upgrading a running service.
