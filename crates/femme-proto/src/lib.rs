//! FEMME wire protocol: the types every peer agrees on, and the codec that carries them.
//!
//! Rules (see the plan, §6):
//! - postcard everywhere. Enums are **append-only**: add variants at the end, never reorder or
//!   remove, so a peer one version behind still decodes what it knows.
//! - [`PROTO_VERSION`] travels in `Hello`; a server accepts the current and the previous version.
//! - No IO here. Framing is a pure byte transform ([`codec`]).

pub mod codec;
pub mod holder;
pub mod ids;
pub mod rpc;
pub mod state;
pub mod term;

pub use ids::{DeviceId, Id, OpId, PaneId, Sid};

/// Desktop ↔ daemon protocol version. Bump on any incompatible change; keep the old decoder for one release.
pub const PROTO_VERSION: u16 = 1;
/// Daemon ↔ holder protocol version (local unix socket).
pub const HOLDER_VERSION: u16 = 1;
/// QUIC ALPN.
pub const ALPN: &[u8] = b"femme/1";
