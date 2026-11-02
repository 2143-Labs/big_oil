//! Own-tab report: what is underpriced, per tab, and how long each tab has sat
//! at its price.
//!
//! Dump tabs are priced by name (`~price 1 divine`) and re-priced by renaming
//! them, so the report leads with one row per tab: its price, how long it has
//! been at that price, its market value and when it was last scanned. Below
//! that it lists every item worth clearly more than it is listed at (see
//! [`crate::listing::Rule`]): those sell the moment someone sees them.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::listing::{self, Rule, TabSelector};
use crate::price;
use crate::rates::Rates;
use crate::store::ItemRow;

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: String,
    pub name: String,
    pub tab: String,
    pub listed: String,
    /// Per unit.
    pub listed_chaos: f64,
    /// Per unit.
    pub value_chaos: f64,
    /// value / listed.
    pub times: f64,
    pub stack: f64,
    /// (value − listed) × stack.
    pub loss_chaos: f64,
    /// Time at the current price.
    pub age_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TabSummary {
    pub stash_id: String,
    pub name: String,
    pub index: Option<i64>,
    pub items: usize,
    /// The price in the tab name, when it is one.
    pub price: Option<String>,
    /// How long the tab has been at that price.
    pub at_price_secs: Option<i64>,
    /// Market value of the tab's live items (whole stacks).
    pub value_chaos: f64,
    pub underpriced: usize,
    /// Time since the tab was last scanned.
    pub scanned_secs: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub tab: Option<TabSelector>,
    pub rule: Rule,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub tabs: Vec<TabSummary>,
    pub underpriced: Vec<Finding>,
    /// Chaos missed if the underpriced items sell as listed.
    pub value_at_risk: f64,
    /// Notes whose currency has no rate, by currency.
    pub unconvertible: BTreeMap<String, usize>,
}

impl Report {
    pub fn is_quiet(&self) -> bool {
        self.underpriced.is_empty()
    }

    /// One-line summary, used for logs and desktop notifications.
    pub fn summary(&self) -> String {
        let mut out = if self.underpriced.is_empty() {
            "nothing underpriced".to_string()
        } else {
            format!(
                "{} underpriced (~{}c at risk)",
                self.underpriced.len(),
                self.value_at_risk.round() as i64
            )
        };
        if !self.unconvertible.is_empty() {
            let notes: usize = self.unconvertible.values().sum();
            let names: Vec<&str> = self.unconvertible.keys().map(String::as_str).collect();
            out.push_str(&format!(
                ", {notes} notes in unknown currencies ({})",
                names.join(", ")
            ));
        }
        out
    }
}

