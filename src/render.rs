//! Terminal rendering helpers.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn ts(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| unix.to_string())
}

/// Compact age, e.g. `42s`, `17m`, `3h`, `12d`; `?` for a time in the future.
pub fn age(unix: i64) -> String {
    let secs = crate::store::unix_now() - unix;
    if secs < 0 {
        return "?".into();
    }
    duration(secs)
}

/// Compact duration, e.g. `42s`, `17m`, `3h`, `12d`. Negative values clamp to 0.
pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// A number for humans: whole numbers without decimals, large numbers rounded,
/// everything else to at most two decimals (`1.35`, `1.5`, `120`, `1235`).
pub fn num(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if (v - v.round()).abs() < 1e-9 {
        return format!("{}", v.round() as i64);
    }
    if v.abs() >= 100.0 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Replace control characters (escape sequences, newlines) so text from the
/// API cannot rewrite the terminal.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Render a left-aligned table. Cells longer than `max` are ellipsised so one
/// pathological item name cannot blow up the layout.
pub fn table(headers: &[&str], rows: &[Vec<String>], max: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let headers: Vec<String> = headers.iter().map(|h| clean(h)).collect();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| clean(c)).collect())
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            rows.iter()
                .map(|r| r.get(i).map(|c| c.chars().count()).unwrap_or(0))
                .chain(std::iter::once(h.chars().count()))
                .max()
                .unwrap_or(0)
                .min(max)
        })
        .collect();

    let mut out = String::new();
    let push = |cells: Vec<String>, out: &mut String| {
        let mut line = String::new();
        for (i, w) in widths.iter().enumerate() {
            let cell = cells.get(i).cloned().unwrap_or_default();
            let cell = if cell.chars().count() > *w {
                let mut s: String = cell.chars().take(w.saturating_sub(1)).collect();
                s.push('…');
                s
            } else {
                cell
            };
            line.push_str(&cell);
            if i + 1 < widths.len() {
                let pad = w.saturating_sub(cell.chars().count());
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };

    push(
        headers.iter().map(|h| h.to_ascii_uppercase()).collect(),
        &mut out,
    );
    for row in rows {
        push(row, &mut out);
    }
    out
}

/// `30m`, `6h`, `7d` as a duration in seconds.
pub fn parse_duration(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (value, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    let value: i64 = value.parse().ok()?;
    Some(match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86_400,
        _ => return None,
    })
}

/// A duration back-dated from now (for `--since`).
pub fn parse_since(raw: &str) -> Option<i64> {
    parse_duration(raw).map(|secs| crate::store::unix_now() - secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relative_durations() {
        let now = crate::store::unix_now();
        let one_hour = parse_since("1h").unwrap();
        assert!((now - one_hour - 3600).abs() <= 2);
        assert!(parse_since("1w").is_none());
        assert!(parse_since("abc").is_none());
    }

    #[test]
    fn table_truncates_wide_cells() {
        let out = table(&["a", "b"], &[vec!["x".repeat(80), "short".into()]], 10);
        assert!(out.contains('…'));
        assert!(out.contains("short"));
    }

    #[test]
    fn formats_numbers_for_humans() {
        assert_eq!(num(1.35), "1.35");
        assert_eq!(num(1.5), "1.5");
        assert_eq!(num(120.0), "120");
        assert_eq!(num(1234.6), "1235");
    }

    #[test]
    fn cleans_control_characters() {
        assert_eq!(clean("a\u{1b}[31mb"), "a\u{FFFD}[31mb");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(1800), "30m");
        assert_eq!(duration(-5), "0s");
    }
}
