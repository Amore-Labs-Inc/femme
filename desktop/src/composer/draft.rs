//! What the composer remembers per session: the typed text, the attachment chips with where each
//! upload stands, and the last notice. Pure state plus its rules (the chip state machine, what may
//! be sent), so it is unit-tested without a window.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use femme_proto::rpc::{Attachment, MAX_BLOB_PUT};
use femme_proto::state::Agent;
use femme_proto::Sid;
use gpui::Image;

use super::sniff;

/// Names a chip across async replies; unique for the life of the composer.
pub type ChipId = u64;

/// Where one attachment's upload stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upload {
    /// `Req::BlobPut` is in flight.
    Uploading,
    /// The daemon stored it; the prompt names it by this hash.
    Ready([u8; 32]),
    /// It failed; the text is the reason, shown on hover.
    Failed(String),
}

/// One attached file.
pub struct Chip {
    /// Matches a reply to its chip.
    pub id: ChipId,
    /// The name the daemon files it under; unique within the draft.
    pub name: String,
    /// The bytes, kept so a failed upload can be retried and the preview can show them.
    pub data: Arc<Vec<u8>>,
    /// Pixel size from the header, when that is cheap to read.
    pub dims: Option<(u32, u32)>,
    /// The picture, when the bytes are one gpui can show: built once so gpui's asset cache holds a
    /// single entry per chip.
    pub image: Option<Arc<Image>>,
    /// Where the upload stands.
    pub upload: Upload,
}

/// The most files one prompt may name; the daemon refuses more (`femmed`'s `uploads.rs`).
pub const MAX_CHIPS: usize = 16;

/// Why a file was not attached.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Over `MAX_BLOB_PUT`.
    TooBig {
        /// The file's name.
        name: String,
        /// Its size in bytes.
        len: u64,
    },
    /// Nothing in it.
    Empty {
        /// The file's name.
        name: String,
    },
    /// A directory was dropped.
    Folder {
        /// The directory's name.
        name: String,
    },
    /// The draft already has [`MAX_CHIPS`] files.
    TooMany {
        /// The file's name.
        name: String,
    },
    /// The file could not be read.
    Unreadable {
        /// The file's name.
        name: String,
        /// What the system said.
        why: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::TooBig { name, len } => write!(
                f,
                "{name} is {}: attachments are limited to {}",
                sniff::human_size(*len as usize),
                sniff::human_size(MAX_BLOB_PUT)
            ),
            Refusal::Empty { name } => write!(f, "{name} is empty"),
            Refusal::Folder { name } => write!(f, "{name} is a folder: attach files"),
            Refusal::TooMany { name } => {
                write!(
                    f,
                    "{name} not attached: at most {MAX_CHIPS} files to a prompt"
                )
            }
            Refusal::Unreadable { name, why } => write!(f, "can't read {name}: {why}"),
        }
    }
}

/// The most refusals one line spells out before it counts the rest.
const NOTICE_NAMES: usize = 3;

/// One line for everything a drop or paste refused, the reasons in order; `None` when it refused
/// nothing. Past the third reason it says how many more there were.
pub fn join_notices(mut lines: Vec<String>) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let more = lines.len().saturating_sub(NOTICE_NAMES);
    lines.truncate(NOTICE_NAMES);
    let line = lines.join("; ");
    Some(match more {
        0 => line,
        n => format!("{line}; and {n} more"),
    })
}

/// The line for a file picker that would not open. On Linux gpui asks the desktop portal, which a
/// session without a desktop lacks; say what to do instead of quoting the D-Bus error.
pub fn picker_notice(why: &str) -> String {
    if why.contains("portal") {
        "no file picker here: drop or paste files instead".into()
    } else {
        format!("can't open the file picker: {why}")
    }
}

/// What the composer is attached to; decides whether it takes input and what its placeholder says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// No connection to a daemon.
    Offline,
    /// No session is selected.
    NoSession,
    /// A shell: it has a terminal, not a prompt.
    Shell,
    /// An agent session, which takes prompts.
    Agent(Agent),
}

