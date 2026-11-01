//! Terminal rendering helpers.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn ts(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| unix.to_string())
}

/// Compact age, e.g. `42s`, `17m`, `3h`, `12d`.
pub fn age(unix: i64) -> String {
    let secs = crate::store::unix_now() - unix;
    if secs < 0 {
        return "?".into();
    }
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Render a left-aligned table. Cells longer than `max` are ellipsised so one
/// pathological item name cannot blow up the layout.
pub fn table(headers: &[&str], rows: &[Vec<String>], max: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }
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
        push(row.clone(), &mut out);
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
}
