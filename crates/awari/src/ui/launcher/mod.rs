//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

use gpui::SpringConfig;


pub const LAUNCHER_W: f32 = 600.0;
pub const LAUNCHER_H: f32 = 1080.0;

const PANEL_SPRING: SpringConfig = SpringConfig::new(520.0, 48.0, 1.0);
const HEIGHT_SPRING: SpringConfig = SpringConfig::new(520.0, 48.0, 1.0);
const ITEM_HOVER_SPRING: SpringConfig = SpringConfig::new(600.0, 52.0, 1.0);

const GRID_COLS: usize = 4;
const SLIDE: f32 = 10.0;
const SEARCH_H: f32 = 68.0;
const PANEL_H: f32 = 560.0;
const ICON_LIST: f32 = 30.0;
const ICON_GRID: f32 = 50.0;
const AWARI_MARK: &[u8] = include_bytes!("../../../assets/icons/awari_mark.svg");

pub mod types;
pub mod scoring;
pub mod view;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod open_path_tests;

pub use types::*;
pub use scoring::*;
pub use view::*;
/// Thin wrapper kept for tests: scores app/window rows inline (no cache).
#[cfg(test)]
pub fn filter_rows(params: FilterParams) -> Vec<LauncherRow> {
    let prefix = command_prefix(params.query);
    let calc = crate::math::evaluate(params.query);
    filter_rows_cached(FilterParams {
        prefix,
        calc,
        ..params
    })
}
