//! Dump-tab health: underpricing and price decay.
//!
//! Two failure modes this catches:
//!
//! * an item listed far below its market value (a typo, or a stale price on
//!   something that got expensive) — money left on the table;
//! * a tab nobody has touched in hours — sales stall because the price never
//!   moves down.
//!
//! The decay clock is derived from the item's own price history, so the moment
//! you re-price in game the watcher records a `price_changed` event and the
//! reminder for that item disappears by itself.

use serde::Serialize;

use crate::rates::Rates;
use crate::store::ItemRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Listed well below market value.
    Underpriced,
    /// Valuable, but has no price note at all.
    Unpriced,
    /// Sitting at the same price long enough to be cut.
    DueForCut,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub kind: Kind,
    pub id: String,
    pub name: String,
    pub tab: Option<String>,
    pub listed: Option<String>,
    pub listed_chaos: Option<f64>,
    pub value_chaos: Option<f64>,
    /// listed / value; below 1.0 means listed under market.
    pub ratio: Option<f64>,
    pub loss_chaos: Option<f64>,
    pub age_secs: i64,
    pub suggested: Option<String>,
    pub suggested_amount: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Tab name substring to restrict to (the dump tab).
    pub tab: Option<String>,
    /// Flag when listed price is below this fraction of market value.
    pub underprice_ratio: f64,
    /// Ignore items whose market value is below this many chaos.
    pub min_value_chaos: f64,
    /// Ignore underpricing worth less than this many chaos.
    pub min_loss_chaos: f64,
    /// Suggest a cut after this long at the same price.
    pub cut_after_secs: i64,
    /// Cut size in percent.
    pub cut_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tab: None,
            underprice_ratio: 0.5,
            min_value_chaos: 20.0,
            min_loss_chaos: 10.0,
            cut_after_secs: 12 * 3600,
            cut_pct: 20.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub underpriced: Vec<Finding>,
    pub unpriced: Vec<Finding>,
    pub due: Vec<Finding>,
    /// Chaos of value missed if the underpriced items sell as listed.
    pub value_at_risk: f64,
    pub tabs: Vec<String>,
}

impl Report {
    pub fn is_quiet(&self) -> bool {
        self.underpriced.is_empty() && self.unpriced.is_empty() && self.due.is_empty()
    }

    /// One-line summary, used for logs and desktop notifications.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.underpriced.is_empty() {
            parts.push(format!(
                "{} underpriced (~{}c at risk)",
                self.underpriced.len(),
                self.value_at_risk.round() as i64
            ));
        }
        if !self.unpriced.is_empty() {
            parts.push(format!("{} unpriced", self.unpriced.len()));
        }
        if !self.due.is_empty() {
            parts.push(format!("{} due for a cut", self.due.len()));
        }
        if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            parts.join(", ")
        }
    }
}

pub fn analyze(rows: &[ItemRow], rates: &Rates, now: i64, settings: &Settings) -> Report {
    let mut report = Report::default();
    let mut underpriced_ids: Vec<String> = Vec::new();

    for row in rows {
        if row.source != "own" || row.delisted_at.is_some() {
            continue;
        }
        if let Some(tab) = &settings.tab {
            let name = row.stash_name.as_deref().unwrap_or_default();
            if !name
                .to_ascii_lowercase()
                .contains(&tab.to_ascii_lowercase())
            {
                continue;
            }
        }
        let tab = row.stash_name.clone();
        if let Some(tab) = &tab
            && !report.tabs.contains(tab)
        {
            report.tabs.push(tab.clone());
        }

        let age = now - row.price_since.unwrap_or(row.first_seen);
        let value = row.value_chaos.filter(|v| *v > 0.0);
        let listed_chaos = row
            .price_amount
            .zip(row.price_currency.as_deref())
            .and_then(|(amount, currency)| rates.chaos_per_unit(currency).map(|r| amount * r));

        let mut flagged = false;

        match (value, listed_chaos) {
            (Some(value), Some(listed)) => {
                let ratio = listed / value;
                let loss = value - listed;
                if value >= settings.min_value_chaos
                    && ratio < settings.underprice_ratio
                    && loss >= settings.min_loss_chaos
                {
                    flagged = true;
                    underpriced_ids.push(row.id.clone());
                    report.value_at_risk += loss;
                    report.underpriced.push(Finding {
                        kind: Kind::Underpriced,
                        id: row.id.clone(),
                        name: row.display_name(),
                        tab: tab.clone(),
                        listed: row.price().map(|p| strip_kind(&p)),
                        listed_chaos: Some(listed),
                        value_chaos: Some(value),
                        ratio: Some(ratio),
                        loss_chaos: Some(loss),
                        age_secs: age,
                        suggested: None,
                        suggested_amount: None,
                        currency: row.price_currency.clone(),
                    });
                }
            }
            // No price at all, or a currency we cannot convert.
            (Some(value), None)
                if row.price_amount.is_none() && value >= settings.min_value_chaos =>
            {
                report.unpriced.push(Finding {
                    kind: Kind::Unpriced,
                    id: row.id.clone(),
                    name: row.display_name(),
                    tab: tab.clone(),
                    listed: None,
                    listed_chaos: None,
                    value_chaos: Some(value),
                    ratio: None,
                    loss_chaos: None,
                    age_secs: age,
                    suggested: None,
                    suggested_amount: None,
                    currency: None,
                });
            }
            _ => {}
        }

        // Any priced item that has been sitting still long enough gets a cut
        // suggestion — except the ones already flagged as too cheap.
        if !flagged
            && let (Some(amount), Some(currency)) =
                (row.price_amount, row.price_currency.as_deref())
            && amount > 0.0
            && age >= settings.cut_after_secs
        {
            let cut = amount * (1.0 - settings.cut_pct / 100.0);
            let suggested = round_price(cut, currency);
            report.due.push(Finding {
                kind: Kind::DueForCut,
                id: row.id.clone(),
                name: row.display_name(),
                tab: tab.clone(),
                listed: row.price().map(|p| strip_kind(&p)),
                listed_chaos,
                value_chaos: value,
                ratio: listed_chaos.zip(value).map(|(l, v)| l / v),
                loss_chaos: None,
                age_secs: age,
                suggested: Some(format_price(suggested, currency)),
                suggested_amount: Some(suggested),
                currency: Some(currency.to_owned()),
            });
        }
    }

    report.underpriced.sort_by(|a, b| {
        b.loss_chaos
            .unwrap_or(0.0)
            .total_cmp(&a.loss_chaos.unwrap_or(0.0))
    });
    report.unpriced.sort_by(|a, b| {
        b.value_chaos
            .unwrap_or(0.0)
            .total_cmp(&a.value_chaos.unwrap_or(0.0))
    });
    report.due.sort_by_key(|f| std::cmp::Reverse(f.age_secs));

    report
}

