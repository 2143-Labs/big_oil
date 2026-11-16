//! The cursor: every rule about which hour to ask for and when to stop lives
//! here.

use crate::exchange::HOUR;
use crate::exchange::client::get_hour;
use crate::exchange::store::ExchangeStore;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// The hour `now` falls in: the newest hour that can already be complete. The
/// hour in progress is never asked for — it answers with no markets and the
/// same id back.
pub fn current_hour(now: i64) -> i64 {
    now - now.rem_euclid(HOUR)
}

#[derive(Debug)]
pub struct Walked {
    pub stored: Vec<i64>,
    /// The first hour not stored: the hour in progress, or where an error left
    /// off.
    pub cursor: i64,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Store every missing hour from `from` up to the live edge.
pub fn walk(
    store: &mut ExchangeStore,
    base: &str,
    user_agent: &str,
    from: i64,
    pace: Duration,
    raw_dir: &Path,
    now: i64,
) -> Result<Walked> {
    std::fs::create_dir_all(raw_dir)
        .with_context(|| format!("creating raw archive directory {}", raw_dir.display()))?;
    let head = current_hour(now);
    let mut hour = from;
    let mut stored = Vec::new();
    while hour < head {
        if store.has_hour(hour)? {
            hour += HOUR;
            continue;
        }
        let (page, raw) = get_hour(base, hour, user_agent)?;
        // The documented end-of-stream signal for a hour that is not available.
        if page.markets.is_empty() && page.next_change_id == hour {
            tracing::info!(hour, "reached the end of the stream");
            break;
        }
        // A silent jump would skip hours with nobody noticing.
        if page.next_change_id != hour + HOUR {
            bail!(
                "currency-exchange {hour} returned next_change_id {} (expected {})",
                page.next_change_id,
                hour + HOUR
            );
        }
        let skipped = page.markets.iter().filter(|m| m.sides().is_none()).count();
        if skipped > 0 {
            tracing::warn!(hour, skipped, "skipped malformed markets");
        }
        let hash = sha256_hex(&raw);
        write_raw(raw_dir, hour, &raw)?;
        let written = store.record_hour(hour, &page, raw.len() as i64, &hash)?;
        tracing::info!(
            hour,
            markets = page.markets.len(),
            stored = written,
            "stored exchange hour"
        );
        stored.push(hour);
        hour = page.next_change_id;
        if hour < head {
            std::thread::sleep(pace);
        }
    }
    Ok(Walked {
        stored,
        cursor: hour,
    })
}

/// Archive the payload beside the database: GGG documents that old history may
/// be removed, so the raw copy is the fallback record. Written to a temporary
/// name and renamed, so a crash never leaves a truncated hour that looks
/// complete.
fn write_raw(dir: &Path, hour: i64, raw: &[u8]) -> Result<()> {
    let path = dir.join(format!("{hour}.json.gz"));
    let tmp = dir.join(format!("{hour}.json.gz.tmp"));
    let mut encoder = GzEncoder::new(
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?,
        Compression::default(),
    );
    encoder
        .write_all(raw)
        .with_context(|| format!("writing {}", tmp.display()))?;
    encoder.finish()?.sync_all()?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_hour_floors_to_the_hour() {
        assert_eq!(current_hour(1794808800), 1794808800);
        assert_eq!(current_hour(1794808800 + 3599), 1794808800);
        assert_eq!(current_hour(1794808800 + 3600), 1794808800 + 3600);
    }

    #[test]
    fn sha_is_hex_and_stable() {
        let h = sha256_hex(b"abc");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
