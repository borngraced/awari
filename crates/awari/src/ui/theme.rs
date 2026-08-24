//! Launcher palette. Defaults match the overlay concept; every token is KDL-overridable.

use gpui::{rgb, rgba, Rgba};

/// Packed `0xRRGGBBAA`. Six-digit hex is stored with `AA = FF`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(hex: u32) -> Self {
        Self((hex << 8) | 0xff)
    }

    pub const fn rgba(hex: u32) -> Self {
        Self(hex)
    }

    pub fn to_rgba(&self) -> Rgba {
        let v = self.0;
        if v & 0xff == 0xff {
            rgb(v >> 8)
        } else {
            rgba(v)
        }
    }
}

/// Concept tokens (`--accent`, `--panel`, …). Unknown KDL keys are ignored.
///
/// `font` is a system family name resolved through fontdb (empty = GPUI's
/// `.SystemUIFont` default); `font_size` overrides the rem size in px.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub accent_dim: Color,
    pub bg: Color,
    pub panel: Color,
    pub raise: Color,
    pub border: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_faint: Color,
    pub scrim: Color,
    pub font: Option<String>,
    pub font_size: Option<u32>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::rgb(0x8b_7b_f0),
            accent_dim: Color::rgba(0x8b_7b_f0_24),
            bg: Color::rgb(0x0b_0b_0c),
            panel: Color::rgb(0x14_14_16_u32),
            raise: Color::rgba(0xff_ff_ff_0b),
            border: Color::rgba(0xff_ff_ff_12),
            text: Color::rgb(0xec_ea_f0),
            text_dim: Color::rgb(0x8b_89_94),
            text_faint: Color::rgb(0x57_55_5f),
            scrim: Color::rgba(0x0b_0b_0c_b3),
            font: None,
            font_size: None,
        }
    }
}

impl Theme {
    pub fn accent(&self) -> Rgba {
        self.accent.to_rgba()
    }
    pub fn select(&self) -> Rgba {
        self.accent_dim.to_rgba()
    }
    #[allow(dead_code)]
    pub fn bg(&self) -> Rgba {
        self.bg.to_rgba()
    }
    pub fn panel(&self) -> Rgba {
        self.panel.to_rgba()
    }
    pub fn hover(&self) -> Rgba {
        self.raise.to_rgba()
    }
    pub fn surface(&self) -> Rgba {
        self.raise.to_rgba()
    }
    pub fn border(&self) -> Rgba {
        self.border.to_rgba()
    }
    pub fn fg(&self) -> Rgba {
        self.text.to_rgba()
    }
    pub fn muted(&self) -> Rgba {
        self.text_dim.to_rgba()
    }
    pub fn faint(&self) -> Rgba {
        self.text_faint.to_rgba()
    }
    pub fn scrim(&self) -> Rgba {
        self.scrim.to_rgba()
    }
    pub fn ghost(&self) -> Rgba {
        rgba(0x00_00_00_00)
    }
}

impl Theme {
    /// Gruvbox (dark) palette. Any token can still be overridden via `theme { … }` in config.
    pub fn gruvbox() -> Self {
        Self {
            accent: Color::rgb(0xfe_80_19),
            accent_dim: Color::rgba(0xfe_80_19_2e),
            bg: Color::rgb(0x28_28_28),
            panel: Color::rgb(0x3c_38_36),
            raise: Color::rgba(0x50_49_45_ff),
            border: Color::rgba(0x50_49_45_ff),
            text: Color::rgb(0xeb_db_b2),
            text_dim: Color::rgb(0xa8_99_84),
            text_faint: Color::rgb(0x7c_6f_64),
            scrim: Color::rgba(0x00_00_00_b3),
            font: None,
            font_size: None,
        }
    }

    /// Gruvbox (light) palette.
    pub fn gruvbox_light() -> Self {
        Self {
            accent: Color::rgb(0xaf_3a_03),
            accent_dim: Color::rgba(0xaf_3a_03_2e),
            bg: Color::rgb(0xfb_f1_c7),
            panel: Color::rgb(0xeb_db_b2),
            raise: Color::rgb(0xd5_c4_a1),
            border: Color::rgb(0xd5_c4_a1),
            text: Color::rgb(0x3c_38_36),
            text_dim: Color::rgb(0x7c_6f_64),
            text_faint: Color::rgb(0x92_83_74),
            scrim: Color::rgba(0x00_00_00_99),
            font: None,
            font_size: None,
        }
    }

    /// Look up a built-in named preset (case-insensitive). Unknown names return `None`.
    pub fn preset(name: &str) -> Option<Theme> {
        match name.to_ascii_lowercase().as_str() {
            "gruvbox" => Some(Self::gruvbox()),
            "gruvbox-light" | "gruvbox_light" => Some(Self::gruvbox_light()),
            _ => None,
        }
    }
}

/// `#RGB`, `#RRGGBB`, `#RRGGBBAA`. No `url()`, no named CSS colors.
pub fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        3 => {
            let n = u32::from_str_radix(s, 16).ok()?;
            let r = (n >> 8) & 0xf;
            let g = (n >> 4) & 0xf;
            let b = n & 0xf;
            Some(Color::rgb((r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b))
        }
        6 => Some(Color::rgb(u32::from_str_radix(s, 16).ok()?)),
        8 => Some(Color::rgba(u32::from_str_radix(s, 16).ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_rgb_and_rgba() {
        assert_eq!(parse_hex("#8b7bf0"), Some(Color::rgb(0x8b7bf0)));
        assert_eq!(parse_hex("8b7bf024"), Some(Color::rgba(0x8b7bf024)));
        assert_eq!(parse_hex("#fff"), Some(Color::rgb(0xffffff)));
        assert!(parse_hex("red").is_none());
    }
}