/// Price notes carry their kind (`b/o 5 divine`); the report only needs the
/// amount and currency.
fn strip_kind(price: &str) -> String {
    let mut parts = price.split_whitespace();
    match parts.next() {
        Some("b/o" | "gb/o" | "c/o") => parts.collect::<Vec<_>>().join(" "),
        _ => price.to_string(),
    }
}

fn format_price(amount: f64, currency: &str) -> String {
    if amount >= 10.0 || (amount - amount.round()).abs() < f64::EPSILON {
        format!("{} {}", amount.round() as i64, currency)
    } else {
        format!("{amount:.1} {}", currency)
    }
}

/// Keep suggestions trade-shaped: whole chaos, one decimal for everything else.
fn round_price(amount: f64, currency: &str) -> f64 {
    if currency.eq_ignore_ascii_case("chaos") {
        amount.floor().max(1.0)
    } else {
        ((amount * 10.0).round() / 10.0).max(0.1)
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

    fn item(
        name: &str,
        amount: Option<f64>,
        currency: Option<&str>,
        value: Option<f64>,
    ) -> ItemRow {
        ItemRow {
            id: name.to_ascii_lowercase(),
            name: Some(name.into()),
            type_line: Some("Leather Belt".into()),
            stash_name: Some("Dump".into()),
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

    const NOW: i64 = 100 * 3600;

    #[test]
    fn flags_a_badly_underpriced_item() {
        // 1 divine (400c) against a 1000c market value.
        let rows = [item("Headhunter", Some(1.0), Some("divine"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.underpriced.len(), 1);
        assert_eq!(report.underpriced[0].loss_chaos, Some(600.0));
        assert_eq!(report.value_at_risk, 600.0);
        assert!(
            report.due.is_empty(),
            "underpriced items are not cut further"
        );
    }

    #[test]
    fn leaves_healthy_listings_alone() {
        let rows = [item("Headhunter", Some(3.0), Some("divine"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.underpriced.is_empty(), "1200c is not underpriced");
    }

    #[test]
    fn ignores_cheap_items_and_small_gaps() {
        // Worth 15c: below min_value_chaos.
        let rows = [item("Trinket", Some(0.01), Some("divine"), Some(15.0))];
        assert!(
            analyze(&rows, &rates(400.0), NOW, &Settings::default())
                .underpriced
                .is_empty()
        );

        // Worth 100c, listed at 60c: a 40c gap, under the 50% ratio? no (0.6).
        let rows = [item("Mid", Some(60.0), Some("chaos"), Some(100.0))];
        assert!(
            analyze(&rows, &rates(400.0), NOW, &Settings::default())
                .underpriced
                .is_empty()
        );
    }

    #[test]
    fn flags_valuable_items_without_a_price() {
        let rows = [item("Mageblood", None, None, Some(90_000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.unpriced.len(), 1);
        assert!(report.unpriced[0].name.contains("Mageblood"));
    }

    #[test]
    fn suggests_a_cut_after_the_interval() {
        let mut row = item("Mageblood", Some(10.0), Some("divine"), Some(4000.0));
        row.price_since = Some(NOW - 13 * 3600);
        let report = analyze(&[row], &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.due.len(), 1);
        assert_eq!(report.due[0].suggested.as_deref(), Some("8 divine"));
        assert_eq!(report.due[0].suggested_amount, Some(8.0));
    }

    #[test]
    fn young_listings_are_not_due() {
        let mut row = item("Mageblood", Some(10.0), Some("divine"), Some(4000.0));
        row.price_since = Some(NOW - 3600);
        assert!(
            analyze(&[row], &rates(400.0), NOW, &Settings::default())
                .due
                .is_empty()
        );
    }

    #[test]
    fn chaos_cuts_are_whole_numbers() {
        let mut row = item("Ring", Some(15.0), Some("chaos"), Some(15.0));
        row.price_since = Some(NOW - 13 * 3600);
        let report = analyze(&[row], &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.due[0].suggested.as_deref(), Some("12 chaos"));
    }

    #[test]
    fn tab_filter_limits_the_report() {
        let mut row = item("Headhunter", Some(1.0), Some("divine"), Some(1000.0));
        row.stash_name = Some("Sell".into());
        let settings = Settings {
            tab: Some("dump".into()),
            ..Default::default()
        };
        let report = analyze(&[row], &rates(400.0), NOW, &settings);
        assert!(report.is_quiet());
    }

    #[test]
    fn unknown_currency_is_not_called_underpriced() {
        let rows = [item("Thing", Some(1.0), Some("weirdbuck"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.underpriced.is_empty());
        assert!(report.unpriced.is_empty(), "it does have a price");
    }
}
