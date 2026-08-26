//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

use gpui::SpringConfig;
#[cfg(test)]
mod open_path_tests;
pub mod scoring;
#[cfg(test)]
mod tests;
pub mod types;
pub mod view;

pub use scoring::*;
pub use types::*;
pub use view::*;

pub const LAUNCHER_W: f32 = 600.0;
pub const LAUNCHER_H: f32 = 1080.0;

const PANEL_SPRING: SpringConfig = SpringConfig::new(520.0, 48.0, 1.0);
const HEIGHT_SPRING: SpringConfig = SpringConfig::new(520.0, 48.0, 1.0);
/// Spring driving list scroll-to-selection: near-critical, slight overshoot for
/// a buttery glide that retargets without restarting.
const SCROLL_SPRING: SpringConfig = SpringConfig::new(300.0, 32.0, 1.0);
/// Settled tolerance (px) for the scroll spring.
const SCROLL_EPSILON: f32 = 0.5;

const GRID_COLS: usize = 4;
/// Height of one grid row of app tiles: tile py(16)*2 + gap_3(12) + icon(12)
/// + label line(~16) = 72, plus the grid row's p(8)*2 = 88.
const GRID_ROW_H: f32 = 88.0;
/// Fixed height of one list result row. Deterministic single source of truth
/// (icon `ICON_LIST`, 16px title, 12px subtitle, `py(px(11.))`) so the panel
/// height formula always matches the actual rendered rows — no per-keystroke
/// measurement, no clipping, no spurious resize.
const ROW_H: f32 = 60.0;
/// Breathing room below the last row so one (or a few) results are fully
/// visible with slack — absorbs the sub-pixel overflow that would otherwise
/// let the list scroll a hair.
const LIST_BREATH: f32 = 8.0;
/// Content height of the "no matches" block (py(28)*2 + ~text).
const NO_MATCH_H: f32 = 76.0;
/// Minimum scale while the overlay is closed, so it eases in/out from ~96%.
const SCALE_MIN: f32 = 0.96;
const SEARCH_H: f32 = 50.0;
const ICON_LIST: f32 = 30.0;
const ICON_GRID: f32 = 50.0;
