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
//! Geometry comes from little_oil's quad-tab calibration: per-column and
//! per-row edges in frame space (relative to its `game_window_region`). A
//! normal or premium tab is a 12×12 grid over the same area, so each of its
//! cells spans two quad cells. Item positions come from the stash API
//! (`x`/`y`/`w`/`h`).

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
    /// Where the overlay surface goes on screen (little_oil's `stash_region`).
    pub surface: Rect,
    /// Screen position of the frame the grid is measured in (little_oil's
    /// `game_window_region` x, y).
    pub origin: (i32, i32),
    pub cell_w: u32,
    pub cell_h: u32,
    /// Left edge of each of the 24 quad columns, in frame space.
    pub cols: Vec<u32>,
    /// Top edge of each of the 24 quad rows, in frame space.
    pub rows: Vec<u32>,
}

/// little_oil always calibrates the 24×24 quad grid.
const QUAD: usize = 24;

impl Calibration {
    /// Read little_oil's calibration, so both tools agree on where the grid is.
    pub fn from_little_oil(path: &Path) -> Result<Calibration> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading little_oil config {}", path.display()))?;
        let config: LittleOil = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let region = config
            .stash_region
            .context("little_oil config has no stash_region — run `little_oil set-region stash`")?;
        let window = config.game_window_region.context(
            "little_oil config has no game_window_region — run `little_oil set-region window`",
        )?;
        let grid = config
            .stash_grid
            .context("little_oil config has no stash_grid — run `little_oil calibrate-stash`")?;
        if grid.cols.len() != QUAD || grid.rows.len() != QUAD {
            bail!(
                "little_oil stash_grid has {}x{} cells; expected the 24x24 quad calibration",
                grid.cols.len(),
                grid.rows.len()
            );
        }
        Ok(Calibration {
            surface: Rect {
                x: region.x,
                y: region.y,
                w: region.width,
                h: region.height,
            },
            origin: (window.x, window.y),
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

    /// Screen rectangle of an item occupying `w`×`h` cells at `(x, y)` in a tab
    /// whose cells span `scale` quad cells (see [`grid_scale`]).
    pub fn cell_rect(&self, x: i64, y: i64, w: i64, h: i64, scale: usize) -> Option<Rect> {
        if x < 0 || y < 0 || w <= 0 || h <= 0 || scale == 0 {
            return None;
        }
        let (x0, y0) = (x as usize * scale, y as usize * scale);
        let (x1, y1) = ((x + w) as usize * scale, (y + h) as usize * scale);
        let left = *self.cols.get(x0)?;
        let top = *self.rows.get(y0)?;
        // The far edge is the next cell's start, or the last cell's start plus
        // a cell at the grid's edge.
        let right = match self.cols.get(x1) {
            Some(next) => *next,
            None => *self.cols.get(x1 - 1)? + self.cell_w,
        };
        let bottom = match self.rows.get(y1) {
            Some(next) => *next,
            None => *self.rows.get(y1 - 1)? + self.cell_h,
        };
        Some(Rect {
            x: self.origin.0 + left as i32,
            y: self.origin.1 + top as i32,
            w: right.saturating_sub(left),
            h: bottom.saturating_sub(top),
        })
    }
}

/// Quad cells per tab cell: 1 for a quad tab, 2 for a normal or premium tab.
/// Other tab types (currency, maps, …) have no plain grid to draw on.
pub fn grid_scale(stash_type: &str) -> Option<usize> {
    match stash_type {
        "QuadStash" => Some(1),
        "NormalStash" | "PremiumStash" => Some(2),
        _ => None,
    }
}

/// Removes the pid file when the overlay exits on its own.
#[cfg(target_os = "linux")]
pub struct PidFile(PathBuf);

#[cfg(target_os = "linux")]
impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Running the overlay again closes the one already on screen (a hotkey
/// toggles it) instead of stacking another surface. `None` means a running
/// overlay was closed and nothing should be drawn.
#[cfg(target_os = "linux")]
pub fn toggle_previous() -> Result<Option<PidFile>> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join("big_oil-overlay.pid");
    let me = std::process::id();
    if let Ok(raw) = std::fs::read_to_string(&path)
        && let Ok(pid) = raw.trim().parse::<u32>()
        && pid != me
        && std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "big_oil")
    {
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .context("running kill")?;
        if !status.success() {
            bail!("could not stop the running overlay (pid {pid})");
        }
        let _ = std::fs::remove_file(&path);
        println!("overlay closed");
        return Ok(None);
    }
    std::fs::write(&path, me.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(PidFile(path)))
}

/// Frames for every item that has a position on the grid.
pub fn marks(
    rows: &[&ItemRow],
    rates: &Rates,
    cal: &Calibration,
    rule: &Rule,
    scale: usize,
) -> Vec<Mark> {
    rows.iter()
        .filter(|row| row.delisted_at.is_none())
        .filter_map(|row| {
            let rect = cal.cell_rect(
                row.x?,
                row.y?,
                row.w.unwrap_or(1),
                row.h.unwrap_or(1),
                scale,
            )?;
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
    game_window_region: Option<RegionJson>,
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
            surface: Rect {
                x: 20,
                y: 30,
                w: 820,
                h: 820,
            },
            origin: (10, 20),
            cell_w: 33,
            cell_h: 34,
            cols: (0..24).map(|i| i * 33).collect(),
            rows: (0..24).map(|i| i * 34).collect(),
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
    fn quad_cells_map_from_the_game_window() {
        assert_eq!(
            cal().cell_rect(2, 3, 1, 1, 1),
            Some(Rect {
                x: 76,
                y: 122,
                w: 33,
                h: 34
            })
        );
    }

    #[test]
    fn premium_cells_span_two_quad_cells() {
        // A 2x3 armour piece at (1,1) in a 12x12 tab.
        assert_eq!(
            cal().cell_rect(1, 1, 2, 3, 2),
            Some(Rect {
                x: 76,
                y: 88,
                w: 132,
                h: 204
            })
        );
        // The last premium cell ends at the grid's far edge.
        assert_eq!(
            cal().cell_rect(11, 11, 1, 1, 2),
            Some(Rect {
                x: 10 + 22 * 33,
                y: 20 + 22 * 34,
                w: 66,
                h: 68
            })
        );
    }

    #[test]
    fn out_of_range_positions_are_ignored() {
        assert!(cal().cell_rect(24, 0, 1, 1, 1).is_none());
        assert!(cal().cell_rect(0, 24, 1, 1, 1).is_none());
        assert!(cal().cell_rect(12, 0, 1, 1, 2).is_none());
        assert!(cal().cell_rect(-1, 0, 1, 1, 1).is_none());
    }

    #[test]
    fn grid_scale_knows_the_plain_grids() {
        assert_eq!(grid_scale("QuadStash"), Some(1));
        assert_eq!(grid_scale("PremiumStash"), Some(2));
        assert_eq!(grid_scale("NormalStash"), Some(2));
        assert_eq!(grid_scale("CurrencyStash"), None);
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
        let marks = marks(&[&hidden, &placed], &rates, &cal(), &Rule::default(), 1);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].rect.y, 20 + 4 * 34);
    }
}
