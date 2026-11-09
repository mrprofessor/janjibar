//! Bundled fonts: Geist for the interface, `JetBrains Mono` for names and
//! numbers. Both are under the SIL Open Font License 1.1; the license files
//! sit next to the fonts in `assets/fonts`.

use std::borrow::Cow;

use gpui_kit::App;

const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/geist/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/geist/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/geist/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Medium.ttf"),
];

/// Registers the bundled fonts with GPUI's text system.
pub fn register(cx: &mut App) -> gpui_kit::Result<()> {
    cx.text_system()
        .add_fonts(FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect())
}
