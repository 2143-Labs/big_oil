//! Post-mapping triage for a dump tab.
//!
//! The loop this supports: dump unidentified drops, identify everything, set a
//! tab price, then pull out the items that are worth more than the tab price
//! (uniques, gems, currency — anything with a real market) and leave the rest
//! to decay. An unidentified unique is a blind spot: it cannot be valued until
//! it is identified, and it may be the best item in the tab.
//!
//! Fungibility here means "poe.ninja prices it", which in practice is uniques,
//! gems, currency, cards and maps. Rares never match, so they are counted as
//! illiquid rather than guessed at.

use std::collections::HashMap;

use serde::Serialize;

use crate::rates::Rates;
use crate::store::ItemRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No id yet: cannot be valued, and an unidentified unique can be anything.
    Unidentified,
    /// Worth clearly more than the tab price — pull it out and list it properly.
    Pull,
    /// Within the tab price's range; leaving it in the tab is fine.
    Fair,
    /// Priced above what the market pays: it will sit. Move it down or vendor.
    Stuck,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub tab: Option<String>,
    pub pos: Option<(i64, i64)>,
    pub frame_type: Option<i64>,
    pub value_chaos: Option<f64>,
    pub listed: Option<String>,
    pub listed_chaos: Option<f64>,
    /// value / tab price.
    pub times: Option<f64>,
    /// Supply for the matched market line — a rough "how fast will it sell".
    pub listings: Option<i64>,
    pub verdict: Verdict,
}

