//! Model the files a final link reads, from rustc's own `--print link-args`.
//!
//! rustc prints the exact linker command it runs, including every transitive
//! rlib, object, native library request and search directory. Bellows records
//! the files that command can read so a restored link is revalidated against
//! them. Library resolution deliberately over-approximates: every file a
//! `-l`/`name.lib` request could resolve to in any searched directory is an
//! input, so a library that changes or newly appears earlier in the search
//! order invalidates the link instead of silently changing its meaning.
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkCommand {
    pub env: Vec<(String, String)>,
    pub program: String,
    pub args: Vec<String>,
}

/// Parse the `Debug` rendering of the linker `Command` that rustc writes for
/// `--print link-args`: optional `NAME="value"` assignments, then quoted
/// program and argument strings with Rust escapes.
pub fn parse_link_command(text: &str) -> Result<LinkCommand> {
    let mut command = LinkCommand::default();
    let mut rest = text.trim();
    let mut program = None;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('"') {
            let (value, remainder) = read_quoted(stripped)?;
            if program.is_none() {
                program = Some(value);
            } else {
                command.args.push(value);
            }
            rest = remainder.trim_start();
            continue;
        }
        let word_end = rest.find([' ', '"']).unwrap_or(rest.len());
        let word = &rest[..word_end];
        if let Some(name) = word.strip_suffix('=')
            && rest[word_end..].starts_with('"')
            && program.is_none()
        {
            let (value, remainder) = read_quoted(&rest[word_end + 1..])?;
            command.env.push((name.to_owned(), value));
            rest = remainder.trim_start();
            continue;
        }
        // `cd "dir" &&`, `env -i`, `env -u NAME` prefixes do not change which
        // files the linker reads relative to rustc's own working directory
        // in a way Bellows can model; refuse rather than guess.
        bail!("unrecognized linker command syntax near {word:?}")
    }
    command.program = program.context("linker command has no program")?;
    Ok(command)
}

fn read_quoted(text: &str) -> Result<(String, &str)> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Ok((value, &text[index + 1..])),
            '\\' => {
                let (_, escaped) = chars.next().context("unterminated escape")?;
                match escaped {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    'r' => value.push('\r'),
                    '0' => value.push('\0'),
                    '\\' | '"' | '\'' => value.push(escaped),
                    'u' => {
                        let (_, open) = chars.next().context("bad unicode escape")?;
                        if open != '{' {
                            bail!("bad unicode escape")
                        }
                        let mut digits = String::new();
                        for (_, digit) in chars.by_ref() {
                            if digit == '}' {
                                break;
                            }
                            digits.push(digit);
                        }
                        value.push(
                            char::from_u32(u32::from_str_radix(&digits, 16)?)
                                .context("invalid unicode escape")?,
                        );
                    }
                    // Non-UTF-8 bytes cannot be reproduced as a path string.
                    other => bail!("unsupported escape \\{other} in linker command"),
                }
            }
            other => value.push(other),
        }
    }
    bail!("unterminated quoted string in linker command")
}

#[derive(Debug, Default)]
pub struct LinkInputs {
    pub files: BTreeSet<PathBuf>,
}

pub struct LinkContext<'a> {
    /// rustc's working directory; relative linker arguments resolve here.
    pub cwd: &'a Path,
    pub msvc: bool,
    /// rustc's sysroot, which holds self-contained linkers (`rust-lld`).
    pub sysroot: &'a Path,
    /// Files and directories that belong to this unit's own outputs.
    pub outputs: &'a dyn Fn(&Path) -> bool,
}

const C_RUNTIME_OBJECTS: &[&str] = &[
    "crt1.o",
    "Scrt1.o",
    "rcrt1.o",
    "crti.o",
    "crtn.o",
    "crtbegin.o",
    "crtbeginS.o",
    "crtbeginT.o",
    "crtend.o",
    "crtendS.o",
];

