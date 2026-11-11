# Design note: caching linked outputs through the rustc wrapper

Status: implemented in protocol 6 (`store-v6`). This note was written before
the code. It records what the transparent wrapper keys, what it restores, and
which failure mode each rule guards against.

## Why declared actions and archives did not help

`bellows action run` and `bellows archive publish/restore` wrap a *whole*
command with explicit, workspace-relative inputs and outputs. The rustc wrapper
never produces either record. It sees one rustc invocation at a time and only
writes `ActionCandidate` records. Manifold agents use `bellows run -- cargo …`,
so `declared actions 0, archives 0` is expected. Final links, test harnesses,
build scripts, proc-macro crates and every crate carrying Cargo's propagated
`-L native=` paths were bypassed before any identity was computed.

This change keeps one mechanism. Every new output kind is an
`ActionCandidate`: a static key computed before compiling, plus a manifest of
inputs learned from the successful compile. Each input is re-verified before
any restore. The records use the same CAS blobs, the same L1/remote lookup,
the same single-flight leases, and the same eight-candidates-per-static-key
policy. Declared actions stay the explicit, sandboxed tool for whole commands.

## What is keyed

Static key (known before compiling). This part is unchanged:

- `rustc -vV`, normalized arguments (every `-C`, `--cfg`, `--test`, `-C lto`,
  `strip`, `debuginfo`, `split-debuginfo`, `opt-level`, `panic`, target and
  link-arg flags), relevant environment, and digests of the primary source and
  every `--extern` file, including proc-macro `.so`/`.dll` files.

New in the static key:

- Output kind (`library` or `linked`) and the key-format version.
- For linked outputs, the linker identity: the resolved linker program from
  `-C linker=` or the target default (`cc`, or `link.exe` on MSVC), its
  content digest, and its `--version`/`-v` banner. Linker-selection link args
  (`-fuse-ld=`, `-B`) are resolved the same way.
- `CARGO_INCREMENTAL` is no longer part of the static key. Its effect is
  visible in the arguments, and `-C incremental=` is excluded from the key
  (see Incremental).

Candidate manifest (learned from the compile, verified before every restore):

- Every file in rustc's dep-info, as before: sources, `include_*!` files,
  and files proc macros report through their expansions.
- Every `# env-dep:` entry. A value inside the workspace or target root is
  compared *normalized* (`$WORKSPACE/…`, `$TARGET/…`) when the leak scan is
  clean, and literally otherwise.
- For linked outputs, every file the linker reads. rustc writes the exact
  linker command with `--print link-args=<tmp>`, a file, so Cargo's stdout is
  untouched. From that command Bellows records:
  - every existing file argument (transitive rlibs, objects, `.lib`, linker
    and version scripts, including paths inside `-Wl,` and `--opt=`);
  - every candidate a `-l`/`name.lib` could resolve to in `-L`/`/LIBPATH:`
    directories, then `LIBRARY_PATH`/`LIB`, then the C driver's default search
    directories. Both `.so` and `.a` are recorded. This over-approximation
    means a library that changes, or appears earlier in the search order,
    invalidates the link. A GNU ld script is followed one level.
  - C runtime objects reported by the driver (`crt1.o`, `crti.o`, and so on).

  Paths inside the Rust sysroot are covered by the compiler identity and are
  skipped. Paths outside the recognized roots are stored as absolute
  `host_files`, which are valid only on a host with byte-identical files.
- For rlib compiles that accept `-L native=`/`-l`: every `-l static` library
  that rustc bundles, resolved in the given search paths.

## Leak scan (relocatability, requirements 2 and 4)

The miss path already compiles with `--remap-path-prefix` for the workspace
and target roots. After a successful compile, every captured output (rlib,
rmeta, executable, `.so`/`.dll`, `.pdb`, `.dwp`, `.wasm`) is scanned for every
spelling of the workspace root and the target root. The spellings are
canonical, as given, and on Windows with the verbatim prefix and forward
slashes. A remapped build of path-independent code contains none of them;
this was verified on fixtures and is byte-identical across checkout paths.

- **Clean.** The candidate is shareable across worktrees. Path-valued env-deps
  are stored normalized.
- **Leaked**, for example `concat!(env!("CARGO_MANIFEST_DIR"), …)`,
  `env!("OUT_DIR")` used as a value, or a proc macro that reads
  `CARGO_MANIFEST_DIR` through `std::env`. The candidate is pinned with
  `@bellows:root:$WORKSPACE`/`$TARGET` inputs holding the digest of this
  checkout's absolute roots, and env-deps are compared literally. It hits only
  in the same checkout path.

