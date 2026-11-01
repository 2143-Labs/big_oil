//! Dump-tab health: underpricing and price decay.
//!
//! The practice this models: everything goes into a priced dump tab (often a
//! quad tab at 1 divine, or a ladder of rotating tabs at 5c/10c/20c/50c/1div),
//! and the price is stepped down every few hours or days until things sell.
//! Two things cost money:
//!
//! * an item worth far more than the tab price sitting in that tab — it sells
//!   the instant someone sees it, at the tab price;
//! * a tab nobody has touched, so the price never steps down and nothing moves.
//!
//! Because a tab price applies to every item in it, the useful unit is the tab,
//! not the item. The report therefore leads with one line per tab and only
//! lists individual items for mixed tabs or for genuine outliers.

use std::collections::HashMap;

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

/// Per-tab rollup: this is what a dump tab actually looks like.
#[derive(Debug, Clone, Serialize)]
pub struct TabSummary {
    pub name: String,
    pub items: usize,
    pub priced: usize,
    pub unpriced: usize,
    /// The price most items share, when the tab is priced as a whole.
    pub price: Option<String>,
    pub price_amount: Option<f64>,
    pub price_currency: Option<String>,
    /// Most items share one price, so the tab is managed as a unit.
    pub tab_priced: bool,
    /// Age of the oldest item at its current price.
    pub oldest_secs: i64,
    pub due: bool,
    pub suggested: Option<String>,
    pub suggested_amount: Option<f64>,
    pub underpriced: usize,
    /// Total known market value of the tab's live items.
    pub value_chaos: f64,
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
    /// Step down by a fixed amount instead of a percentage (queue-method
    /// ladders use a constant step, e.g. 10c).
    pub cut_abs: Option<f64>,
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
            cut_abs: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub tabs: Vec<TabSummary>,
    pub underpriced: Vec<Finding>,
    pub unpriced: Vec<Finding>,
    pub due: Vec<Finding>,
    /// Chaos of value missed if the underpriced items sell as listed.
    pub value_at_risk: f64,
}

impl Report {
    pub fn is_quiet(&self) -> bool {
        self.underpriced.is_empty()
            && self.unpriced.is_empty()
            && self.due.is_empty()
            && !self.tabs.iter().any(|t| t.due)
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
        let due_tabs = self.tabs.iter().filter(|t| t.due).count();
        if due_tabs > 0 {
            parts.push(format!(
                "{due_tabs} tab{} due a cut",
                if due_tabs == 1 { "" } else { "s" }
            ));
        }
        if !self.due.is_empty() {
            parts.push(format!("{} individual item(s) due", self.due.len()));
        }
        if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug, Clone)]
struct PriceCount {
    amount: f64,
    currency: String,
    count: usize,
    total: usize,
}

#[derive(Default)]
struct TabAcc {
    items: usize,
    priced: usize,
    unpriced: usize,
    value: f64,
    oldest: i64,
    underpriced: usize,
}

const UNNAMED: &str = "<unnamed tab>";
/// A tab counts as priced as a whole when most items share one price and there
/// are enough of them to be a tab rather than a coincidence.
const TAB_PRICED_MIN: usize = 3;
const TAB_PRICED_SHARE: f64 = 0.6;

fn tab_key(row: &ItemRow) -> String {
    row.stash_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| UNNAMED.to_string())
}

fn price_key(amount: f64, currency: &str) -> String {
    format!("{amount:.4}|{}", currency.to_ascii_lowercase())
}