impl Target {
    /// The target for a daemon link (`connected`) and the agent of the selected session, if any.
    pub fn of(connected: bool, session: Option<Agent>) -> Target {
        match (connected, session) {
            (false, _) => Target::Offline,
            (true, None) => Target::NoSession,
            (true, Some(Agent::Shell)) => Target::Shell,
            (true, Some(agent)) => Target::Agent(agent),
        }
    }

    /// Whether a prompt can go to it.
    pub fn takes_prompts(self) -> bool {
        matches!(self, Target::Agent(_))
    }

    /// The input's placeholder: what to type, or why nothing can be typed.
    pub fn placeholder(self) -> &'static str {
        match self {
            Target::Offline => "not connected",
            Target::NoSession => "no session open",
            Target::Shell => "this is a shell: type in the terminal",
            Target::Agent(Agent::Codex) => "message codex",
            Target::Agent(Agent::Pi) => "message pi",
            Target::Agent(_) => "message claude",
        }
    }
}

/// Why a send did not go out.
#[derive(Debug, PartialEq, Eq)]
pub enum Block {
    /// The session does not take prompts (the input says why).
    Target(Target),
    /// The previous send has not been answered yet.
    Busy,
    /// Nothing typed and nothing attached.
    Empty,
    /// This many uploads have not finished.
    Uploading(usize),
    /// This many uploads failed: remove them or retry.
    Failed(usize),
}

/// What the daemon's error says when a prompt names an upload it no longer has: it sweeps the blobs
/// no item refers to, at start and daily (`femmed`'s `uploads.rs`, "its upload is gone").
pub const UPLOAD_GONE: &str = "its upload is gone";

/// The end of the line a send blocked on unfinished uploads shows...
const WAIT: &str = "still uploading: wait";
/// ...and the end of the one for failed uploads.
const FAILED: &str = "failed: retry or remove";

fn attachments(n: usize) -> String {
    match n {
        1 => "1 attachment".into(),
        n => format!("{n} attachments"),
    }
}

impl Block {
    /// The line to show under the input; `None` when the block is silent (the input, the button or
    /// the empty box already say it).
    pub fn notice(&self) -> Option<String> {
        match self {
            Block::Target(_) | Block::Busy | Block::Empty => None,
            Block::Uploading(n) => Some(format!("{} {WAIT}", attachments(*n))),
            Block::Failed(n) => Some(format!("{} {FAILED}", attachments(*n))),
        }
    }
}

/// A prompt cleared to go out.
#[derive(Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The text, trailing whitespace trimmed.
    pub text: String,
    /// The attachments in chip order.
    pub attachments: Vec<Attachment>,
    /// The chips they came from, to remove once the daemon has the prompt.
    pub ids: Vec<ChipId>,
}

/// One session's draft.
#[derive(Default)]
pub struct Draft {
    /// What was typed, parked here while another session is current (the live text is in the input).
    pub text: String,
    /// The attached files, left to right.
    pub chips: Vec<Chip>,
    /// The last refusal or send error, shown under the input until the next edit.
    pub notice: Option<String>,
    /// A `Req::Prompt` is in flight.
    pub sending: bool,
}

