# Native Windows validation

The Windows CI job runs on Windows Server 2022, x64 MSVC, Rust 1.92.0. A WSL
run exercises Linux filesystem and process behavior and does not replace this
native Windows check. Windows GNU, ARM64, network shares, and arbitrary declared
remote execution are not covered by this x64 qualification.

## Install for daily use

Use x64 Windows with local NTFS storage. Install Rust through rustup, Git,
PowerShell 7.2+, and Visual Studio Build Tools with **Desktop development with
C++** and a Windows SDK. A normal terminal is sufficient; Bellows itself does
not need administrator rights, WSL, a background service, or a token.

From the reviewed Bellows checkout:

```powershell
pwsh -NoProfile -File scripts/install.ps1
```

This installs the pinned Rust toolchain and the client into Cargo's bin directory
without changing your default toolchain or permanent environment settings.
Reopen your terminal if Rust was just installed so Cargo's bin directory is on
PATH. Then use the same commands as a Linux teammate:

```powershell
bellows cargo build --release
bellows cargo test --release
bellows explain --local --latest --summary
bellows stats --local --latest
```

Start with local caching. It needs no network access. Cargo's existing incremental
development builds continue working, with the same incremental policy as on
Linux: lookups hit, and only from-scratch results are published.
Do not delete your normal target directory to chase hits: Cargo's no-op is faster.

A shared cache is optional: it lets compatible teammates reuse compiler artifacts.
The service can run on Linux even when clients run on Windows. Native Linux and
Windows artifacts have different compiler/platform identities and are not
interchangeable. A configured team service uses `BELLOWS_SERVER` and
`BELLOWS_AUTH_TOKEN`, then `bellows run -- cargo build --release`; keep the token
out of source control. `-IncludeServer` installs the optional server binary.

Keep project roots reasonably short (for example `C:\src\manifold`) because
upstream Windows build tools may reject deeply nested working or output paths.
Long Unicode source dependencies are tested; that does not remove the operating
system or a third-party tool's path limit.

## Run qualification on your PC

Install Git, PowerShell 7.2 or newer, Rustup, and the Microsoft C++ build tools
with a Windows SDK. See the [Rustup MSVC prerequisites](https://rust-lang.github.io/rustup/installation/windows-msvc.html)
for the compiler/linker components. Use a normal PowerShell 7 window; the tests
use NTFS junctions and do not require enabling symlink privileges.

```powershell
git clone --branch main https://github.com/Manifold-Game/Bellows.git
cd Bellows
rustup toolchain install 1.92.0-x86_64-pc-windows-msvc --profile minimal --component rustfmt,clippy
pwsh -NoProfile -File .\scripts\windows-check.ps1
```

The script runs formatting, strict Clippy, the Rust tests, and a release build.
It then creates a separate workspace and cache with spaces in their paths and
checks executable output through cold/local cache hits, a source edit, cold/L1/
remote service use, service restart, and offline fallback. The functional suite
also checks environment and flag changes, output restoration, diagnostic
rotation/concurrent writers, lease reacquisition, and NTFS junction handling.
The native C-archive fixture uses MSVC on Windows and cc/ar on Linux; both check
that a changed native library is rebuilt rather than reused from the cache.

The script restores its temporary environment changes, stops its test daemon,
and prints the retained report directory on success or failure. `checks.log`,
`results.json`, per-phase JSONL diagnostics, and server logs are saved there.
To choose a report location, pass `-ReportDirectory C:\bellows-report`; use a
new directory for each run. CI uploads the same evidence as an artifact.
A failing native command stops qualification and returns failure.

The additional `scripts/windows-extended.ps1 -ReportDirectory <new-directory>`
suite exercises native archives, declared local/nested actions, authenticated
remote execution in a separate disposable service, compiler and execution
single-flight, bounded lease waits, corrupt-cache recovery, advisory analysis,
and non-ASCII paths. It requires the release binaries from `windows-check.ps1`
and `nightly-2026-01-15` for a real compiler-version invalidation comparison.
CI runs both suites and retains their command logs and machine-readable results.
The extended PowerShell suite also runs on Linux with the same assertions,
including compiler invalidation, lease timeouts, Unicode paths and remote execution.
Its remote executor is temporary; this does not enable execution on daily-use
services or qualify arbitrary host tools.

## Exercise Manifold on the actual machine

After this check passes, point your usual Manifold workflow at the built
`target\release\bellows.exe`. Use a disposable Manifold worktree and a dedicated
cache/target directory for a baseline/cold/warm comparison, preserving the same
compiler, flags, feature set, and source revision. Keep exact-diagnostic contract
lanes wrapper-free as required by Manifold. Compare test outcomes and runtime
behavior, not only cache-hit totals.

Final executables are restored with their `.pdb` and, for DLLs, `.dll.lib` and
`.dll.exp`. An executable that is still running (a test binary, for example)
is renamed aside before replacement, because Windows refuses to overwrite a
mapped image. Program databases record the linker's own working directory,
command line and module paths; those records are linker bookkeeping and do not
pin a result, but any other checkout path in a `.pdb` does.
Run `bellows.exe explain --local --latest --json` from the workspace to inspect
miss reasons. Prefer a local NTFS cache; this pass does not qualify SMB locking
or antivirus-specific behavior on your machine.

Windows paths and Rustup home discovery received fixes in this pass. The
compiler identity now includes a dependency-parser revision so compiler records
captured by the earlier backslash parser cannot be reused. Expect an initial
cold compiler build after updating; the protocol remains version 5.