pub fn analyze(rows: &[ItemRow], rates: &Rates, now: i64, settings: &Settings) -> Report {
    let mut report = Report::default();
    let mut tabs: HashMap<&str, TabSummary> = HashMap::new();

    let live = rows
        .iter()
        .filter(|r| r.delisted_at.is_none())
        .filter(|r| settings.tab.as_ref().is_none_or(|sel| sel.matches(r)));
    for row in live {
        let tab = tabs.entry(row.stash_id.as_str()).or_insert_with(|| {
            let price = row
                .stash_name
                .as_deref()
                .and_then(price::parse_note)
                .map(|p| listing::format_price(p.amount, &p.currency));
            TabSummary {
                stash_id: row.stash_id.clone(),
                name: listing::tab_label(row),
                index: row.tab_index,
                items: 0,
                at_price_secs: price
                    .as_ref()
                    .and(row.tab_price_since)
                    .map(|since| now - since),
                price,
                value_chaos: 0.0,
                underpriced: 0,
                scanned_secs: row.stash_last_seen.map(|seen| now - seen),
            }
        });
        tab.items += 1;

        let stack = listing::stack(row);
        if let Some(value) = row.value_chaos.filter(|v| *v > 0.0) {
            tab.value_chaos += value * stack;
        }
        if row.price_amount.is_some() && listing::listed_chaos(row, rates).is_none() {
            let currency = row.price_currency.as_deref().unwrap_or("?");
            *report
                .unconvertible
                .entry(currency.to_ascii_lowercase())
                .or_default() += 1;
        }

        if let Some(gap) = listing::underpriced(row, rates, &settings.rule) {
            let loss = (gap.value - gap.listed) * stack;
            tab.underpriced += 1;
            report.value_at_risk += loss;
            report.underpriced.push(Finding {
                id: row.id.clone(),
                name: row.display_name(),
                tab: listing::tab_label(row),
                listed: listing::listed_label(row).unwrap_or_default(),
                listed_chaos: gap.listed,
                value_chaos: gap.value,
                times: gap.value / gap.listed,
                stack,
                loss_chaos: loss,
                age_secs: now - row.price_since.unwrap_or(row.first_seen),
            });
        }
    }

    report.tabs = tabs.into_values().collect();
    report.tabs.sort_by(|a, b| {
        b.underpriced
            .cmp(&a.underpriced)
            .then_with(|| match (a.index, b.index) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    report
        .underpriced
        .sort_by(|a, b| b.loss_chaos.total_cmp(&a.loss_chaos));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ninja::PriceLine;

    const NOW: i64 = 100 * 3600;

    fn rates(divine: f64) -> Rates {
        Rates::from_lines(&[PriceLine {
            category: "Currency".into(),
            name: Some("Divine Orb".into()),
            chaos: divine,
            ..Default::default()
        }])
    }

    fn item(
        name: &str,
        amount: Option<f64>,
        currency: Option<&str>,
        value: Option<f64>,
    ) -> ItemRow {
        ItemRow {
            id: name.to_ascii_lowercase(),
            stash_id: "dump".into(),
            name: Some(name.into()),
            type_line: Some("Leather Belt".into()),
            stash_name: Some("Dump".into()),
            tab_index: Some(0),
            price_amount: amount,
            price_currency: currency.map(str::to_owned),
            price_type: amount.map(|_| "b/o".to_string()),
            value_chaos: value,
            source: "own".into(),
            first_seen: 0,
            price_since: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn flags_an_item_worth_far_more_than_its_price() {
        // 1 divine (400c) against a 1000c market value.
        let rows = [item("Headhunter", Some(1.0), Some("divine"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.underpriced.len(), 1);
        let finding = &report.underpriced[0];
        assert_eq!(finding.loss_chaos, 600.0);
        assert_eq!(finding.times, 2.5);
        assert_eq!(finding.listed, "1 divine");
        assert_eq!(report.value_at_risk, 600.0);
        assert_eq!(report.tabs[0].underpriced, 1);
        assert!(
            report
                .summary()
                .starts_with("1 underpriced (~600c at risk)")
        );
    }

    #[test]
    fn leaves_healthy_listings_alone() {
        let rows = [item("Headhunter", Some(3.0), Some("divine"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.underpriced.is_empty(), "1200c is not underpriced");
        assert!(report.is_quiet());
        assert_eq!(report.summary(), "nothing underpriced");
    }

    #[test]
    fn ignores_items_below_the_minimum_value() {
        let rows = [item("Trinket", Some(0.01), Some("divine"), Some(4.0))];
        assert!(
            analyze(&rows, &rates(400.0), NOW, &Settings::default())
                .underpriced
                .is_empty()
        );
    }

    #[test]
    fn a_tab_priced_by_name_is_one_row() {
        let rows: Vec<ItemRow> = (0..10)
            .map(|i| ItemRow {
                stash_name: Some("~price 1 divine".into()),
                tab_price_since: Some(NOW - 1800),
                stash_last_seen: Some(NOW - 60),
                price_from_tab: true,
                ..item(&format!("item{i}"), Some(1.0), Some("divine"), Some(300.0))
            })
            .collect();
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());

        assert_eq!(report.tabs.len(), 1);
        let tab = &report.tabs[0];
        assert_eq!(tab.items, 10);
        assert_eq!(tab.price.as_deref(), Some("1 divine"));
        assert_eq!(tab.at_price_secs, Some(1800));
        assert_eq!(tab.scanned_secs, Some(60));
        assert_eq!(tab.value_chaos, 3000.0);
        assert!(report.underpriced.is_empty());
    }

    #[test]
    fn an_outlier_in_a_priced_tab_is_listed() {
        let mut rows: Vec<ItemRow> = (0..10)
            .map(|i| {
                item(
                    &format!("filler{i}"),
                    Some(1.0),
                    Some("divine"),
                    Some(400.0),
                )
            })
            .collect();
        let mut hh = item("Headhunter", Some(1.0), Some("divine"), Some(16_000.0));
        hh.price_from_tab = true;
        rows.push(hh);
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());

        assert_eq!(report.tabs[0].underpriced, 1);
        assert_eq!(report.underpriced[0].name, "Headhunter (Leather Belt)");
        assert_eq!(report.underpriced[0].listed, "1 divine (tab)");
        assert_eq!(report.value_at_risk, 15_600.0);
    }

    #[test]
    fn unknown_currency_is_reported() {
        let rows = [item("Thing", Some(1.0), Some("weirdbuck"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.underpriced.is_empty());
        assert_eq!(report.unconvertible.get("weirdbuck"), Some(&1));
        assert!(
            report
                .summary()
                .contains("1 notes in unknown currencies (weirdbuck)")
        );
    }

    #[test]
    fn stack_multiplies_value_and_loss() {
        let mut row = item("Scarab", Some(1.0), Some("chaos"), Some(10.0));
        row.stack_size = Some(10);
        let report = analyze(&[row], &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.tabs[0].value_chaos, 100.0);
        assert_eq!(report.underpriced[0].stack, 10.0);
        assert_eq!(report.underpriced[0].loss_chaos, 90.0);
        assert_eq!(report.value_at_risk, 90.0);
    }

    #[test]
    fn tab_selector_limits_the_report() {
        let mut row = item("Headhunter", Some(1.0), Some("divine"), Some(1000.0));
        row.stash_name = Some("Sell".into());
        let settings = Settings {
            tab: Some(TabSelector::Name("dump".into())),
            ..Default::default()
        };
        let report = analyze(&[row.clone()], &rates(400.0), NOW, &settings);
        assert!(report.is_quiet());
        assert!(report.tabs.is_empty());

        let settings = Settings {
            tab: Some(TabSelector::Index(0)),
            ..Default::default()
        };
        assert_eq!(analyze(&[row], &rates(400.0), NOW, &settings).tabs.len(), 1);
    }

    #[test]
    fn tabs_with_findings_come_first_then_by_index() {
        let mut a = item("a", Some(1.0), Some("chaos"), Some(1.0));
        a.stash_id = "a".into();
        a.tab_index = Some(1);
        let mut b = item("b", Some(1.0), Some("chaos"), Some(100.0));
        b.stash_id = "b".into();
        b.tab_index = Some(5);
        let mut c = item("c", Some(1.0), Some("chaos"), Some(1.0));
        c.stash_id = "c".into();
        c.tab_index = None;
        let mut d = item("d", Some(1.0), Some("chaos"), Some(1.0));
        d.stash_id = "d".into();
        d.tab_index = Some(0);
        let report = analyze(&[a, b, c, d], &rates(400.0), NOW, &Settings::default());
        let order: Vec<&str> = report.tabs.iter().map(|t| t.stash_id.as_str()).collect();
        assert_eq!(order, vec!["b", "d", "a", "c"]);
    }
}