A binary restored into another worktree therefore never carries that
worktree's paths, and never carries another worktree's paths either.
Debuginfo, `file!()` and panic locations use `/bellows/workspace` and
`/bellows/target`, which are identical for every checkout. Paths that are
relative to Cargo's working directory, such as `file!()` for workspace
members, are unchanged.

## What is restored

For a linked unit with stem `S` (`{crate}{extra-filename}` or
`lib{crate}{extra}`), the candidate holds every regular file in `--out-dir`
that starts with `S` followed by `.` or end of name, and that rustc wrote
during this compile. rustc may produce the executable, `.exe`, `.pdb`, `.dwp`,
`.wasm`, `.so`, `.dll`, `.dll.lib`, `.dll.exp`, `.dylib` and `.d`. The
predicted primary output and `.d` must be present. A directory output such as
`.dSYM`, or unpacked `.dwo` files, stops capture safely.

Restore downloads and verifies every blob before writing anything, then writes
each file atomically with its executable bit. On Windows, a destination held
open by a running `.exe` or `.dll` is first renamed aside, which Windows
permits for mapped images, and swept later. Stdout and stderr are replayed as
for libraries.

## Incremental hybrid

`bellows run` no longer forces `CARGO_INCREMENTAL=0`. An invocation carrying
`-C incremental=` is looked up with the incremental argument removed from the
key.

- **Hit:** outputs are restored. rustc's incremental session is untouched and
  remains self-consistent.
- **Miss:** rustc runs *with* incremental. The result is published only when no
  session existed before the compile, meaning a from-scratch compile with no
  prior state. Edit-loop recompiles that reuse a session are never published,
  so incremental state from one checkout cannot reach another. (Implemented
  with a per-unit session directory; see below.)

## Proc-macro consumers (owner decision, 2026-09-30)

Direct `--extern` proc macros no longer bypass. Their `.so`/`.dll` digests are
already in the static key. Their expansions' tracked inputs (`include_*!`,
`env!`, `tracked_*`) are in dep-info, and the leak scan pins any expansion that
bakes a checkout path. The remaining risk is a proc macro that reads an
untracked file or environment variable and emits content, not a path, derived
from it. That risk already applies today to proc macros loaded through
re-exports (serde_derive through serde), and plain Cargo has the same
staleness. `explain` names the proc macros a restored result used.

## Failure modes guarded

| Requirement | Guard |
|---|---|
| 1. Link identity | Transitive rlibs, objects, scripts, resolved native libs and CRT objects from the real link command; linker binary digest and banner; link args in the key; `LIBRARY_PATH`/`LIB`/SDK environment in the key; LTO, strip and debuginfo flags in the key. |
| 2. Env and path capture | dep-info env-deps are verified. Normalized comparison only when the output contains no checkout path; otherwise literal comparison plus a root pin. |
| 3. All outputs, atomic | Discovery by unit stem and write time. The primary output and `.d` are required. All blobs are verified before the first write, and each file is written atomically with its mode. Windows running images are renamed aside. |
| 4. Relocatability | The miss compile is always remapped. The leak scan decides whether sharing is safe. |
| 5. Same test results | Measured on Manifold: uncached and cached pass/fail/ignored lists are diffed. |
| 6. Proc macros | Enabled with the owner's approval. Residual risk documented above. |

Anything the model cannot describe stays a visible bypass or capture fallback:
an unresolvable `-l`, a directory output, a shell-quoted (`@shell:`) response file, `-L all=`, profile
or plugin inputs, `save-temps`, a custom sysroot or target JSON, or
`target-cpu=native`.

## Changes made during implementation

Measurements on Manifold and the Windows desktop changed several details.
Each one keeps the guarantees above.

