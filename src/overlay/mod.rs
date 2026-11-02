//! In-game stash overlay.
//!
//! Draws a coloured frame around every item in the open stash tab, on the
//! compositor's overlay layer so it stays visible over a fullscreen game:
//!
//! * green  — worth clearly more than it is listed at; price it by hand
//! * blue   — belongs in a specialised tab (gems, currency, cards, flasks,
//!   contracts, maps)
//! * grey   — settled: the tab price is fine
//!
//! Geometry comes from little_oil's stash calibration (region + per-column and
//! per-row edges), and item positions come from the stash API (`x`/`y`/`w`/`h`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::listing::{self, Rule};
use crate::rates::Rates;
use crate::store::ItemRow;

#[cfg(target_os = "linux")]
pub mod wayland;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    /// Listed around market value.
    Settled,
    /// Worth more than it is listed at — price it manually.
    Cheap,
    /// Has a home in a specialised tab.
    Groupable,
}

impl Colour {
    /// Straight-alpha RGB.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Colour::Settled => (0x9A, 0x9A, 0x9A),
            Colour::Cheap => (0x3D, 0xDC, 0x6B),
            Colour::Groupable => (0x4A, 0xA8, 0xFF),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Colour::Settled => "settled",
            Colour::Cheap => "too cheap",
            Colour::Groupable => "move to group tab",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub rect: Rect,
    pub colour: Colour,
}

#[derive(Debug, Clone)]
pub struct Calibration {
    /// The stash grid on screen (top-left of the first cell).
    pub stash: Rect,
    pub cell_w: u32,
    pub cell_h: u32,
    /// Left edge of each column, relative to `stash.x`.
    pub cols: Vec<u32>,
    /// Top edge of each row, relative to `stash.y`.
    pub rows: Vec<u32>,
}

impl Calibration {
    /// Read little_oil's calibration (`stash_region` + `stash_grid`), so both
    /// tools agree on where the grid is.
    pub fn from_little_oil(path: &Path) -> Result<Calibration> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading little_oil config {}", path.display()))?;
        let config: LittleOil = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let region = config
            .stash_region
            .context("little_oil config has no stash_region — run `little_oil set-region stash`")?;
        let grid = config
            .stash_grid
            .context("little_oil config has no stash_grid — run `little_oil calibrate-stash`")?;
        if grid.cols.len() < 2 || grid.rows.len() < 2 {
            bail!(
                "little_oil stash_grid is incomplete ({} cols, {} rows)",
                grid.cols.len(),
                grid.rows.len()
            );
        }
        Ok(Calibration {
            stash: Rect {
                x: region.x,
                y: region.y,
                w: region.width,
                h: region.height,
            },
            cell_w: grid.cell_w,
            cell_h: grid.cell_h,
            cols: grid.cols,
            rows: grid.rows,
        })
    }

    pub fn default_little_oil_path() -> Result<PathBuf> {
        dirs::config_dir()
            .map(|d| d.join("little_oil").join("config.json"))
            .context(
                "cannot determine the config directory; pass --config <little_oil config.json>",
            )
    }

    /// Screen rectangle of an item occupying `w`×`h` cells at `(x, y)`.
    pub fn cell_rect(&self, x: i64, y: i64, w: i64, h: i64) -> Option<Rect> {
        if x < 0 || y < 0 || w <= 0 || h <= 0 {
            return None;
        }
        let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
        let left = *self.cols.get(x)?;
        let top = *self.rows.get(y)?;
        // Last cell's far edge, so multi-cell items span the grid correctly.
        let right = match self.cols.get(x + w) {
            Some(next) => *next,
            None => *self.cols.get(x + w - 1)? + self.cell_w,
        };
        let bottom = match self.rows.get(y + h) {
            Some(next) => *next,
            None => *self.rows.get(y + h - 1)? + self.cell_h,
        };
        Some(Rect {
            x: self.stash.x + left as i32,
            y: self.stash.y + top as i32,
            w: right.saturating_sub(left),
            h: bottom.saturating_sub(top),
        })
    }
}

/// Frames for every item that has a position on the grid.
pub fn marks(rows: &[&ItemRow], rates: &Rates, cal: &Calibration, rule: &Rule) -> Vec<Mark> {
    rows.iter()
        .filter(|row| row.delisted_at.is_none())
        .filter_map(|row| {
            let rect = cal.cell_rect(row.x?, row.y?, row.w.unwrap_or(1), row.h.unwrap_or(1))?;
            Some(Mark {
                rect,
                colour: classify(row, rates, rule),
            })
        })
        .collect()
}

/// Green when worth clearly more than listed (the shared [`Rule`]), or an
/// unidentified unique — nothing is known about those until identified.
pub fn classify(row: &ItemRow, rates: &Rates, rule: &Rule) -> Colour {
    if listing::unidentified_unique(row) || listing::underpriced(row, rates, rule).is_some() {
        Colour::Cheap
    } else if is_groupable(row) {
        Colour::Groupable
    } else {
        Colour::Settled
    }
}

