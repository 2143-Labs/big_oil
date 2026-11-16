//! GGG's hourly Currency Exchange digests: fetch, store, verify, expose.
//!
//! The endpoint is documented (`#currencyexchange`) and unauthenticated: one
//! request per hour returns every market in every league for that hour. The
//! SQLite file is the record — the metric layer is derived and may be dropped
//! without losing data.

pub mod client;
pub mod metrics;
pub mod page;
pub mod report;
pub mod serve;
pub mod store;
pub mod walk;

/// Chaos Orb: the unit of account for every chaos-denominated price here.
pub const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";

pub const DEFAULT_BASE_URL: &str = "https://web.poecdn.com/api/currency-exchange";

/// The API's granularity. Hour ids are unix timestamps aligned to this.
pub const HOUR: i64 = 3600;

/// Price of one `base` unit expressed in `quote` units, from a market's two
/// ratio counters.
///
/// Ratios are per-side quantities, so the price of X in Y is
/// `ratio(Y) / ratio(X)`: the chaos/divine market carries
/// `{chaos: 850, divine: 1}` and a Divine was worth 850 chaos.
pub fn price(base_ratio: i64, quote_ratio: i64) -> Option<f64> {
    if base_ratio <= 0 || quote_ratio <= 0 {
        return None;
    }
    let p = quote_ratio as f64 / base_ratio as f64;
    p.is_finite().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_is_quote_over_base() {
        assert_eq!(price(850, 1), Some(1.0 / 850.0));
        assert_eq!(price(1, 850), Some(850.0));
    }

    #[test]
    fn price_rejects_unusable_ratios() {
        assert_eq!(price(0, 1), None);
        assert_eq!(price(1, 0), None);
        assert_eq!(price(-5, 1), None);
    }
}