impl Draft {
    /// Nothing worth keeping: dropping the draft loses nothing and no reply is waiting for it.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.chips.is_empty() && self.notice.is_none() && !self.sending
    }

    /// Attaches `data` as `name` (renamed if the draft already has that name) and starts it
    /// `Uploading`. Over-size and empty files are refused, and so is any past [`MAX_CHIPS`].
    pub fn add(&mut self, id: ChipId, name: &str, data: Vec<u8>) -> Result<&Chip, Refusal> {
        if self.chips.len() >= MAX_CHIPS {
            return Err(Refusal::TooMany { name: name.into() });
        }
        if data.is_empty() {
            return Err(Refusal::Empty { name: name.into() });
        }
        if data.len() > MAX_BLOB_PUT {
            return Err(Refusal::TooBig {
                name: name.into(),
                len: data.len() as u64,
            });
        }
        let format = sniff::sniff(&data);
        let dims = format.and_then(|f| sniff::dimensions(&data, f));
        let image = format.map(|f| Arc::new(Image::from_bytes(f, data.clone())));
        self.chips.push(Chip {
            id,
            name: self.unique_name(name),
            data: Arc::new(data),
            dims,
            image,
            upload: Upload::Uploading,
        });
        Ok(self.chips.last().expect("just pushed"))
    }

    /// `name`, or `stem (2).ext`, `stem (3).ext`, ... when a chip already has it.
    fn unique_name(&self, name: &str) -> String {
        let taken = |n: &str| self.chips.iter().any(|c| c.name == n);
        if !taken(name) {
            return name.into();
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
            _ => (name, String::new()),
        };
        (2..)
            .map(|n| format!("{stem} ({n}){ext}"))
            .find(|n| !taken(n))
            .expect("an unbounded range")
    }

    /// The chip with this id.
    pub fn chip(&self, id: ChipId) -> Option<&Chip> {
        self.chips.iter().find(|c| c.id == id)
    }

    fn chip_mut(&mut self, id: ChipId) -> Option<&mut Chip> {
        self.chips.iter_mut().find(|c| c.id == id)
    }

    /// `Uploading` to `Ready`. False, and nothing changes, when the chip is gone or already answered
    /// (a late reply for a chip that was removed, or retried).
    pub fn uploaded(&mut self, id: ChipId, hash: [u8; 32]) -> bool {
        let moved = match self.chip_mut(id) {
            Some(c) if c.upload == Upload::Uploading => {
                c.upload = Upload::Ready(hash);
                true
            }
            _ => false,
        };
        self.settle();
        moved
    }

    /// `Uploading` to `Failed`, with the reason. False when the chip is gone or already answered.
    pub fn failed(&mut self, id: ChipId, why: String) -> bool {
        let moved = match self.chip_mut(id) {
            Some(c) if c.upload == Upload::Uploading => {
                c.upload = Upload::Failed(why);
                true
            }
            _ => false,
        };
        self.settle();
        moved
    }

    /// `Failed` back to `Uploading`. False when the chip is not failed.
    pub fn retry(&mut self, id: ChipId) -> bool {
        let moved = match self.chip_mut(id) {
            Some(c) if matches!(c.upload, Upload::Failed(_)) => {
                c.upload = Upload::Uploading;
                true
            }
            _ => false,
        };
        self.settle();
        moved
    }

    /// `Ready` back to `Uploading`, for an upload the daemon no longer has. False when the chip is
    /// not ready.
    pub fn expire(&mut self, id: ChipId) -> bool {
        match self.chip_mut(id) {
            Some(c) if matches!(c.upload, Upload::Ready(_)) => {
                c.upload = Upload::Uploading;
                true
            }
            _ => false,
        }
    }

    /// Takes the chip off the draft.
    pub fn remove(&mut self, id: ChipId) -> Option<Chip> {
        let at = self.chips.iter().position(|c| c.id == id)?;
        let chip = self.chips.remove(at);
        self.settle();
        Some(chip)
    }

    /// Takes down a "wait" or "failed" line once what it complains about is over; any other
    /// notice (a refused file, a send error) stays until the next edit.
    fn settle(&mut self) {
        let uploading = self.chips.iter().any(|c| c.upload == Upload::Uploading);
        let failed = self
            .chips
            .iter()
            .any(|c| matches!(c.upload, Upload::Failed(_)));
        let over = match self.notice.as_deref() {
            Some(line) if line.ends_with(WAIT) => !uploading,
            Some(line) if line.ends_with(FAILED) => !failed,
            _ => false,
        };
        if over {
            self.notice = None;
        }
    }

    /// What may be sent now with `text`, or why not.
    pub fn gate(&self, text: &str, target: Target) -> Result<Outgoing, Block> {
        if !target.takes_prompts() {
            return Err(Block::Target(target));
        }
        if self.sending {
            return Err(Block::Busy);
        }
        let count = |f: fn(&Upload) -> bool| self.chips.iter().filter(|c| f(&c.upload)).count();
        let uploading = count(|u| matches!(u, Upload::Uploading));
        if uploading > 0 {
            return Err(Block::Uploading(uploading));
        }
        let failed = count(|u| matches!(u, Upload::Failed(_)));
        if failed > 0 {
            return Err(Block::Failed(failed));
        }
        if text.trim().is_empty() && self.chips.is_empty() {
            return Err(Block::Empty);
        }
        let mut out = Outgoing {
            text: text.trim_end().to_string(),
            attachments: Vec::new(),
            ids: Vec::new(),
        };
        for c in &self.chips {
            if let Upload::Ready(blob) = c.upload {
                out.attachments.push(Attachment {
                    name: c.name.clone(),
                    blob,
                });
                out.ids.push(c.id);
            }
        }
        Ok(out)
    }

    /// The daemon has the prompt: takes off the chips that went out (ones added meanwhile stay) and
    /// hands them back, for the caller to let go of what they held.
    pub fn sent(&mut self, ids: &[ChipId]) -> Vec<Chip> {
        let (gone, kept) = std::mem::take(&mut self.chips)
            .into_iter()
            .partition(|c| ids.contains(&c.id));
        self.chips = kept;
        gone
    }
}

