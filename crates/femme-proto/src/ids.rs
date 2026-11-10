use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// A ULID carried as its 128-bit value (16 bytes on the wire, 26 chars as text). In a
/// human-readable format (JSON, for the phone's WebSocket carrier) it is the 26-char text: a
/// 128-bit number is not something a JSON reader keeps whole.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
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

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            s.serialize_u128(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
            text.parse().map_err(serde::de::Error::custom)
        } else {
            u128::deserialize(d).map(Id)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_in_json_and_number_in_postcard() {
        let id = Id::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<Id>(&json).unwrap(), id);
        assert!(serde_json::from_str::<Id>("\"not a ulid\"").is_err());
        let bytes = postcard::to_stdvec(&id).unwrap();
        assert_eq!(bytes, postcard::to_stdvec(&id.0).unwrap());
        assert_eq!(postcard::from_bytes::<Id>(&bytes).unwrap(), id);
    }
}