- **Linker identity by content, after linking.** Instead of a `--version`
  banner computed before every compile, the programs the link command
  actually uses are verified inputs: the driver, GCC's `collect2`, and the
  `ld`/`lld`/`mold` selected by `-fuse-ld=` and `-B` (or the driver's default),
  plus `link.exe` on MSVC. The static key keeps the arguments and `PATH`.
- **Per-unit incremental directories.** A crate's lib, test harness and
  binaries share Cargo's incremental directory and the `{crate}-` prefix, so a
  "session exists" check wrongly withheld 219 results in a fresh Manifold
  worktree. Each unit now gets `{crate}-bellows{extra}` beneath Cargo's
  directory, which makes "from scratch" exact and race-free.
- **Pin only the root that leaked.** For registry crates the "workspace" is the
  registry package directory, identical in every worktree. Pinning the
  target directory alongside it forced needless per-worktree rebuilds, for
  example `zstd-sys`.
- **`$CHECKOUT` root.** `flagship-game-bundle`'s nested Cargo build runs rustc
  from a subdirectory while inheriting the outer `OUT_DIR` and library paths.
  The directory where `bellows run/local` started is normalized, remapped,
  scanned and pinned like the other roots.
- **`$PROFILE` root.** `release` and Manifold's `test-fast` (which inherits it)
  give dependencies byte-identical rustc arguments; only
  `<target>/<profile>` differs. That directory is normalized and remapped to
  `/bellows/profile`, so both profiles share results.
- **Stable empty directories for procedural macros.** wit-bindgen's macro bakes
  its build script's (empty) `OUT_DIR` through `env!("DEBUG_OUTPUT_DIR")`. Its
  crate hash then differed per worktree and cascaded into every consumer.
  While compiling a proc-macro crate, a non-Cargo variable naming an empty
  directory inside the target directory is given a machine-wide empty
  directory instead. That is observably equivalent at compile time, the only
  time a macro runs.
- **Windows.** The canonical workspace is a verbatim `\\?\C:\…` path that never
  prefixed rustc's paths, so the protocol-5 workspace remap silently did
  nothing on Windows. Every spelling of each root is now remapped. Linker
  bookkeeping in `.pdb`, `.exp` and import `.lib` files (working directory,
  command line, module and output paths) does not pin a result; any other
  checkout path there does. Cargo omits `-C extra-filename` for MSVC
  executables and for `cdylib`/`dylib` packages, whose unhashed names are
  accepted.
- **Build-script runs** use the same candidate machinery through a launcher
  (see [local builds](local-builds.md#build-script-runs)). Nested Cargo target
  directories, marked by `CACHEDIR.TAG`, are excluded from stored `OUT_DIR`
  trees.
- **Descriptive `-Z` flags.** `-Zbuild-std` passes `-Zunstable-options` to every
  crate. That flag and a few others that only change what the arguments
  already describe are kept in the key instead of bypassing.
- **`BELLOWS_READ_ONLY=1`** restores verified results without publishing, for
  CI jobs that share a developer's service.
- **Unambiguous virtual paths and real-path diagnostics.** Cargo runs rustc
  from a path dependency's own directory, so one flat `/bellows/workspace`
  meant different directories in different compiles. rustc then could not
  reopen a restored dependency's source for a diagnostic snippet, which a
  Manifold trybuild test caught. A package directory is now remapped through
  its place in the session checkout (`/bellows/checkout/crates/x`) or Cargo's
  home (`/bellows/cargo-home/…`). rustc's messages are rewritten to the
  receiving checkout's real paths live and on replay, because remapping also
  applies to diagnostics.
- **clippy-driver linked units are compiled directly.** With
  `--print link-args`, clippy-driver writes corrupted `# env-dep:` names,
  which made Cargo rebuild on every clippy run.
- **Build-script mtimes.** A launcher keeps the real script's mtime; Cargo
  otherwise treats the script as older than its build-dependencies and
  rebuilds everything downstream.
- **Checkout root from the source tree (0.3.1).** The checkout root was the
  directory `bellows run` was launched from. A session launched in a crate
  subdirectory therefore cached a build-script output carrying absolute paths
  into its checkout (manifold-mod-macros' WIT table) without a pin, with
  inputs recorded at that checkout's absolute paths. Another worktree then
  validated those inputs against the first worktree's unchanged files and
  restored stale bindings. The checkout is now the git root (a worktree's
  `.git` file or a clone's `.git` directory) containing rustc's working
  directory. A cached input inside a *different* git checkout rejects the
  record, and such inputs are never stored.
- **Restored dep-info carries this checkout's `env!` values (0.3.2).** A result
  shared through a normalized path-valued `env!` dependency now records the
  receiving checkout's value in its `# env-dep:` line, escaped as rustc
  escapes it. The producer's value would make Cargo rebuild for any variable
  it does not set itself.
- **Pinned candidates name their checkout (0.3.17).** A pin is the digest of a
  path, which cannot say whether the path still exists, so pinned candidates
  of deleted worktrees stayed in local stores until the budget reached them
  (525 GiB in one store). The client now records, beside the candidate in
  its local store, the checkout that contains each pinned root
  (`pinned_checkouts`: the checkout's path and the root of its volume), and a
  local collection evicts the candidate once that checkout is gone from a
  volume that is still present (see the
  [runbook](operations.md#capacity-and-recovery)). The field is optional and
  outside the action key: earlier clients read such a candidate unchanged,
  and their own candidates are collected as before. It never leaves the
  machine.
- **Memoized digests.** Files Bellows writes are recorded in the digest memo
  under their post-write identity, so link validation in a fresh worktree does
  not rehash every restored rlib. `rustc -vV` is memoized per concrete compiler
  binary, never for a rustup proxy.
