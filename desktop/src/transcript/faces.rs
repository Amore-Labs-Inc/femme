//! The bold and italic faces of the body font. `Schibsted Grotesk` ships as one variable file and
//! gpui draws a variable file at its default weight only, so `**bold**`, `*italic*` and headings
//! came out plain. These are static instances cut from the family's own variable files (Google
//! Fonts, `ofl/schibstedgrotesk`): `fonttools varLib.instancer <file> wght=400|700
//! --update-name-table`, the Roman file for Bold and the Italic file for the other two. Same family
//! name, so the text system picks them by weight and style; the licence is `faces/OFL.txt`.

use std::borrow::Cow;
use std::sync::Once;

use gpui::App;

macro_rules! face {
    ($name:literal) => {
        Cow::Borrowed(&include_bytes!(concat!("faces/", $name))[..])
    };
}

/// Registers the faces with the text system, once per process.
pub(super) fn register(cx: &App) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let faces = vec![
            face!("SchibstedGrotesk-Bold.ttf"),
            face!("SchibstedGrotesk-Italic.ttf"),
            face!("SchibstedGrotesk-BoldItalic.ttf"),
        ];
        if let Err(e) = cx.text_system().add_fonts(faces) {
            eprintln!("femme-desktop: transcript fonts: {e}");
        }
    });
}
