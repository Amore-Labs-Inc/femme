//! The on-disk snapshot: what the holder knew last, for showing a "last screen" after a reboot
//! killed the session. Written at most every 30 s (and on `Shutdown`) to `<state>/snap/<sid>.snap`,
//! atomically (temp file + rename), as a bare postcard-encoded [`Snapshot`] (no header, no
//! compression). Postcard structs cannot grow fields compatibly, so a reader that cannot decode a
//! file should treat it as "no snapshot".

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use femme_proto::term::RowPatch;
use femme_proto::Sid;
use serde::{Deserialize, Serialize};

/// Everything the holder remembers about one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The session this holder served.
    pub sid: Sid,
    /// Unix ms when the file was written.
    pub written: i64,
    /// One entry per pane that existed at the time (closed panes are gone).
    pub panes: Vec<SnapPane>,
    /// Whether the headless agent was alive when the snapshot was taken.
    pub agent_alive: bool,
}

/// One pane's last known state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapPane {
    /// Pane number (`SpawnPty::no`).
    pub no: u16,
    /// Last title the program set (empty if none).
    pub title: String,
    /// The visible screen as plain text, as `Screen { fmt: Text }` returns it.
    pub screen_text: String,
    /// The last (up to) 200 rows ending at the bottom of the visible screen, scrollback first.
    pub last_200_rows: Vec<RowPatch>,
}

impl Snapshot {
    /// The postcard encoding written to disk.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("a snapshot always serializes")
    }

    /// Inverse of [`Snapshot::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }

    /// Write to `path` atomically: a temp file next to it (mode 0600), then `rename`.
    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("snap.tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&self.encode())?;
        drop(f);
        std::fs::rename(&tmp, path)
    }

    /// Read and decode a snapshot file.
    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::decode(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            sid: "01J00000000000000000000000".parse().unwrap(),
            written: 1_762_000_000_000,
            panes: vec![
                SnapPane {
                    no: 0,
                    title: "vim".into(),
                    screen_text: "hello\nworld\n".into(),
                    last_200_rows: vec![RowPatch { abs: 41, wrapped: false, runs: vec![(0, "hello".into())], wide_cols: vec![] }],
                },
                SnapPane { no: 3, title: String::new(), screen_text: String::new(), last_200_rows: vec![] },
            ],
            agent_alive: true,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let s = sample();
        assert_eq!(Snapshot::decode(&s.encode()).unwrap(), s);
        assert!(Snapshot::decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn write_is_atomic_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.snap");
        let s = sample();
        s.write_atomic(&path).unwrap();
        assert_eq!(Snapshot::read(&path).unwrap(), s);
        assert!(!path.with_extension("snap.tmp").exists(), "temp file must be renamed away");
        let mut s2 = s.clone();
        s2.agent_alive = false;
        s2.write_atomic(&path).unwrap();
        assert_eq!(Snapshot::read(&path).unwrap(), s2);
    }
}
