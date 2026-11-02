//! `femme-holder`: the per-session process that owns a session's PTY panes and its headless agent.
//!
//! The binary lives in `src/main.rs` (see the crate README for the socket protocol and lifecycle).
//! This library holds the parts `femmed` also needs: where the holder's files live ([`paths`]),
//! how its socket is framed ([`wire`]) and what a snapshot looks like on disk ([`snap`]).

#![warn(missing_docs)]

pub mod paths;
pub mod snap;
pub mod wire;
