//! The drawing board: an Excalidraw-style canvas over the centre for quick diagrams that go straight
//! to Claude. Owned by the board agent (docs/desktop-ui.md §board).

pub mod drawlist;
pub mod editor;
pub mod export;
pub mod font;
pub mod geom;
pub mod geometry;
pub mod palette;
pub mod scene;
pub mod store;
pub mod stroke;
pub mod textedit;
pub mod view;