/// Collect every file the linker command can read, plus the linker programs.
pub fn link_inputs(command: &LinkCommand, context: &LinkContext<'_>) -> Result<LinkInputs> {
    let mut inputs = LinkInputs::default();
    let path_env = command
        .env
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let program = resolve_program(&command.program, &path_env, context.cwd)
        .or_else(|error| sysroot_tool(&command.program, context.sysroot).ok_or(error))
        .with_context(|| format!("resolve linker {}", command.program))?;
    inputs.files.insert(program.clone());

    let mut search = Vec::<PathBuf>::new();
    let mut libraries = Vec::<LibraryRequest>::new();
    let mut output = None::<PathBuf>;
    let mut fuse_ld = None::<String>;
    let mut driver_dirs = Vec::<PathBuf>::new();
    let mut previous = None::<&'static str>;
    for arg in &command.args {
        let pieces = split_linker_argument(arg);
        for piece in &pieces {
            if let Some(flag) = previous.take() {
                let value = context.cwd.join(piece);
                match flag {
                    "-L" => search.push(value),
                    "-l" => libraries.push(LibraryRequest::unix(piece)),
                    "-o" => output = Some(value),
                    _ => driver_dirs.push(value),
                }
                continue;
            }
            match piece.as_str() {
                "-L" => previous = Some("-L"),
                "-l" => previous = Some("-l"),
                "-o" => previous = Some("-o"),
                "-B" => previous = Some("-B"),
                _ => {}
            }
            if previous.is_some() {
                continue;
            }
            if let Some(dir) = piece.strip_prefix("-L") {
                search.push(context.cwd.join(dir));
            } else if let Some(name) = piece.strip_prefix("-l") {
                libraries.push(LibraryRequest::unix(name));
            } else if let Some(dir) = piece.strip_prefix("-B") {
                driver_dirs.push(context.cwd.join(dir));
            } else if let Some(name) = piece.strip_prefix("-fuse-ld=") {
                fuse_ld = Some(name.to_owned());
            }
            if context.msvc {
                let upper = piece.to_ascii_uppercase();
                if let Some(dir) = strip_msvc_option(piece, &upper, "LIBPATH:") {
                    search.push(context.cwd.join(dir));
                } else if let Some(name) = strip_msvc_option(piece, &upper, "DEFAULTLIB:") {
                    libraries.push(LibraryRequest::msvc(name));
                } else if upper.ends_with(".LIB") && !piece.starts_with(['/', '-']) {
                    let candidate = context.cwd.join(piece);
                    if !candidate.is_file() {
                        libraries.push(LibraryRequest::msvc(piece));
                    }
                }
                for option in ["OUT:", "PDB:", "IMPLIB:"] {
                    if let Some(path) = strip_msvc_option(piece, &upper, option) {
                        output = Some(context.cwd.join(path));
                    }
                }
            }
            for candidate in path_like_values(piece) {
                let path = context.cwd.join(candidate);
                if path.is_file() {
                    inputs.files.insert(path);
                }
            }
        }
    }

    let unix_driver = !context.msvc && is_c_driver(&program);
    if unix_driver {
        let dirs = driver_search_dirs(&program, &path_env)?;
        search.extend(dirs.iter().cloned());
        // The C driver adds its runtime objects implicitly.
        for object in C_RUNTIME_OBJECTS {
            for dir in &dirs {
                let path = dir.join(object);
                if path.is_file() {
                    inputs.files.insert(path);
                }
            }
        }
        for tool in linker_tools(&program, fuse_ld.as_deref(), &driver_dirs, &path_env)? {
            inputs.files.insert(tool);
        }
    }
    let library_env = if context.msvc { "LIB" } else { "LIBRARY_PATH" };
    if let Some(value) = command
        .env
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(library_env))
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var(library_env).ok())
    {
        search.extend(std::env::split_paths(&value));
    }
    if context.msvc {
        search.push(context.cwd.to_path_buf());
    }
    for library in &libraries {
        for dir in &search {
            for name in library.file_names() {
                let path = dir.join(&name);
                if path.is_file() {
                    inputs.files.insert(path);
                }
            }
        }
    }
    // GNU ld scripts (`libc.so` on glibc) name further archives and objects.
    let scripts = inputs.files.iter().cloned().collect::<Vec<_>>();
    for script in scripts {
        for referenced in linker_script_references(&script) {
            if referenced.is_file() {
                inputs.files.insert(referenced);
            }
        }
    }
    inputs
        .files
        .retain(|path| output.as_deref() != Some(path.as_path()) && !(context.outputs)(path));
    Ok(inputs)
}