impl Row {
    pub fn pos_cell(&self) -> String {
        match self.pos {
            Some((x, y)) => format!("({x},{y})"),
            None => "-".into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.frame_type {
            Some(3) => "unique",
            Some(4) => "gem",
            Some(5) => "currency",
            Some(6) => "card",
            Some(2) => "rare",
            Some(1) => "magic",
            Some(0) => "normal",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub tab: Option<String>,
    /// Ignore items worth less than this many chaos.
    pub min_value_chaos: f64,
    /// Pull when value is at least this multiple of the tab price.
    pub pull_ratio: f64,
    /// Stuck when value is at most 1/`stuck_ratio` of the tab price.
    pub stuck_ratio: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tab: None,
            min_value_chaos: 5.0,
            pull_ratio: 1.5,
            stuck_ratio: 2.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// Tabs included in the report (one when `--tab` is used).
    pub tabs: Vec<String>,
    /// Tab price(s), when the tab is priced as a whole.
    pub tab_price: Option<String>,
    pub tab_price_chaos: Option<f64>,
    pub items: usize,
    pub unidentified: Vec<Row>,
    pub liquid: Vec<Row>,
    pub illiquid: usize,
    pub pull_value: f64,
}

impl Report {
    pub fn pulls(&self) -> impl Iterator<Item = &Row> {
        self.liquid.iter().filter(|r| r.verdict == Verdict::Pull)
    }

    pub fn stuck(&self) -> impl Iterator<Item = &Row> {
        self.liquid.iter().filter(|r| r.verdict == Verdict::Stuck)
    }

    pub fn summary(&self) -> String {
        format!(
            "{} to pull ({}c), {} fair, {} stuck, {} illiquid, {} unidentified",
            self.pulls().count(),
            self.pull_value.round() as i64,
            self.liquid
                .iter()
                .filter(|r| r.verdict == Verdict::Fair)
                .count(),
            self.stuck().count(),
            self.illiquid,
            self.unidentified.len()
        )
    }
}

pub fn analyze(rows: &[ItemRow], rates: &Rates, settings: &Settings) -> Report {
    let live: Vec<&ItemRow> = rows
        .iter()
        .filter(|r| r.delisted_at.is_none())
        .filter(|r| match &settings.tab {
            Some(tab) => r
                .stash_name
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&tab.to_ascii_lowercase()),
            None => true,
        })
        .collect();

    // Majority price per tab, same rule the dump report uses.
    let mut per_tab: HashMap<String, HashMap<String, (f64, String, usize)>> = HashMap::new();
    for row in &live {
        let (Some(amount), Some(currency)) = (row.price_amount, row.price_currency.as_deref())
        else {
            continue;
        };
        let tab = tab_name(row);
        let key = format!("{amount:.4}|{}", currency.to_ascii_lowercase());
        let entry = per_tab
            .entry(tab)
            .or_default()
            .entry(key)
            .or_insert_with(|| (amount, currency.to_ascii_lowercase(), 0));
        entry.2 += 1;
    }
    let mut dominant: HashMap<String, (f64, String, usize)> = HashMap::new();
    for (tab, prices) in &per_tab {
        let total: usize = prices.values().map(|(_, _, count)| count).sum();
        if let Some((_, (amount, currency, count))) = prices.iter().max_by_key(|(_, (_, _, c))| *c)
            && *count >= 3
            && (*count as f64) >= total as f64 * 0.6
        {
            dominant.insert(tab.clone(), (*amount, currency.clone(), *count));
        }
    }

    let mut report = Report {
        items: live.len(),
        ..Default::default()
    };
    let mut seen_tabs: Vec<String> = Vec::new();

    for row in &live {
        let tab = tab_name(row);
        if !seen_tabs.contains(&tab) {
            seen_tabs.push(tab.clone());
        }
        let listed_chaos = row
            .price_amount
            .zip(row.price_currency.as_deref())
            .and_then(|(a, c)| rates.chaos_per_unit(c).map(|r| a * r));

        let identified = row.identified.unwrap_or(true);
        let mut out = Row {
            id: row.id.clone(),
            name: row.display_name(),
            tab: row.stash_name.clone(),
            pos: row.x.zip(row.y),
            frame_type: row.frame_type,
            value_chaos: row.value_chaos,
            listed: row.price().map(|p| strip_kind(&p)),
            listed_chaos,
            times: None,
            listings: row.value_listings,
            verdict: Verdict::Fair,
        };

        if !identified {
            out.verdict = Verdict::Unidentified;
            report.unidentified.push(out);
            continue;
        }

        let Some(value) = row.value_chaos.filter(|v| *v > 0.0) else {
            report.illiquid += 1;
            continue;
        };
        if value < settings.min_value_chaos {
            report.illiquid += 1;
            continue;
        }

        // Compare against the price this item is actually listed at (in a
        // tab-priced tab that is the tab's price); the tab-wide figure is only
        // context for the header.
        out.times = listed_chaos.filter(|c| *c > 0.0).map(|c| value / c);
        out.verdict = match out.times {
            Some(times) if times >= settings.pull_ratio => {
                report.pull_value += value;
                Verdict::Pull
            }
            Some(times) if times <= 1.0 / settings.stuck_ratio => Verdict::Stuck,
            _ => Verdict::Fair,
        };
        report.liquid.push(out);
    }

    report.unidentified.sort_by(|a, b| {
        b.frame_type
            .unwrap_or(0)
            .cmp(&a.frame_type.unwrap_or(0))
            .then_with(|| a.name.cmp(&b.name))
    });
    report.liquid.sort_by(|a, b| {
        b.value_chaos
            .unwrap_or(0.0)
            .total_cmp(&a.value_chaos.unwrap_or(0.0))
    });
    report.liquid.sort_by(|a, b| {
        let rank = |v: Verdict| match v {
            Verdict::Pull => 0,
            Verdict::Fair => 1,
            Verdict::Stuck => 2,
            Verdict::Unidentified => 3,
        };
        rank(a.verdict).cmp(&rank(b.verdict)).then_with(|| {
            b.value_chaos
                .unwrap_or(0.0)
                .total_cmp(&a.value_chaos.unwrap_or(0.0))
        })
    });

    report.tabs = seen_tabs;
    if let Some((amount, currency, _)) = dominant.values().max_by_key(|(_, _, c)| *c) {
        report.tab_price = Some(format_price(*amount, currency));
        report.tab_price_chaos = rates.chaos_per_unit(currency).map(|r| amount * r);
    }
    report
}

fn tab_name(row: &ItemRow) -> String {
    row.stash_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "<unnamed tab>".to_string())
}

fn strip_kind(price: &str) -> String {
    let mut parts = price.split_whitespace();
    match parts.next() {
        Some("b/o" | "gb/o" | "c/o") => parts.collect::<Vec<_>>().join(" "),
        _ => price.to_string(),
    }
}

fn format_price(amount: f64, currency: &str) -> String {
    if (amount - amount.round()).abs() < f64::EPSILON {
        format!("{} {}", amount.round() as i64, currency)
    } else {
        format!("{amount:.1} {}", currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ninja::PriceLine;

    fn rates(divine: f64) -> Rates {
        Rates::from_lines(&[PriceLine {
            league: "L".into(),
            category: "Currency".into(),
            name: Some("Divine Orb".into()),
            base_type: None,
            variant: None,
            links: None,
            corrupted: None,
            gem_level: None,
            gem_quality: None,
            map_tier: None,
            chaos: divine,
            divine: None,
            listing_count: None,
            details_id: None,
            fetched_at: 0,
        }])
    }

    fn item(name: &str, frame: i64, amount: Option<f64>, value: Option<f64>) -> ItemRow {
        ItemRow {
            id: name.to_ascii_lowercase(),
            name: Some(name.into()),
            type_line: Some("Leather Belt".into()),
            stash_name: Some("Dump".into()),
            frame_type: Some(frame),
            price_amount: amount,
            price_currency: amount.map(|_| "divine".to_string()),
            price_type: amount.map(|_| "b/o".to_string()),
            value_chaos: value,
            source: "own".into(),
            identified: Some(true),
            x: Some(3),
            y: Some(7),
            ..Default::default()
        }
    }

    #[test]
    fn pulls_items_worth_more_than_the_tab_price() {
        // 1 divine tab (=400c): a 4000c unique and a 100c rare-ish unique.
        let rows = vec![
            item("Headhunter", 3, Some(1.0), Some(4000.0)),
            item("Cheap", 3, Some(1.0), Some(100.0)),
        ];
        let report = analyze(&rows, &rates(400.0), &Settings::default());
        assert_eq!(report.pulls().count(), 1);
        assert_eq!(
            report.pulls().next().unwrap().name,
            "Headhunter (Leather Belt)"
        );
        assert_eq!(report.pull_value, 4000.0);
        // 100c vs a 400c tab price: a quarter of the price, so stuck.
        assert_eq!(report.stuck().count(), 1);
    }

    #[test]
    fn unidentified_items_are_listed_first_with_positions() {
        let mut hidden = item("", 3, Some(1.0), None);
        hidden.identified = Some(false);
        hidden.type_line = Some("Leather Belt".into());
        hidden.name = None;
        let rows = vec![item("Mageblood", 3, Some(1.0), Some(90_000.0)), hidden];

        let report = analyze(&rows, &rates(400.0), &Settings::default());
        assert_eq!(report.unidentified.len(), 1);
        let row = &report.unidentified[0];
        assert_eq!(row.pos_cell(), "(3,7)");
        assert_eq!(row.kind(), "unique");
        assert_eq!(
            report.pulls().count(),
            1,
            "the valuable one is still a pull"
        );
    }

    #[test]
    fn rares_are_illiquid_not_guessed_at() {
        let rows = vec![
            item("Dragon Veil", 2, Some(1.0), None),
            item("Headhunter", 3, Some(1.0), Some(4000.0)),
        ];
        let report = analyze(&rows, &rates(400.0), &Settings::default());
        assert_eq!(report.illiquid, 1);
        assert_eq!(report.items, 2);
    }

    #[test]
    fn cheap_items_are_ignored() {
        let rows = vec![item("Trinket", 3, Some(1.0), Some(2.0))];
        let report = analyze(&rows, &rates(400.0), &Settings::default());
        assert!(report.liquid.is_empty(), "under min_value_chaos");
        assert_eq!(report.illiquid, 1);
    }

    #[test]
    fn fair_items_are_not_pulled() {
        // 400c item in a 400c tab: exactly the tab price.
        let rows = vec![item("Fair", 3, Some(1.0), Some(500.0))];
        let report = analyze(&rows, &rates(400.0), &Settings::default());
        assert_eq!(report.pulls().count(), 0);
        assert_eq!(report.liquid[0].verdict, Verdict::Fair);
        assert_eq!(report.liquid[0].times, Some(1.25));
    }

    #[test]
    fn pulls_are_ranked_before_stuck() {
        let rows = vec![
            item("Stuck", 3, Some(1.0), Some(50.0)),
            item("Pull", 3, Some(1.0), Some(4000.0)),
            item("Fair", 3, Some(1.0), Some(500.0)),
        ];
        let report = analyze(&rows, &rates(400.0), &Settings::default());
        let order: Vec<&str> = report
            .liquid
            .iter()
            .map(|r| match r.verdict {
                Verdict::Pull => "pull",
                Verdict::Fair => "fair",
                Verdict::Stuck => "stuck",
                Verdict::Unidentified => "id",
            })
            .collect();
        assert_eq!(order, vec!["pull", "fair", "stuck"]);
    }
}
