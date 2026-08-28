//! Awa icons. GPUI tints the SVG alpha mask with `text_color`.

use gpui::{Rgba, Styled, px, svg};

#[allow(dead_code)]
pub const ICON_PX: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    AppWindow,
    LayoutGrid,
    File,
    Command,
    Search,
}

impl Icon {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::AppWindow => include_bytes!("../../assets/icons/app_window.svg"),
            Self::LayoutGrid => include_bytes!("../../assets/icons/layout_grid.svg"),
            Self::File => include_bytes!("../../assets/icons/file.svg"),
            Self::Command => include_bytes!("../../assets/icons/command.svg"),
            Self::Search => include_bytes!("../../assets/icons/search.svg"),
        }
    }

    pub fn element_px(self, color: Rgba, size: f32) -> gpui::Svg {
        svg()
            .data(self.bytes())
            .size(px(size))
            .flex_none()
            .text_color(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_icons_are_embedded() {
        for icon in [
            Icon::AppWindow,
            Icon::LayoutGrid,
            Icon::File,
            Icon::Command,
            Icon::Search,
        ] {
            let bytes = icon.bytes();
            assert!(bytes.starts_with(b"<svg"), "{:?}", icon);
        }
    }
}