/// The drafts of every session the composer has been attached to.
#[derive(Default)]
pub struct Drafts {
    map: HashMap<Sid, Draft>,
    next: ChipId,
}

impl Drafts {
    /// A fresh chip id.
    pub fn new_id(&mut self) -> ChipId {
        self.next += 1;
        self.next
    }

    /// The session's draft, if it has one.
    pub fn get(&self, sid: Sid) -> Option<&Draft> {
        self.map.get(&sid)
    }

    /// The session's draft if it has one: for a late reply, which must not conjure a draft up.
    pub fn get_mut(&mut self, sid: Sid) -> Option<&mut Draft> {
        self.map.get_mut(&sid)
    }

    /// The session's draft, created empty on first use.
    pub fn entry(&mut self, sid: Sid) -> &mut Draft {
        self.map.entry(sid).or_default()
    }

    /// Parks what the session has typed so far; a draft left with nothing in it is dropped.
    pub fn park(&mut self, sid: Sid, text: String) {
        self.map.entry(sid).or_default().text = text;
        self.prune(sid);
    }

    /// Drops the session's draft when nothing is left in it.
    pub fn prune(&mut self, sid: Sid) {
        if self.map.get(&sid).is_some_and(Draft::is_empty) {
            self.map.remove(&sid);
        }
    }

