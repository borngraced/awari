//! Launcher palette. Defaults match the overlay concept; every token is KDL-overridable.

use gpui::{Rgba, rgb, rgba};

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
        Self::awari()
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
    #[allow(dead_code)]
    pub fn scrim(&self) -> Rgba {
        self.scrim.to_rgba()
    }
    pub fn ghost(&self) -> Rgba {
        rgba(0x00_00_00_00)
    }
}

impl Theme {
    pub fn awari() -> Self {
        Self {
            accent: Color::rgb(0xb4a0ff),
            accent_dim: Color::rgba(0xb4a0ff33),
            bg: Color::rgb(0x141416),
            panel: Color::rgb(0x1b1b1e),
            raise: Color::rgb(0x232326),
            border: Color::rgb(0x2b2b30),
            text: Color::rgb(0xf2eef9),
            text_dim: Color::rgb(0x8c899b),
            text_faint: Color::rgb(0x57545f),
            scrim: Color::rgba(0x0a0a0be6),
            font: None,
            font_size: None,
        }
    }

    pub fn ash() -> Self {
        Self {
            accent: Color::rgb(0x9aa5b8),
            accent_dim: Color::rgba(0x9aa5b833),
            bg: Color::rgb(0x1c1c1e),
            panel: Color::rgb(0x232326),
            raise: Color::rgb(0x2c2c30),
            border: Color::rgb(0x323236),
            text: Color::rgb(0xe8e6ea),
            text_dim: Color::rgb(0x8a8890),
            text_faint: Color::rgb(0x625f68),
            scrim: Color::rgba(0x0e0e0fe6),
            font: None,
            font_size: None,
        }
    }

    pub fn ember() -> Self {
        Self {
            accent: Color::rgb(0xe0935f),
            accent_dim: Color::rgba(0xe0935f33),
            bg: Color::rgb(0x181310),
            panel: Color::rgb(0x211a16),
            raise: Color::rgb(0x2a211c),
            border: Color::rgb(0x332822),
            text: Color::rgb(0xf3ece4),
            text_dim: Color::rgb(0xa3927f),
            text_faint: Color::rgb(0x6f5f4f),
            scrim: Color::rgba(0x0d0a08e6),
            font: None,
            font_size: None,
        }
    }

    pub fn verdant() -> Self {
        Self {
            accent: Color::rgb(0x7fd996),
            accent_dim: Color::rgba(0x7fd99633),
            bg: Color::rgb(0x10150f),
            panel: Color::rgb(0x161d15),
            raise: Color::rgb(0x1e281c),
            border: Color::rgb(0x243024),
            text: Color::rgb(0xe7f0e6),
            text_dim: Color::rgb(0x8ea38c),
            text_faint: Color::rgb(0x5f7060),
            scrim: Color::rgba(0x090c08e6),
            font: None,
            font_size: None,
        }
    }

    pub fn paper() -> Self {
        Self {
            accent: Color::rgb(0x4a5bc4),
            accent_dim: Color::rgba(0x4a5bc433),
            bg: Color::rgb(0xf2f0ea),
            panel: Color::rgb(0xffffff),
            raise: Color::rgb(0xece8df),
            border: Color::rgb(0xdcd8cd),
            text: Color::rgb(0x231f1a),
            text_dim: Color::rgb(0x7c766a),
            text_faint: Color::rgb(0xa39d8f),
            scrim: Color::rgba(0x000000e6),
            font: None,
            font_size: None,
        }
    }

    pub fn mono() -> Self {
        Self {
            accent: Color::rgb(0xe6e6e6),
            accent_dim: Color::rgba(0xe6e6e633),
            bg: Color::rgb(0x101010),
            panel: Color::rgb(0x171717),
            raise: Color::rgb(0x202020),
            border: Color::rgb(0x2a2a2a),
            text: Color::rgb(0xe6e6e6),
            text_dim: Color::rgb(0x7a7a7a),
            text_faint: Color::rgb(0x4d4d4d),
            scrim: Color::rgba(0x000000e6),
            font: None,
            font_size: None,
        }
    }

    pub fn tokyonight() -> Self {
        Self {
            accent: Color::rgb(0x7aa2f7),
            accent_dim: Color::rgba(0x7aa2f733),
            bg: Color::rgb(0x16161e),
            panel: Color::rgb(0x1a1b26),
            raise: Color::rgb(0x292e42),
            border: Color::rgb(0x414868),
            text: Color::rgb(0xc0caf5),
            text_dim: Color::rgb(0xa9b1d6),
            text_faint: Color::rgb(0x565f89),
            scrim: Color::rgba(0x0f0f14e6),
            font: None,
            font_size: None,
        }
    }

    pub fn catppuccin() -> Self {
        Self {
            accent: Color::rgb(0xcba6f7),
            accent_dim: Color::rgba(0xcba6f733),
            bg: Color::rgb(0x11111b),
            panel: Color::rgb(0x1e1e2e),
            raise: Color::rgb(0x313244),
            border: Color::rgb(0x45475a),
            text: Color::rgb(0xcdd6f4),
            text_dim: Color::rgb(0xa6adc8),
            text_faint: Color::rgb(0x9399b2),
            scrim: Color::rgba(0x08080ce6),
            font: None,
            font_size: None,
        }
    }

    pub fn gruvbox() -> Self {
        Self {
            accent: Color::rgb(0xfe8019),
            accent_dim: Color::rgba(0xfe801933),
            bg: Color::rgb(0x1d2021),
            panel: Color::rgb(0x282828),
            raise: Color::rgb(0x3c3836),
            border: Color::rgb(0x504945),
            text: Color::rgb(0xebdbb2),
            text_dim: Color::rgb(0xa89984),
            text_faint: Color::rgb(0x928374),
            scrim: Color::rgba(0x141617e6),
            font: None,
            font_size: None,
        }
    }

    pub fn preset(name: &str) -> Option<Theme> {
        match name.to_ascii_lowercase().as_str() {
            "awari" => Some(Self::awari()),
            "ash" => Some(Self::ash()),
            "ember" => Some(Self::ember()),
            "verdant" => Some(Self::verdant()),
            "paper" => Some(Self::paper()),
            "mono" => Some(Self::mono()),
            "tokyonight" | "tokyo-night" | "tokyo_night" => Some(Self::tokyonight()),
            "catppuccin" | "catppuccin-mocha" | "mocha" => Some(Self::catppuccin()),
            "gruvbox" => Some(Self::gruvbox()),
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
            Some(Color::rgb(
                (r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b,
            ))
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

    #[test]
    fn presets_resolve_and_default_is_awari() {
        assert_eq!(Theme::default(), Theme::awari());
        for name in [
            "awari", "ash", "ember", "verdant", "paper", "mono", "tokyonight",
            "catppuccin", "gruvbox",
        ] {
            assert!(Theme::preset(name).is_some(), "{name} should resolve");
        }
        assert!(Theme::preset("nope").is_none());
        assert_ne!(Theme::awari().panel, Theme::tokyonight().panel);
        assert_ne!(Theme::tokyonight().panel, Theme::catppuccin().panel);
    }
}
