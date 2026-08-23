//! Zed Lucide icons. GPUI tints the SVG alpha mask with `text_color`.

use gpui::{px, svg, Rgba, Styled};

pub const ICON_PX: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    AppWindow,
    LayoutGrid,
    File,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Self::Search => "icons/search.svg",
            Self::AppWindow => "icons/app_window.svg",
            Self::LayoutGrid => "icons/layout_grid.svg",
            Self::File => "icons/file.svg",
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Search => include_bytes!("../../assets/icons/search.svg"),
            Self::AppWindow => include_bytes!("../../assets/icons/app_window.svg"),
            Self::LayoutGrid => include_bytes!("../../assets/icons/layout_grid.svg"),
            Self::File => include_bytes!("../../assets/icons/file.svg"),
        }
    }

    pub fn element(self, color: Rgba) -> gpui::Svg {
        svg()
            .data(self.bytes())
            .size(px(ICON_PX))
            .flex_none()
            .text_color(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_icons_are_embedded() {
        for icon in [Icon::Search, Icon::AppWindow, Icon::LayoutGrid] {
            let bytes = icon.bytes();
            assert!(bytes.starts_with(b"<svg"), "{}", icon.path());
        }
    }
}