struct LibraryRequest {
    name: String,
    verbatim: bool,
    msvc: bool,
}

impl LibraryRequest {
    fn unix(value: &str) -> Self {
        match value.strip_prefix(':') {
            Some(name) => Self {
                name: name.into(),
                verbatim: true,
                msvc: false,
            },
            None => Self {
                name: value.into(),
                verbatim: false,
                msvc: false,
            },
        }
    }

    fn msvc(value: &str) -> Self {
        Self {
            name: value.into(),
            verbatim: false,
            msvc: true,
        }
    }

    fn file_names(&self) -> Vec<String> {
        if self.verbatim {
            vec![self.name.clone()]
        } else if self.msvc {
            if self.name.to_ascii_lowercase().ends_with(".lib") {
                vec![self.name.clone()]
            } else {
                vec![format!("{}.lib", self.name), self.name.clone()]
            }
        } else {
            vec![
                format!("lib{}.so", self.name),
                format!("lib{}.a", self.name),
                format!("lib{}.dylib", self.name),
                format!("lib{}.tbd", self.name),
            ]
        }
    }
}

fn strip_msvc_option<'a>(piece: &'a str, upper: &str, option: &str) -> Option<&'a str> {
    for prefix in ['/', '-'] {
        if upper.starts_with(&format!("{prefix}{option}")) {
            return Some(&piece[option.len() + 1..]);
        }
    }
    None
}

/// `-Wl,a,b` passes several arguments; `--opt=value`, `/OPT:value` and
/// `-T` forms carry file names after a separator.
fn split_linker_argument(arg: &str) -> Vec<String> {
    match arg.strip_prefix("-Wl,") {
        Some(list) => list.split(',').map(str::to_owned).collect(),
        None => vec![arg.to_owned()],
    }
}

fn path_like_values(piece: &str) -> Vec<&str> {
    let mut values = vec![piece];
    if let Some((_, value)) = piece.split_once('=') {
        values.push(value);
    }
    // MSVC `/OPT:value`: the option name itself contains no separator.
    if let Some(rest) = piece.strip_prefix('/')
        && let Some((option, value)) = rest.split_once(':')
        && !option.is_empty()
        && !option.contains(['/', '\\'])
    {
        values.push(value);
    }
    for prefix in ["-T", "-Wl,-T"] {
        if let Some(value) = piece.strip_prefix(prefix)
            && !value.is_empty()
        {
            values.push(value);
        }
    }
    values.retain(|value| !value.is_empty() && !value.starts_with('-'));
    values
}

fn is_c_driver(program: &Path) -> bool {
    let name = program
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    ["cc", "gcc", "clang", "c++", "g++", "clang++"]
        .iter()
        .any(|driver| name == *driver || name.ends_with(&format!("-{driver}")))
        || name.starts_with("gcc-")
        || name.starts_with("clang-")
}

pub fn resolve_program(program: &str, path_env: &str, cwd: &Path) -> Result<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        let path = cwd.join(candidate);
        if path.is_file() {
            return Ok(path.canonicalize()?);
        }
        bail!("linker {program} does not exist")
    }
    for dir in std::env::split_paths(path_env) {
        for name in executable_names(program) {
            let path = dir.join(&name);
            if path.is_file() {
                return Ok(path.canonicalize()?);
            }
        }
    }
    bail!("linker {program} is not on PATH")
}

