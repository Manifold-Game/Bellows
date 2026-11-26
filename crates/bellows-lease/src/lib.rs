//! A lease scheduler for shared build machines.
//!
//! Each machine runs one leased job at a time. Requests queue in order of
//! priority, then merge-queue position, then arrival, and are granted as the
//! machine frees. A lease lives exactly as long as the requesting client's
//! connection: when the client exits, crashes or is killed, the daemon sees
//! the socket close, kills the job's process group, and grants the next
//! request. Nothing depends on a client remembering to release.
//!
//! A *session* lease (`lease hold`) covers several commands: it hands its
//! children a token, and a `lease run` carrying that token runs at once
//! inside the session instead of queueing behind it.
//!
//! While a CI job runs on the laptop (`ci-start` … `ci-stop`), laptop grants
//! still happen, with a reduced job count, so CI never waits behind a long
//! local batch.
pub mod protocol;
pub mod scheduler;
#[cfg(unix)]
pub mod server;

use anyhow::{Result, bail};

/// Parses an estimate like `20m`, `1h`, `90s` or a bare number of minutes.
pub fn parse_duration_secs(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(split) => text.split_at(split),
        None => (text, "m"),
    };
    let value: u64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {text:?}: expected e.g. 20m, 1h or 90s"))?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => bail!("invalid duration {text:?}: unit must be s, m or h"),
    };
    Ok(value.saturating_mul(scale))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Default socket: `$XDG_RUNTIME_DIR/bellows-lease.sock`, else `/tmp`.
pub fn default_socket() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("bellows-lease.sock")
}

/// Default state directory for the audit log and the admin token.
pub fn default_state_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("bellows-lease")
}

#[cfg(test)]
#[path = "../tests/src/lib_test.rs"]
mod tests;
