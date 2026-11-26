//! Member names of `ar` archives (rlibs, `.a`, and MSVC `.lib`).
//!
//! Supports the GNU/System V variant (used by rustc and by COFF import
//! libraries) and the BSD variant. Symbol tables are skipped.
use anyhow::{Context, Result, bail};

const MAGIC: &[u8] = b"!<arch>\n";
const HEADER: usize = 60;

pub fn member_names(bytes: &[u8]) -> Result<Vec<String>> {
    if !bytes.starts_with(MAGIC) {
        bail!("not an ar archive")
    }
    let mut names = Vec::new();
    let mut long_names: &[u8] = &[];
    let mut offset = MAGIC.len();
    while offset + HEADER <= bytes.len() {
        let header = &bytes[offset..offset + HEADER];
        if &header[58..60] != b"`\n" {
            bail!("corrupt ar member header at {offset}")
        }
        let raw_name = std::str::from_utf8(&header[..16])?.trim_end();
        let size: usize = std::str::from_utf8(&header[48..58])?
            .trim()
            .parse()
            .context("ar member size")?;
        let data_start = offset + HEADER;
        let data_end = data_start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .context("ar member exceeds archive")?;
        let data = &bytes[data_start..data_end];
        if raw_name == "//" {
            long_names = data;
        } else if matches!(raw_name, "/" | "/SYM64/" | "__.SYMDEF" | "__.SYMDEF SORTED") {
        } else if let Some(length) = raw_name.strip_prefix("#1/") {
            let length: usize = length.parse().context("BSD name length")?;
            let name = data.get(..length).context("BSD name exceeds member")?;
            let name = String::from_utf8_lossy(name)
                .trim_end_matches('\0')
                .to_owned();
            if !name.starts_with("__.SYMDEF") {
                names.push(name);
            }
        } else if let Some(index) = raw_name.strip_prefix('/') {
            let index: usize = index.parse().context("long name index")?;
            let rest = long_names.get(index..).context("long name out of range")?;
            let end = rest
                .iter()
                .position(|b| *b == b'\n' || *b == 0)
                .unwrap_or(rest.len());
            names.push(
                String::from_utf8_lossy(&rest[..end])
                    .trim_end_matches('/')
                    .to_owned(),
            );
        } else {
            names.push(raw_name.trim_end_matches('/').to_owned());
        }
        offset = data_end + (size % 2);
    }
    Ok(names)
}

#[cfg(test)]
#[path = "../tests/src/archive_test.rs"]
mod tests;