    /// What the session had typed.
    pub fn text(&self, sid: Sid) -> &str {
        self.map.get(&sid).map_or("", |d| d.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::Id;
    use gpui::ImageFormat;

    const A: Sid = Id(1);
    const B: Sid = Id(2);

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend(64u32.to_be_bytes());
        v.extend(32u32.to_be_bytes());
        v.extend([8, 6, 0, 0, 0]);
        v
    }

    fn agent() -> Target {
        Target::Agent(Agent::Claude)
    }

    fn add(d: &mut Draft, id: ChipId, name: &str, data: &[u8]) {
        d.add(id, name, data.to_vec()).unwrap();
    }

    #[test]
    fn a_chip_goes_from_added_to_uploading_to_ready_to_removed() {
        let mut d = Draft::default();
        let chip = d.add(1, "shot.png", png()).unwrap();
        assert_eq!(chip.upload, Upload::Uploading);
        assert_eq!(chip.dims, Some((64, 32)));
        assert_eq!(
            chip.image.as_ref().map(|i| i.format),
            Some(ImageFormat::Png)
        );

        assert!(d.uploaded(1, [7; 32]));
        assert_eq!(d.chip(1).unwrap().upload, Upload::Ready([7; 32]));
        assert_eq!(d.remove(1).unwrap().name, "shot.png");
        assert!(d.chips.is_empty());
        assert!(d.remove(1).is_none());
    }

    #[test]
    fn a_failed_upload_can_be_retried() {
        let mut d = Draft::default();
        add(&mut d, 1, "notes.txt", b"hello");
        assert!(d.chip(1).unwrap().image.is_none());

        assert!(d.failed(1, "Unsupported: not served yet".into()));
        assert_eq!(
            d.chip(1).unwrap().upload,
            Upload::Failed("Unsupported: not served yet".into())
        );
        assert!(!d.retry(2), "no such chip");
        assert!(d.retry(1));
        assert_eq!(d.chip(1).unwrap().upload, Upload::Uploading);
        assert!(!d.retry(1), "only a failed chip is retried");
        assert!(d.uploaded(1, [1; 32]));
    }

    #[test]
    fn late_and_stray_replies_change_nothing() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.txt", b"a");
        assert!(d.uploaded(1, [1; 32]));
        // A second answer for an upload that already finished.
        assert!(!d.uploaded(1, [2; 32]));
        assert!(!d.failed(1, "late".into()));
        assert_eq!(d.chip(1).unwrap().upload, Upload::Ready([1; 32]));
        // A reply for a chip that was removed.
        d.remove(1);
        assert!(!d.uploaded(1, [1; 32]));
        assert!(!d.failed(1, "late".into()));
    }

    #[test]
    fn empty_and_oversize_files_are_refused() {
        let mut d = Draft::default();
        assert_eq!(
            d.add(1, "nothing", vec![]).err(),
            Some(Refusal::Empty {
                name: "nothing".into()
            })
        );
        let err = d
            .add(2, "big.bin", vec![0; MAX_BLOB_PUT + 1])
            .err()
            .unwrap();
        assert_eq!(
            err,
            Refusal::TooBig {
                name: "big.bin".into(),
                len: MAX_BLOB_PUT as u64 + 1
            }
        );
        assert_eq!(
            err.to_string(),
            "big.bin is 8.0 MB: attachments are limited to 8.0 MB"
        );
        assert!(d.chips.is_empty());
        // Exactly the cap is fine.
        assert!(d.add(3, "just-fits.bin", vec![0; MAX_BLOB_PUT]).is_ok());
    }

