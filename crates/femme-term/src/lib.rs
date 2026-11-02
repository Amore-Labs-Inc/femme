//! Terminal state for FEMME: the server-side emulator wrapper and the client-side row cache.
//!
//! - [`client::RowCache`] (always built, no alacritty dependency) applies [`femme_proto::term::Frame`]s
//!   and answers "what is on screen" for the desktop and the phone.
//! - `Emulator` (feature `emulator`, default) wraps `alacritty_terminal` inside the per-session
//!   holder: it parses PTY output, numbers rows absolutely, stamps every row with the rev it last
//!   changed at, and turns that into incremental frames per attached `View`.
//!
//! The invariant everything is tested against: after applying every frame a view receives, the
//! client's visible screen equals the server's, byte for byte (`RowCache::text()` ==
//! `Emulator::screen_text()`, same cursor, modes, title, styles).

#![warn(missing_docs)]

pub mod client;

#[cfg(feature = "emulator")]
pub mod emu;
#[cfg(feature = "emulator")]
pub use emu::{EmuEvent, Emulator, View};
