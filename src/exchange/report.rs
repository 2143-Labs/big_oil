//! Completeness (`verify`) and a human summary (`stats`).

use crate::exchange::HOUR;
use crate::exchange::client::get_hour;
use crate::exchange::store::ExchangeStore;
use crate::exchange::walk::sha256_hex;
use crate::render;
use anyhow::Result;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub hours: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Hours absent from the ledger inside the covered range.
    pub gaps: Vec<i64>,
    /// `(hour, markets the ledger claims, market rows actually present)`.
    pub short: Vec<(i64, i64, i64)>,
    /// Hours whose payload no longer hashes to the recorded digest.
    pub hash_mismatches: Vec<i64>,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.gaps.is_empty() && self.short.is_empty() && self.hash_mismatches.is_empty()
    }

    pub fn describe(&self) -> String {
        let mut out = String::new();
        for hour in &self.gaps {
            out.push_str(&format!("missing {hour} ({})\n", hour_label(*hour)));
        }
        for (hour, claimed, actual) in &self.short {
            out.push_str(&format!(
                "hour {hour} ({}): ledger says {claimed} markets, {actual} stored\n",
                hour_label(*hour)
            ));
        }
        for hour in &self.hash_mismatches {
            out.push_str(&format!(
                "hour {hour} ({}): payload hash changed\n",
                hour_label(*hour)
            ));
        }
        out
    }
}

/// Prove the stored hours are contiguous from `since` (or the first stored
/// hour) to the last, and that every ledger row still has its markets.
pub fn verify(
    store: &ExchangeStore,
    since: Option<i64>,
    recheck: bool,
    base: &str,
    user_agent: &str,
    pace: Duration,
) -> Result<VerifyReport> {
    let ledger = store.ledger()?;
    let mut report = VerifyReport {
        hours: ledger.len() as i64,
        first: ledger.first().map(|r| r.hour),
        last: ledger.last().map(|r| r.hour),
        ..Default::default()
    };
    let (Some(first), Some(last)) = (report.first, report.last) else {
        return Ok(report);
    };

    let start = since.map_or(first, |since| since.min(first));
    let have: HashSet<i64> = ledger.iter().map(|row| row.hour).collect();
    let mut hour = start;
    while hour <= last {
        if !have.contains(&hour) {
            report.gaps.push(hour);
        }
        hour += HOUR;
    }

    for row in &ledger {
        let actual = store.count_for_hour(row.hour)?;
        if actual != row.markets {
            report.short.push((row.hour, row.markets, actual));
        }
    }

    if recheck {
        for (i, row) in ledger.iter().enumerate() {
            let (_, raw) = get_hour(base, row.hour, user_agent)?;
            if sha256_hex(&raw) != row.sha256 {
                report.hash_mismatches.push(row.hour);
            }
            if i + 1 < ledger.len() {
                std::thread::sleep(pace);
            }
        }
    }
    Ok(report)
}

pub fn stats(store: &ExchangeStore, _now: i64) -> Result<String> {
    let ledger = store.ledger()?;
    if ledger.is_empty() {
        return Ok("no hours stored\n".into());
    }
    let first = ledger[0].hour;
    let last = ledger[ledger.len() - 1].hour;
    let rows: i64 = ledger.iter().map(|row| row.markets).sum();
    let mut out = format!(
        "hours {}, {} .. {} (newest {} ago)\n{rows} market rows\n\n",
        ledger.len(),
        hour_label(first),
        hour_label(last),
        render::age(last)
    );

    let leagues: Vec<Vec<String>> = store
        .leagues_at(last)?
        .into_iter()
        .map(|(league, markets)| vec![league, markets.to_string()])
        .collect();
    if !leagues.is_empty() {
        out.push_str(&render::table(&["league", "markets"], &leagues, 40));
        out.push('\n');
    }

    let mut prices = store.markets_at(last)?;
    prices.sort_by(|a, b| b.volume.cmp(&a.volume).then(a.item.cmp(&b.item)));
    prices.truncate(10);
    let busiest: Vec<Vec<String>> = prices
        .into_iter()
        .map(|row| {
            vec![
                row.league,
                row.item.rsplit('/').next().unwrap_or(&row.item).to_string(),
                render::num(row.chaos_mid),
                row.volume.to_string(),
            ]
        })
        .collect();
    if !busiest.is_empty() {
        out.push_str(&render::table(
            &["league", "item", "chaos", "volume"],
            &busiest,
            44,
        ));
    }
    Ok(out)
}

/// `2026-11-13 22:00 UTC`, from the RFC3339 rendering used elsewhere.
fn hour_label(hour: i64) -> String {
    let stamp = render::ts(hour);
    match stamp.split_once('T') {
        Some((date, rest)) => {
            let time: String = rest.chars().take(5).collect();
            format!("{date} {time} UTC")
        }
        None => stamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_labels_are_readable() {
        assert_eq!(hour_label(1794600000), "2026-11-13 20:00 UTC");
    }

    #[test]
    fn a_clean_report_has_an_empty_description() {
        let report = VerifyReport {
            hours: 2,
            first: Some(1794600000),
            last: Some(1794603600),
            ..Default::default()
        };
        assert!(report.is_clean());
        assert!(report.describe().is_empty());
    }

    #[test]
    fn problems_are_described_one_per_line() {
        let report = VerifyReport {
            hours: 2,
            first: Some(1794600000),
            last: Some(1794603600),
            gaps: vec![1794603600],
            short: vec![(1794600000, 5, 4)],
            hash_mismatches: vec![1794600000],
        };
        assert!(!report.is_clean());
        let text = report.describe();
        assert!(text.contains("missing 1794603600 (2026-11-13 21:00 UTC)"));
        assert!(
            text.contains(
                "hour 1794600000 (2026-11-13 20:00 UTC): ledger says 5 markets, 4 stored"
            )
        );
        assert!(text.contains("hour 1794600000 (2026-11-13 20:00 UTC): payload hash changed"));
    }
}