/// A self-contained tool rustc ships in its sysroot (`rust-lld` for wasm and
/// some MSVC targets, `wasm-component-ld`). rustc runs these from
/// `<sysroot>/lib/rustlib/<host>/bin` without them being on `PATH`.
pub fn sysroot_tool(program: &str, sysroot: &Path) -> Option<PathBuf> {
    if Path::new(program).components().count() > 1 {
        return None;
    }
    let hosts = std::fs::read_dir(sysroot.join("lib").join("rustlib")).ok()?;
    for host in hosts.flatten() {
        for dir in [
            host.path().join("bin"),
            host.path().join("bin").join("gcc-ld"),
        ] {
            for name in executable_names(program) {
                let path = dir.join(&name);
                if path.is_file() {
                    return path.canonicalize().ok();
                }
            }
        }
    }
    None
}

fn executable_names(program: &str) -> Vec<String> {
    if cfg!(windows) && !program.to_ascii_lowercase().ends_with(".exe") {
        vec![format!("{program}.exe"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    }
}

fn driver_search_dirs(program: &Path, path_env: &str) -> Result<Vec<PathBuf>> {
    let output = Command::new(program)
        .arg("-print-search-dirs")
        .env("PATH", path_env)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("run {} -print-search-dirs", program.display()))?;
    if !output.status.success() {
        bail!("{} -print-search-dirs failed", program.display())
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut dirs = Vec::new();
    for line in text.lines() {
        if let Some(list) = line.strip_prefix("libraries: ") {
            dirs.extend(std::env::split_paths(list.trim_start_matches('=')));
        }
    }
    let mut unique = BTreeSet::new();
    dirs.retain(|dir| {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        dir.is_dir() && unique.insert(key)
    });
    Ok(dirs)
}

/// The programs a C driver runs to link: `collect2` (GCC) and the linker
/// selected by `-fuse-ld=` (searched in `-B` directories, then PATH) or its
/// default `ld`.
fn linker_tools(
    driver: &Path,
    fuse_ld: Option<&str>,
    driver_dirs: &[PathBuf],
    path_env: &str,
) -> Result<Vec<PathBuf>> {
    let mut tools = Vec::new();
    let print = |name: &str| -> Option<PathBuf> {
        let output = Command::new(driver)
            .arg(format!("-print-prog-name={name}"))
            .env("PATH", path_env)
            .output()
            .ok()?;
        let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        let path = PathBuf::from(&value);
        if path.is_absolute() && path.is_file() {
            Some(path)
        } else {
            resolve_program(&value, path_env, Path::new("/")).ok()
        }
    };
    if let Some(collect2) = print("collect2") {
        tools.push(collect2);
    }
    let linker = match fuse_ld {
        Some(name) if Path::new(name).is_absolute() => Some(PathBuf::from(name)),
        Some(name) => driver_dirs
            .iter()
            .flat_map(|dir| [dir.join(format!("ld.{name}")), dir.join(name)])
            .find(|path| path.is_file())
            .or_else(|| resolve_program(&format!("ld.{name}"), path_env, Path::new("/")).ok())
            .or_else(|| resolve_program(name, path_env, Path::new("/")).ok()),
        None => driver_dirs
            .iter()
            .map(|dir| dir.join("ld"))
            .find(|path| path.is_file())
            .or_else(|| print("ld")),
    };
    match linker {
        Some(path) => tools.push(path.canonicalize().unwrap_or(path)),
        None => bail!("cannot resolve the linker the C driver will run"),
    }
    Ok(tools)
}

fn linker_script_references(path: &Path) -> Vec<PathBuf> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.len() > 64 * 1024 {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let head = text.trim_start();
    if !["/*", "GROUP", "INPUT", "OUTPUT_FORMAT", "SEARCH_DIR"]
        .iter()
        .any(|prefix| head.starts_with(prefix))
    {
        return Vec::new();
    }
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ','))
        .filter(|word| word.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
#[path = "../tests/src/link_test.rs"]
mod tests;
