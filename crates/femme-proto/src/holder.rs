//! Daemon ↔ holder protocol over `$XDG_RUNTIME_DIR/femme/s/<sid>.sock`. Framing is `u32 len`
//! (little-endian) + postcard, no compression. One connection per role: the first message says
//! which (`Hello` = control, `Attach` = a pane view, `SpoolRead` = the agent's stdout tail).

use serde::{Deserialize, Serialize};

use crate::term::{TermDown, TermUp};
use crate::Sid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Pane(u16),
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenFmt {
    Text,
    Ansi,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HolderReq {
    Hello { v: u16 },
    SpawnPty {
        no: u16,
        cmd: Vec<String>,
        cwd: String,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    },
    ClosePane { no: u16 },
    /// Headless agent: stdin kept open, stdout appended to the spool file, stderr tail kept.
    SpawnAgent {
        cmd: Vec<String>,
        cwd: String,
        env: Vec<(String, String)>,
    },
    AgentWrite(Vec<u8>),
    AgentCloseStdin,
    /// The daemon has durably consumed the spool up to `offset`; bytes below may be truncated.
    SpoolCommit { offset: u64 },
    Signal { target: Target, sig: i32 },
    Screen { pane: u16, fmt: ScreenFmt },
    Shutdown,
    /// Pane view role.
    Attach { pane: u16, epoch: u64, have_rev: u64, client_instance: u128 },
    Term(TermUp),
    /// Spool role: stream bytes from `from`, then follow.
    SpoolRead { from: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderPane {
    pub no: u16,
    pub pid: u32,
    pub alive: bool,
    pub exit: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderAgent {
    pub pid: u32,
    pub alive: bool,
    pub exit: Option<i32>,
    /// Lowest byte offset still on disk, and the end of the spool.
    pub spool_lo: u64,
    pub spool_hi: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderInfo {
    pub v: u16,
    pub sid: Sid,
    /// Changes whenever the holder process starts.
    pub epoch: u64,
    pub started: i64,
    pub panes: Vec<HolderPane>,
    pub agent: Option<HolderAgent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HolderEvent {
    ChildExited { target: Target, status: i32 },
    Bell { pane: u16 },
    Title { pane: u16, title: String },
    /// OSC 9 / 777 notifications from the pane.
    Notify { pane: u16, title: String, body: String },
    /// Last stderr bytes of the agent, sent with `ChildExited { Agent }`.
    AgentStderr(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HolderResp {
    Info(HolderInfo),
    Ok,
    Err(String),
    Screen(String),
    Event(HolderEvent),
    Term(TermDown),
    Spool { offset: u64, data: Vec<u8> },
}