/// Items with a natural home in another tab.
pub fn is_groupable(row: &ItemRow) -> bool {
    if matches!(row.frame_type, Some(4) | Some(5) | Some(6)) {
        return true;
    }
    let base = row.base_type.as_deref().unwrap_or_default();
    let type_line = row.type_line.as_deref().unwrap_or_default();
    let text = |s: &str| s.to_ascii_lowercase();

    let base_l = text(base);
    let type_l = text(type_line);
    base_l.ends_with("flask")
        || type_l.ends_with("flask")
        || type_l.starts_with("contract:")
        || type_l.starts_with("blueprint:")
        || base_l.ends_with("map")
        || type_l.ends_with("map")
}

#[derive(Deserialize)]
struct LittleOil {
    stash_region: Option<RegionJson>,
    stash_grid: Option<GridJson>,
}

#[derive(Deserialize)]
struct RegionJson {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct GridJson {
    cols: Vec<u32>,
    rows: Vec<u32>,
    cell_w: u32,
    cell_h: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal() -> Calibration {
        Calibration {
            stash: Rect {
                x: 100,
                y: 200,
                w: 330,
                h: 340,
            },
            cell_w: 33,
            cell_h: 34,
            cols: (0..12).map(|i| i * 33).collect(),
            rows: (0..12).map(|i| i * 34).collect(),
        }
    }

    fn row(name: &str) -> ItemRow {
        ItemRow {
            id: name.into(),
            name: Some(name.into()),
            source: "own".into(),
            x: Some(2),
            y: Some(3),
            w: Some(1),
            h: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn maps_grid_cells_to_screen() {
        let rect = cal().cell_rect(2, 3, 1, 1).unwrap();
        assert_eq!(
            rect,
            Rect {
                x: 100 + 66,
                y: 200 + 102,
                w: 33,
                h: 34
            }
        );
    }

    #[test]
    fn multi_cell_items_span_the_grid() {
        // A 2x3 armour piece starting at (1,1).
        let rect = cal().cell_rect(1, 1, 2, 3).unwrap();
        assert_eq!(rect.x, 133);
        assert_eq!(rect.y, 234);
        assert_eq!(rect.w, 66);
        assert_eq!(rect.h, 102);
    }

    #[test]
    fn out_of_range_positions_are_ignored() {
        assert!(cal().cell_rect(99, 0, 1, 1).is_none());
        assert!(cal().cell_rect(0, 99, 1, 1).is_none());
        assert!(cal().cell_rect(-1, 0, 1, 1).is_none());
    }

    #[test]
    fn gems_currency_and_cards_are_groupable() {
        let mut gem = row("Vaal Fireball");
        gem.frame_type = Some(4);
        assert!(is_groupable(&gem));

        let mut card = row("The Doctor");
        card.frame_type = Some(6);
        assert!(is_groupable(&card));

        let mut flask = row("Divine Life Flask");
        flask.frame_type = Some(2);
        flask.base_type = Some("Divine Life Flask".into());
        assert!(is_groupable(&flask));

        let mut contract = row("Contract: Breaking and Entering");
        contract.type_line = Some("Contract: Breaking and Entering".into());
        contract.base_type = Some("Contract: Breaking and Entering".into());
        assert!(is_groupable(&contract));

        let mut map = row("Strand Map");
        map.base_type = Some("Strand Map".into());
        assert!(is_groupable(&map));
    }

    #[test]
    fn ordinary_uniques_are_not_groupable() {
        let mut unique = row("Headhunter");
        unique.frame_type = Some(3);
        unique.base_type = Some("Leather Belt".into());
        assert!(!is_groupable(&unique));
    }

    #[test]
    fn cheap_items_win_over_grouping() {
        let mut gem = row("Vaal Fireball");
        gem.frame_type = Some(4);
        gem.value_chaos = Some(4000.0);
        gem.price_amount = Some(1.0);
        gem.price_currency = Some("divine".into());
        let rates = Rates::from_lines(&[crate::ninja::PriceLine {
            category: "Currency".into(),
            name: Some("Divine Orb".into()),
            chaos: 400.0,
            ..Default::default()
        }]);
        assert_eq!(classify(&gem, &rates, &Rule::default()), Colour::Cheap);

        // Without a rate the price cannot be judged, so it stays blue.
        let none = Rates::from_lines(&[]);
        assert_eq!(classify(&gem, &none, &Rule::default()), Colour::Groupable);
    }

    #[test]
    fn unidentified_unique_is_flagged() {
        let mut unique = row("Leather Belt");
        unique.frame_type = Some(3);
        unique.identified = Some(false);
        unique.name = None;
        let rates = Rates::from_lines(&[]);
        assert_eq!(classify(&unique, &rates, &Rule::default()), Colour::Cheap);
    }

    #[test]
    fn marks_skip_items_without_positions() {
        let mut hidden = row("Hidden");
        hidden.x = None;
        let mut placed = row("Placed");
        placed.y = Some(4);
        let rates = Rates::from_lines(&[]);
        let marks = marks(&[&hidden, &placed], &rates, &cal(), &Rule::default());
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].rect.y, 200 + 136);
    }
}
