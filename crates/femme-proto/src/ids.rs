use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A ULID carried as its 128-bit value (16 bytes on the wire, 26 chars as text).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct Id(pub u128);

impl Id {
    pub fn new() -> Self {
        Id(ulid::Ulid::generate().0)
    }
    pub fn is_nil(self) -> bool {
        self.0 == 0
    }
}
impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", ulid::Ulid(self.0))
    }
}
impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", ulid::Ulid(self.0))
    }
}
impl FromStr for Id {
    type Err = ulid::DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ulid::Ulid::from_string(s).map(|u| Id(u.0))
    }
}

/// Session id.
pub type Sid = Id;
/// Idempotency id of a client request (deduped in the daemon's `ops` table).
pub type OpId = Id;
/// Enrolled device.
pub type DeviceId = Id;

/// A terminal pane inside a session. `no` is the pane number (0 = the agent's pane in TUI mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PaneId {
    pub sid: Sid,
    pub no: u16,
}
