//! Post-mapping triage for one dump tab.
//!
//! The loop this supports: dump unidentified drops, identify everything, price
//! the tab by name, then pull out the items worth clearly more than they are
//! listed at (uniques, gems, currency — anything with a real market) and leave
//! the rest. An unidentified unique is a blind spot: it cannot be valued until
//! it is identified, and it may be the best item in the tab.
//!
//! Fungibility here means "poe.ninja prices it", which in practice is uniques,
//! gems, currency, cards and maps. Rares never match, so they are counted as
//! illiquid rather than guessed at.

use serde::Serialize;

use crate::listing::{self, Rule};
use crate::price;
use crate::rates::Rates;
use crate::store::ItemRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No id yet: cannot be valued, and an unidentified unique can be anything.
    Unidentified,
    /// Worth clearly more than its listed price — pull it out and list it.
    Pull,
    /// Within range of its price; leaving it in the tab is fine.
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
    /// value / listed price.
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
    pub rule: Rule,
    /// Stuck when value is at most 1/`stuck_ratio` of the listed price.
    pub stuck_ratio: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rule: Rule::default(),
            stuck_ratio: 2.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub tab: String,
    pub tab_index: Option<i64>,
    /// The price in the tab name, when it is one.
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

/// Triage the live rows of one tab (see [`listing::resolve_one`]).
pub fn analyze(rows: &[&ItemRow], rates: &Rates, settings: &Settings) -> Report {
    let live: Vec<&ItemRow> = rows
        .iter()
        .copied()
        .filter(|r| r.delisted_at.is_none())
        .collect();

    let mut report = Report {
        items: live.len(),
        ..Default::default()
    };
    if let Some(first) = live.first() {
        report.tab = listing::tab_label(first);
        report.tab_index = first.tab_index;
        if let Some(p) = first.stash_name.as_deref().and_then(price::parse_note) {
            report.tab_price = Some(listing::format_price(p.amount, &p.currency));
            report.tab_price_chaos = rates.chaos_per_unit(&p.currency).map(|r| p.amount * r);
        }
    }

    for row in live {
        let listed_chaos = listing::listed_chaos(row, rates);
        let mut out = Row {
            id: row.id.clone(),
            name: row.display_name(),
            tab: row.stash_name.clone(),
            pos: row.x.zip(row.y),
            frame_type: row.frame_type,
            value_chaos: row.value_chaos,
            listed: listing::listed_label(row),
            listed_chaos,
            times: None,
            listings: row.value_listings,
            verdict: Verdict::Fair,
        };

        if !row.identified.unwrap_or(true) {
            out.verdict = Verdict::Unidentified;
            report.unidentified.push(out);
            continue;
        }

        let Some(value) = row
            .value_chaos
            .filter(|v| *v > 0.0 && *v >= settings.rule.min_value_chaos)
        else {
            report.illiquid += 1;
            continue;
        };

        out.times = listed_chaos.map(|listed| value / listed);
        out.verdict = if listing::underpriced(row, rates, &settings.rule).is_some() {
            report.pull_value += value * listing::stack(row);
            Verdict::Pull
        } else if listed_chaos.is_some_and(|listed| value <= listed / settings.stuck_ratio) {
            Verdict::Stuck
        } else {
            Verdict::Fair
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
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ninja::PriceLine;

    fn rates(divine: f64) -> Rates {
        Rates::from_lines(&[PriceLine {
            category: "Currency".into(),
            name: Some("Divine Orb".into()),
            chaos: divine,
            ..Default::default()
        }])
    }

    fn item(name: &str, frame: i64, amount: Option<f64>, value: Option<f64>) -> ItemRow {
        ItemRow {
            id: name.to_ascii_lowercase(),
            stash_id: "dump".into(),
            name: Some(name.into()),
            type_line: Some("Leather Belt".into()),
            stash_name: Some("Dump".into()),
            tab_index: Some(2),
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

    fn run(rows: &[ItemRow]) -> Report {
        let refs: Vec<&ItemRow> = rows.iter().collect();
        analyze(&refs, &rates(400.0), &Settings::default())
    }

    #[test]
    fn pulls_items_worth_more_than_their_price() {
        // 1 divine (=400c): a 4000c unique and a 100c one.
        let report = run(&[
            item("Headhunter", 3, Some(1.0), Some(4000.0)),
            item("Cheap", 3, Some(1.0), Some(100.0)),
        ]);
        assert_eq!(report.tab, "Dump");
        assert_eq!(report.tab_index, Some(2));
        assert_eq!(report.pulls().count(), 1);
        assert_eq!(
            report.pulls().next().unwrap().name,
            "Headhunter (Leather Belt)"
        );
        assert_eq!(report.pull_value, 4000.0);
        // 100c against a 400c price: a quarter of it, so stuck.
        assert_eq!(report.stuck().count(), 1);
    }

    #[test]
    fn the_tab_price_comes_from_the_tab_name() {
        let mut row = item("Headhunter", 3, Some(1.0), Some(4000.0));
        row.stash_name = Some("~price 1 divine".into());
        let report = run(&[row]);
        assert_eq!(report.tab, "~price 1 divine");
        assert_eq!(report.tab_price.as_deref(), Some("1 divine"));
        assert_eq!(report.tab_price_chaos, Some(400.0));

        assert_eq!(run(&[item("x", 3, None, None)]).tab_price, None);
    }

    #[test]
    fn unidentified_items_are_listed_first_with_positions() {
        let mut hidden = item("", 3, Some(1.0), None);
        hidden.identified = Some(false);
        hidden.name = None;
        let report = run(&[item("Mageblood", 3, Some(1.0), Some(90_000.0)), hidden]);
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
        let report = run(&[
            item("Dragon Veil", 2, Some(1.0), None),
            item("Headhunter", 3, Some(1.0), Some(4000.0)),
        ]);
        assert_eq!(report.illiquid, 1);
        assert_eq!(report.items, 2);
    }

    #[test]
    fn cheap_items_are_ignored() {
        let report = run(&[item("Trinket", 3, Some(1.0), Some(2.0))]);
        assert!(report.liquid.is_empty(), "under min_value_chaos");
        assert_eq!(report.illiquid, 1);
    }

    #[test]
    fn fair_items_are_not_pulled() {
        // 500c against a 400c price.
        let report = run(&[item("Fair", 3, Some(1.0), Some(500.0))]);
        assert_eq!(report.pulls().count(), 0);
        assert_eq!(report.liquid[0].verdict, Verdict::Fair);
        assert_eq!(report.liquid[0].times, Some(1.25));
    }

    #[test]
    fn pulls_are_ranked_before_stuck() {
        let report = run(&[
            item("Stuck", 3, Some(1.0), Some(50.0)),
            item("Pull", 3, Some(1.0), Some(4000.0)),
            item("Fair", 3, Some(1.0), Some(500.0)),
        ]);
        let order: Vec<Verdict> = report.liquid.iter().map(|r| r.verdict).collect();
        assert_eq!(order, vec![Verdict::Pull, Verdict::Fair, Verdict::Stuck]);
    }
}
