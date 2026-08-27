//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

use gpui::SpringConfig;
#[cfg(test)]
mod open_path_tests;
pub mod scoring;
#[cfg(test)]
mod tests;
pub mod types;
pub mod view;
mod icon_cache;

pub use scoring::*;
pub use types::*;
pub use view::*;

pub const LAUNCHER_W: f32 = 600.0;
pub const LAUNCHER_H: f32 = 1080.0;

/// Pause after the last keystroke before results (and panel height) appear.
const RESULTS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Delay before a typed query is sent to the daemon to run a (potentially heavy)
/// fuzzy file search. Sending on every keystroke re-runs the search per
/// character; this collapses a burst of typing into one search after the user
/// pauses. Commit keys (Enter) flush immediately instead.
const QUERY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Critically damped spring that settles in about `duration_ms`.
fn motion_spring(duration_ms: u32) -> SpringConfig {
    if duration_ms == 0 {
        return SpringConfig::new(20_000.0, 282.0, 1.0);
    }
    let t = (duration_ms as f32 / 1000.0).clamp(0.04, 1.0);
    let omega = 4.2 / t;
    SpringConfig::new(omega * omega, 2.0 * omega, 1.0)
}

const GRID_COLS: usize = 4;
/// Height of one grid row of app tiles: tile py(16)*2 + gap_3(12) + icon(12)
/// + label line(~16) = 72, plus the grid row's p(8)*2 = 88.
const GRID_ROW_H: f32 = 88.0;
/// Fixed height of one list result row. Deterministic single source of truth
/// (icon `ICON_LIST`, 16px title, 12px subtitle, `py(px(11.))`) so the panel
/// height formula always matches the actual rendered rows — no per-keystroke
/// measurement, no clipping, no spurious resize.
const ROW_H: f32 = 60.0;
/// Content height of the "no matches" block (py(28)*2 + ~text).
const NO_MATCH_H: f32 = 76.0;
/// Content height of the empty-state source list (3 rows: Apps / Files /
/// Windows). Inset 6px + 3×row(~36) + 2×gap(2) + 6px ≈ 124; a little slack
/// avoids any clipping when the font metrics push a row a hair taller.
const SOURCE_LIST_H: f32 = 134.0;
/// Minimum scale while the overlay is closed, so it eases in/out from ~92%.
const SCALE_MIN: f32 = 0.92;
const SEARCH_H: f32 = 50.0;
const ICON_LIST: f32 = 30.0;
const ICON_GRID: f32 = 50.0;
