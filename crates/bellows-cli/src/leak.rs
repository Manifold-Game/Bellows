//! Detect checkout paths that a compile baked into its outputs.
//!
//! Every miss compiles with `--remap-path-prefix` for the workspace and target
//! roots, so a remapped build of path-independent code contains neither root.
//! Any remaining occurrence came from the program itself, such as
//! `env!("CARGO_MANIFEST_DIR")` used as a value or a proc macro that
//! read a path. Such outputs are only valid in the checkout that produced them.
use memchr::memmem::Finder;

/// Minimum root length that can be searched without matching unrelated bytes.
/// Shorter roots, such as `/w`, are treated as always present.
const MIN_NEEDLE: usize = 6;

pub struct Scanner {
    finders: Vec<Finder<'static>>,
    always: bool,
    case_insensitive: bool,
}

impl Scanner {
    pub fn new(roots: &[String]) -> Self {
        let case_insensitive = cfg!(windows);
        let mut needles = std::collections::BTreeSet::new();
        let mut always = false;
        for root in roots {
            let root = root.trim_end_matches(['/', '\\']);
            if root.len() < MIN_NEEDLE {
                always = true;
                continue;
            }
            let mut spellings = vec![root.to_owned()];
            if cfg!(windows) {
                spellings.push(root.replace('\\', "/"));
                spellings.push(root.replace('\\', "\\\\"));
            }
            for spelling in spellings {
                let spelling = if case_insensitive {
                    spelling.to_ascii_lowercase()
                } else {
                    spelling
                };
                if cfg!(windows) {
                    // Wide strings (PE resources, some PDB records).
                    let wide = spelling
                        .encode_utf16()
                        .flat_map(u16::to_le_bytes)
                        .collect::<Vec<_>>();
                    needles.insert(wide);
                }
                needles.insert(spelling.into_bytes());
            }
        }
        Self {
            finders: needles
                .into_iter()
                .map(|needle| Finder::new(&needle).into_owned())
                .collect(),
            always,
            case_insensitive,
        }
    }

    /// MSVC program databases (and the linker's `.exp`/import `.lib`
    /// companions) record the linker's own bookkeeping: its
    /// working directory, command line (`/OUT:…`), the object and library
    /// modules it read, and output paths. None of it affects the program or
    /// symbolization, which uses rustc's remapped source paths. Any other
    /// string naming a checkout root, such as an unremapped source file, is
    /// still a leak.
    pub fn leaks_in_program_database(&self, bytes: &[u8]) -> bool {
        if self.always {
            return true;
        }
        let haystack = if self.case_insensitive {
            bytes.to_ascii_lowercase()
        } else {
            bytes.to_vec()
        };
        for finder in &self.finders {
            let needle = finder.needle();
            if needle.contains(&0) {
                // A wide (UTF-16) spelling: not linker bookkeeping.
                if finder.find(&haystack).is_some() {
                    return true;
                }
                continue;
            }
            for position in finder.find_iter(&haystack) {
                let printable = |b: &u8| (0x20..0x7f).contains(b);
                let start = haystack[..position]
                    .iter()
                    .rposition(|b| !printable(b))
                    .map_or(0, |index| index + 1);
                let end = haystack[position..]
                    .iter()
                    .position(|b| !printable(b))
                    .map_or(haystack.len(), |index| position + index);
                let text = String::from_utf8_lossy(&haystack[start..end]);
                if !is_linker_bookkeeping(&text, needle) {
                    return true;
                }
            }
        }
        false
    }

    pub fn leaks(&self, bytes: &[u8]) -> bool {
        if self.always {
            return true;
        }
        if self.case_insensitive {
            let lowered = bytes.to_ascii_lowercase();
            return self.finders.iter().any(|f| f.find(&lowered).is_some());
        }
        self.finders
            .iter()
            .any(|finder| finder.find(bytes).is_some())
    }
}

fn is_linker_bookkeeping(text: &str, root: &[u8]) -> bool {
    const MODULES: &[&str] = &[
        ".o", ".obj", ".rlib", ".lib", ".a", ".res", ".exe", ".dll", ".pdb", ".exp",
    ];
    let lower = text.to_ascii_lowercase();
    let root = String::from_utf8_lossy(root);
    lower.trim_end_matches(['\\', '/']) == root.trim_end_matches(['\\', '/'])
        || lower.contains("/out:")
        || MODULES.iter().any(|extension| lower.ends_with(extension))
}

#[cfg(test)]
#[path = "../tests/src/leak_test.rs"]
mod tests;