fn row_matches(row: &ItemRow, settings: &Settings) -> bool {
    if row.source != "own" || row.delisted_at.is_some() {
        return false;
    }
    if let Some(tab) = &settings.tab {
        let name = row.stash_name.as_deref().unwrap_or_default();
        if !name
            .to_ascii_lowercase()
            .contains(&tab.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}

pub fn analyze(rows: &[ItemRow], rates: &Rates, now: i64, settings: &Settings) -> Report {
    let live: Vec<&ItemRow> = rows.iter().filter(|r| row_matches(r, settings)).collect();

    // Pass 1: what price does each tab's stock share?
    let mut counts: HashMap<String, HashMap<String, PriceCount>> = HashMap::new();
    for row in &live {
        let Some(amount) = row.price_amount else {
            continue;
        };
        let Some(currency) = row.price_currency.as_deref() else {
            continue;
        };
        let entry = counts
            .entry(tab_key(row))
            .or_default()
            .entry(price_key(amount, currency))
            .or_insert_with(|| PriceCount {
                amount,
                currency: currency.to_owned(),
                count: 0,
                total: 0,
            });
        entry.count += 1;
    }
    for per_tab in counts.values_mut() {
        let total: usize = per_tab.values().map(|c| c.count).sum();
        for count in per_tab.values_mut() {
            count.total = total;
        }
    }

    // A tab is "priced as a whole" when most of its stock shares one price.
    let dominant_by_tab: HashMap<String, PriceCount> = counts
        .iter()
        .filter_map(|(tab, per_tab)| {
            per_tab
                .values()
                .max_by_key(|c| c.count)
                .filter(|c| c.count >= TAB_PRICED_MIN)
                .filter(|c| c.count as f64 >= c.total as f64 * TAB_PRICED_SHARE)
                .cloned()
                .map(|d| (tab.clone(), d))
        })
        .collect();

    let mut report = Report::default();
    let mut accs: HashMap<String, TabAcc> = HashMap::new();

    for row in &live {
        let tab = tab_key(row);
        let acc = accs.entry(tab.clone()).or_default();
        acc.items += 1;

        let age = now - row.price_since.unwrap_or(row.first_seen);
        acc.oldest = acc.oldest.max(age);

        let value = row.value_chaos.filter(|v| *v > 0.0);
        if let Some(value) = value {
            acc.value += value;
        }
        let listed_chaos = row
            .price_amount
            .zip(row.price_currency.as_deref())
            .and_then(|(amount, currency)| rates.chaos_per_unit(currency).map(|r| amount * r));

        if row.price_amount.is_some() {
            acc.priced += 1;
        } else {
            acc.unpriced += 1;
        }

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
                    acc.underpriced += 1;
                    report.value_at_risk += loss;
                    report.underpriced.push(Finding {
                        kind: Kind::Underpriced,
                        id: row.id.clone(),
                        name: row.display_name(),
                        tab: row.stash_name.clone(),
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
                    tab: row.stash_name.clone(),
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
        // suggestion — except items already flagged as too cheap, and items
        // that merely share their tab's price (the tab row covers those).
        let shares_tab_price = row
            .price_amount
            .zip(row.price_currency.as_deref())
            .is_some_and(|(amount, currency)| {
                dominant_by_tab.get(&tab).is_some_and(|d| {
                    price_key(amount, currency) == price_key(d.amount, &d.currency)
                })
            });
        if !flagged
            && !shares_tab_price
            && let (Some(amount), Some(currency)) =
                (row.price_amount, row.price_currency.as_deref())
            && amount > 0.0
            && age >= settings.cut_after_secs
        {
            let suggested = step(amount, currency, settings);
            report.due.push(Finding {
                kind: Kind::DueForCut,
                id: row.id.clone(),
                name: row.display_name(),
                tab: row.stash_name.clone(),
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

    // Roll tabs up into one row each.
    for (name, acc) in accs {
        let dominant = dominant_by_tab.get(&name).cloned();
        let tab_priced = dominant.is_some();

        let due = tab_priced && acc.oldest >= settings.cut_after_secs;
        let suggested = if due {
            dominant
                .as_ref()
                .map(|d| format_price(step(d.amount, &d.currency, settings), &d.currency))
        } else {
            None
        };

        report.tabs.push(TabSummary {
            name: name.clone(),
            items: acc.items,
            priced: acc.priced,
            unpriced: acc.unpriced,
            price: dominant
                .as_ref()
                .map(|d| format_price_raw(d.amount, &d.currency)),
            price_amount: dominant.as_ref().map(|d| d.amount),
            price_currency: dominant.as_ref().map(|d| d.currency.clone()),
            tab_priced,
            oldest_secs: acc.oldest,
            due,
            suggested,
            suggested_amount: dominant
                .as_ref()
                .filter(|_| due)
                .map(|d| step(d.amount, &d.currency, settings)),
            underpriced: acc.underpriced,
            value_chaos: acc.value,
        });
    }

    report.tabs.sort_by(|a, b| {
        b.underpriced
            .cmp(&a.underpriced)
            .then_with(|| b.due.cmp(&a.due))
            .then_with(|| b.oldest_secs.cmp(&a.oldest_secs))
    });
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

fn format_price_raw(amount: f64, currency: &str) -> String {
    if (amount - amount.round()).abs() < f64::EPSILON {
        format!("{} {}", amount.round() as i64, currency)
    } else {
        format!("{amount:.1} {}", currency)
    }
}

/// Next step down: a fixed amount when `cut_abs` is set (queue-method ladders),
/// otherwise a percentage. Rounded to trade-shaped numbers.
fn step(amount: f64, currency: &str, settings: &Settings) -> f64 {
    let raw = match settings.cut_abs {
        Some(absolute) => amount - absolute,
        None => amount * (1.0 - settings.cut_pct / 100.0),
    };
    if currency.eq_ignore_ascii_case("chaos") {
        raw.floor().max(1.0)
    } else {
        ((raw * 10.0).round() / 10.0).max(0.1)
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

    fn aged(mut row: ItemRow, secs: i64) -> ItemRow {
        row.price_since = Some(NOW - secs);
        row
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
        let rows = [item("Trinket", Some(0.01), Some("divine"), Some(15.0))];
        assert!(
            analyze(&rows, &rates(400.0), NOW, &Settings::default())
                .underpriced
                .is_empty()
        );

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
        let row = aged(
            item("Mageblood", Some(10.0), Some("divine"), Some(4000.0)),
            13 * 3600,
        );
        let report = analyze(&[row], &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.due.len(), 1);
        assert_eq!(report.due[0].suggested.as_deref(), Some("8 divine"));
        assert_eq!(report.due[0].suggested_amount, Some(8.0));
    }

    #[test]
    fn young_listings_are_not_due() {
        let row = aged(
            item("Mageblood", Some(10.0), Some("divine"), Some(4000.0)),
            3600,
        );
        assert!(
            analyze(&[row], &rates(400.0), NOW, &Settings::default())
                .due
                .is_empty()
        );
    }

    #[test]
    fn chaos_cuts_are_whole_numbers() {
        let row = aged(
            item("Ring", Some(15.0), Some("chaos"), Some(15.0)),
            13 * 3600,
        );
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
        assert!(report.tabs.is_empty());
    }

    #[test]
    fn unknown_currency_is_not_called_underpriced() {
        let rows = [item("Thing", Some(1.0), Some("weirdbuck"), Some(1000.0))];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.underpriced.is_empty());
        assert!(report.unpriced.is_empty(), "it does have a price");
    }

    #[test]
    fn a_tab_priced_as_a_whole_is_reported_as_one_row() {
        // A quad dump tab at 1 divine, 26h stale.
        let rows: Vec<ItemRow> = (0..40)
            .map(|i| {
                aged(
                    item(
                        &format!("item{i}"),
                        Some(1.0),
                        Some("divine"),
                        Some(500.0 + i as f64),
                    ),
                    26 * 3600,
                )
            })
            .collect();
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());

        assert_eq!(report.tabs.len(), 1, "one tab");
        let tab = &report.tabs[0];
        assert!(tab.tab_priced, "40 items at 1 divine is a tab price");
        assert_eq!(tab.items, 40);
        assert_eq!(tab.price.as_deref(), Some("1 divine"));
        assert!(tab.due);
        assert_eq!(tab.suggested.as_deref(), Some("0.8 divine"));
        assert!(
            report.due.is_empty(),
            "per-item rows are redundant once the tab is summarised"
        );
    }

    #[test]
    fn a_mixed_tab_keeps_per_item_rows() {
        let rows = vec![
            aged(
                item("a", Some(10.0), Some("divine"), Some(4000.0)),
                13 * 3600,
            ),
            aged(item("b", Some(5.0), Some("chaos"), Some(5.0)), 13 * 3600),
            aged(item("c", Some(1.0), Some("divine"), Some(500.0)), 13 * 3600),
        ];
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert_eq!(report.tabs.len(), 1);
        assert!(!report.tabs[0].tab_priced, "three different prices");
        assert_eq!(report.due.len(), 3);
    }

    #[test]
    fn a_tab_under_the_cut_interval_is_not_due() {
        let rows: Vec<ItemRow> = (0..10)
            .map(|i| {
                aged(
                    item(&format!("i{i}"), Some(2.0), Some("divine"), Some(4000.0)),
                    3600,
                )
            })
            .collect();
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());
        assert!(report.tabs[0].tab_priced);
        assert!(!report.tabs[0].due);
        assert!(!report.tabs[0].suggested.is_some());
    }

    #[test]
    fn an_outlier_in_a_priced_tab_is_still_listed() {
        // The whole tab is 1 divine, but one item is worth 40 divine: the tab
        // summary says "3 underpriced" and the section names it.
        let mut rows: Vec<ItemRow> = (0..10)
            .map(|i| {
                aged(
                    item(
                        &format!("filler{i}"),
                        Some(1.0),
                        Some("divine"),
                        Some(400.0),
                    ),
                    26 * 3600,
                )
            })
            .collect();
        rows.push(aged(
            item("Headhunter", Some(1.0), Some("divine"), Some(16_000.0)),
            26 * 3600,
        ));
        let report = analyze(&rows, &rates(400.0), NOW, &Settings::default());

        assert_eq!(report.tabs[0].underpriced, 1);
        assert_eq!(report.underpriced.len(), 1);
        assert_eq!(report.underpriced[0].name, "Headhunter (Leather Belt)");
        assert!(report.value_at_risk > 14_000.0);
    }

    #[test]
    fn fixed_step_ladders_use_an_absolute_cut() {
        // Queue method: 50c tab, 10c interval.
        let rows: Vec<ItemRow> = (0..10)
            .map(|i| {
                aged(
                    item(&format!("i{i}"), Some(50.0), Some("chaos"), Some(20.0)),
                    13 * 3600,
                )
            })
            .collect();
        let settings = Settings {
            cut_abs: Some(10.0),
            ..Default::default()
        };
        let report = analyze(&rows, &rates(400.0), NOW, &settings);
        assert_eq!(report.tabs[0].suggested.as_deref(), Some("40 chaos"));
    }
}
