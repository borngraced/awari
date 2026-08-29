//! Bundled fonts, embedded in the binary so launcher text renders the same on
//! any host without the face being installed. Registered once on the bootstrap
//! path; fontdb keeps the `Cow::Borrowed` bytes as an `Arc` (no copy) and gpui
//! caches family → font-id, so the per-keystroke hot path is unaffected.

use std::borrow::Cow;

pub const JETBRAINS_MONO: &str = "JetBrains Mono";

const JETBRAINS_MONO_REGULAR: &[u8] =
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf");

pub fn register(cx: &gpui::App) {
    let fonts = vec![Cow::Borrowed(JETBRAINS_MONO_REGULAR)];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%e, "failed to register embedded fonts; falling back to system fonts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_is_a_ttf() {
        assert_eq!(&JETBRAINS_MONO_REGULAR[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert!(!JETBRAINS_MONO_REGULAR.is_empty());
    }
}