    #[test]
    fn the_same_name_twice_gets_a_suffix() {
        let mut d = Draft::default();
        add(&mut d, 1, "shot.png", b"1");
        add(&mut d, 2, "shot.png", b"2");
        add(&mut d, 3, "shot.png", b"3");
        add(&mut d, 4, "Makefile", b"4");
        add(&mut d, 5, "Makefile", b"5");
        add(&mut d, 6, ".env", b"6");
        add(&mut d, 7, ".env", b"7");
        let names: Vec<_> = d.chips.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "shot.png",
                "shot (2).png",
                "shot (3).png",
                "Makefile",
                "Makefile (2)",
                ".env",
                ".env (2)"
            ]
        );
    }

    #[test]
    fn only_a_session_that_takes_prompts_may_send() {
        let d = Draft::default();
        for t in [Target::Offline, Target::NoSession, Target::Shell] {
            assert_eq!(d.gate("hi", t), Err(Block::Target(t)));
        }
        assert_eq!(Target::of(false, Some(Agent::Claude)), Target::Offline);
        assert_eq!(Target::of(true, None), Target::NoSession);
        assert_eq!(Target::of(true, Some(Agent::Shell)), Target::Shell);
        assert_eq!(Target::of(true, Some(Agent::Pi)), Target::Agent(Agent::Pi));
        assert!(Target::of(true, Some(Agent::Codex)).takes_prompts());
        assert!(!Target::Shell.takes_prompts());
        assert_eq!(
            Target::Shell.placeholder(),
            "this is a shell: type in the terminal"
        );
        assert_eq!(agent().placeholder(), "message claude");
        assert_eq!(Target::Agent(Agent::Codex).placeholder(), "message codex");
    }

    #[test]
    fn text_alone_sends_and_trailing_whitespace_goes() {
        let d = Draft::default();
        let out = d.gate("fix the bug \n\n", agent()).unwrap();
        assert_eq!(out.text, "fix the bug");
        assert!(out.attachments.is_empty());
        assert_eq!(d.gate("  \n\t", agent()), Err(Block::Empty));
        assert_eq!(d.gate("", agent()), Err(Block::Empty));
    }

    #[test]
    fn an_unfinished_or_failed_upload_blocks_the_send() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.png", b"a");
        add(&mut d, 2, "b.png", b"b");
        add(&mut d, 3, "c.png", b"c");
        assert_eq!(d.gate("look", agent()), Err(Block::Uploading(3)));
        assert!(d.uploaded(1, [1; 32]));
        assert!(d.failed(2, "boom".into()));
        // One is still going: that is reported before the failure.
        assert_eq!(d.gate("look", agent()), Err(Block::Uploading(1)));
        assert!(d.uploaded(3, [3; 32]));
        assert_eq!(d.gate("look", agent()), Err(Block::Failed(1)));
        assert_eq!(
            Block::Failed(1).notice().unwrap(),
            "1 attachment failed: retry or remove"
        );
        assert_eq!(
            Block::Uploading(3).notice().unwrap(),
            "3 attachments still uploading: wait"
        );
        assert_eq!(Block::Empty.notice(), None);
        assert_eq!(Block::Busy.notice(), None);

        d.remove(2);
        let out = d.gate("look", agent()).unwrap();
        assert_eq!(out.ids, [1, 3]);
        assert_eq!(
            out.attachments,
            [
                Attachment {
                    name: "a.png".into(),
                    blob: [1; 32]
                },
                Attachment {
                    name: "c.png".into(),
                    blob: [3; 32]
                },
            ]
        );
    }

    #[test]
    fn attachments_without_text_send() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.png", b"a");
        d.uploaded(1, [1; 32]);
        let out = d.gate("", agent()).unwrap();
        assert_eq!(out.text, "");
        assert_eq!(out.ids, [1]);
    }

    #[test]
    fn what_one_drop_refused_shares_a_line() {
        assert_eq!(join_notices(Vec::new()), None);
        assert_eq!(
            join_notices(vec!["a is empty".into()]).as_deref(),
            Some("a is empty")
        );
        let five = ["a", "b", "c", "d", "e"].map(|n| format!("{n} is empty"));
        assert_eq!(
            join_notices(five.to_vec()).as_deref(),
            Some("a is empty; b is empty; c is empty; and 2 more")
        );
        let three = five[..3].to_vec();
        assert_eq!(
            join_notices(three).as_deref(),
            Some("a is empty; b is empty; c is empty")
        );
    }

    #[test]
    fn a_missing_portal_is_explained_not_quoted() {
        let explained = "no file picker here: drop or paste files instead";
        assert_eq!(
            picker_notice(
                "Portal request failed: org.freedesktop.zbus.Error: The name \
                 org.freedesktop.portal.Desktop was not provided by any .service files"
            ),
            explained
        );
        assert_eq!(
            picker_notice(
                "Couldn't open file picker due to missing xdg-desktop-portal implementation."
            ),
            explained
        );
        assert_eq!(picker_notice("boom"), "can't open the file picker: boom");
    }

    #[test]
    fn a_draft_takes_sixteen_files_and_refuses_the_next() {
        let mut d = Draft::default();
        for id in 0..MAX_CHIPS as ChipId {
            add(&mut d, id, "n.txt", b"x");
        }
        assert_eq!(d.chips.len(), MAX_CHIPS);
        let refused = d.add(99, "one-more.txt", b"x".to_vec()).err().unwrap();
        assert_eq!(
            refused,
            Refusal::TooMany {
                name: "one-more.txt".into()
            }
        );
        assert_eq!(
            refused.to_string(),
            "one-more.txt not attached: at most 16 files to a prompt"
        );
        // Taking one off makes room again.
        d.remove(0);
        add(&mut d, 100, "one-more.txt", b"x");
    }

    #[test]
    fn an_upload_the_daemon_dropped_goes_up_again_before_the_next_send() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.png", b"a");
        add(&mut d, 2, "b.png", b"b");
        d.uploaded(1, [1; 32]);
        // Only a ready chip can expire; one still on its way or a stray id is left alone.
        assert!(d.expire(1));
        assert!(!d.expire(2));
        assert!(!d.expire(9));
        assert_eq!(d.chip(1).unwrap().upload, Upload::Uploading);
        assert_eq!(d.gate("x", agent()), Err(Block::Uploading(2)));
        d.uploaded(1, [1; 32]);
        d.uploaded(2, [2; 32]);
        assert!(d.gate("x", agent()).is_ok());
    }

    #[test]
    fn a_wait_or_failed_line_goes_when_its_cause_does_and_other_notices_stay() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.png", b"a");
        d.notice = Block::Uploading(1).notice();
        d.uploaded(1, [1; 32]);
        assert_eq!(d.notice, None);

        // A refusal is not about the uploads: it stays when they land.
        d.notice = Some("big.bin is 9.0 MB: attachments are limited to 8.0 MB".into());
        add(&mut d, 2, "b.png", b"b");
        d.uploaded(2, [2; 32]);
        assert!(d.notice.is_some());

        add(&mut d, 3, "c.png", b"c");
        d.failed(3, "unsupported".into());
        d.notice = Block::Failed(1).notice();
        d.retry(3);
        assert_eq!(d.notice, None);

        d.failed(3, "unsupported".into());
        d.notice = Block::Failed(1).notice();
        d.remove(3);
        assert_eq!(d.notice, None);
    }

    #[test]
    fn a_send_in_flight_blocks_the_next_and_sent_keeps_later_chips() {
        let mut d = Draft::default();
        add(&mut d, 1, "a.png", b"a");
        d.uploaded(1, [1; 32]);
        let out = d.gate("one", agent()).unwrap();
        d.sending = true;
        assert_eq!(d.gate("two", agent()), Err(Block::Busy));

        // A chip dropped in while the prompt is on its way is not part of it.
        add(&mut d, 2, "b.png", b"b");
        d.sending = false;
        let gone = d.sent(&out.ids);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].id, 1);
        assert_eq!(d.chips.len(), 1);
        assert_eq!(d.chips[0].id, 2);
    }

    #[test]
    fn each_session_keeps_what_was_typed() {
        let mut ds = Drafts::default();
        ds.park(A, "fix the".into());
        ds.park(B, "and also".into());
        assert_eq!(ds.text(A), "fix the");
        assert_eq!(ds.text(B), "and also");
        // Back to A, finish the sentence, away again.
        ds.park(A, "fix the bug".into());
        assert_eq!(ds.text(A), "fix the bug");
        assert_eq!(ds.text(B), "and also");
        assert_eq!(ds.text(Id(3)), "");
    }

    #[test]
    fn a_draft_with_nothing_in_it_is_dropped_but_chips_keep_it() {
        let mut ds = Drafts::default();
        ds.park(A, "x".into());
        assert!(ds.get(A).is_some());
        ds.park(A, String::new());
        assert!(ds.get(A).is_none());

        let id = ds.new_id();
        add(ds.entry(A), id, "a.png", b"a");
        ds.park(A, String::new());
        assert!(
            ds.get(A).is_some(),
            "an upload is still waiting for its reply"
        );
        // A late reply to a session with no draft must not conjure one.
        assert!(ds.get_mut(B).is_none());
        assert!(ds.get(B).is_none());
        // Once the chip is gone and the reply is in, the draft goes with `prune`.
        ds.get_mut(A).unwrap().chips.clear();
        ds.prune(A);
        assert!(ds.get(A).is_none());
    }

    #[test]
    fn chip_ids_are_unique_across_sessions() {
        let mut ds = Drafts::default();
        let ids: Vec<_> = (0..4).map(|_| ds.new_id()).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
    }
}
