//! Terminal rendering: glyph-run batching, the gpui grid element, synthetic sources (the row cache
//! is `femme_term::client::RowCache`).

pub mod element;
pub mod input;
pub mod pane;
pub mod runs;
pub mod synth;

pub use pane::{SourceKind, TerminalPane};
