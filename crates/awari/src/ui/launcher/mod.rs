//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

mod icon_cache;
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
/// Wider panel used for the Apps category (horizontal icon strip).
pub const APP_W: f32 = 760.0;
pub const LAUNCHER_H: f32 = 1080.0;

const RESULTS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);
const QUERY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

const GRID_COLS: usize = 4;
const GRID_ROW_H: f32 = 88.0;
const ROW_H: f32 = 60.0;
const NO_MATCH_H: f32 = 76.0;
const SOURCE_LIST_H: f32 = 104.0;
const SCALE_MIN: f32 = 0.92;
const SEARCH_H: f32 = 50.0;
const ICON_LIST: f32 = 30.0;
const ICON_GRID: f32 = 50.0;
pub const STRIP_ICON: f32 = 56.0;
