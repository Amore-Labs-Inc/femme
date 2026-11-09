//! Small pieces every driver uses: spool-offset bookkeeping, tolerant line parsing, status
//! transitions, text previews, and the protocol-less shell driver.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::{AgentEvent, AgentStatus};

/// Tracks which spool offsets are still needed after a daemon restart ([`crate::Driver::resume_offset`]).
///
/// Call [`Offsets::seen`] for **every** line first, then `turn_begin`/`turn_end` and
/// `item_open`/`item_close` as events are emitted. `resume()` is the smallest start offset of
/// the unfinished turn or any unfinished item, else the offset just past the last line (or the
/// offset a re-attached driver was told to start at, until it sees a line).
///
/// Lines must be **complete**: `seen` moves the fallback to the end of the line, so a partial
/// line would leave `resume()` in the middle of one.
#[derive(Debug, Default, Clone)]
pub(crate) struct Offsets {
    next: u64,
    turn: Option<u64>,
    open: BTreeMap<String, u64>,
}

impl Offsets {
    /// Re-attach: the replay starts at `offset`, so that is the answer until a line is seen.
    pub fn start_at(&mut self, offset: u64) {
        self.next = offset;
    }
    /// The complete `line` (without its `\n`) starts at `offset`; the next one starts after it.
    pub fn seen(&mut self, offset: u64, line: &[u8]) {
        debug_assert!(
            !line.contains(&b'\n'),
            "on_line takes one complete line without its newline"
        );
        self.next = offset.saturating_add(line.len() as u64).saturating_add(1);
    }
    /// The line at `offset` is the first of a turn (kept if one is already open).
    pub fn turn_begin(&mut self, offset: u64) {
        self.turn.get_or_insert(offset);
    }
    /// The turn is over.
    pub fn turn_end(&mut self) {
        self.turn = None;
    }
    /// Where the running turn began, if one runs.
    pub fn turn_start(&self) -> Option<u64> {
        self.turn
    }
    /// Item `id` opened at the line starting at `offset` (kept if already open).
    pub fn item_open(&mut self, id: &str, offset: u64) {
        self.open.entry(id.to_owned()).or_insert(offset);
    }
    /// Where the open item `id` began, else the offset past the last line.
    pub fn open_at(&self, id: &str) -> u64 {
        self.open.get(id).copied().unwrap_or(self.next)
    }
    /// Item `id` is final.
    pub fn item_close(&mut self, id: &str) {
        self.open.remove(id);
    }
    /// Smallest offset still needed.
    pub fn resume(&self) -> u64 {
        self.turn
            .into_iter()
            .chain(self.open.values().copied())
            .min()
            .unwrap_or(self.next)
    }
}

/// One stdout line as JSON; `None` for blank lines and anything that is not valid JSON.
pub(crate) fn parse(line: &[u8]) -> Option<Value> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    serde_json::from_slice(line).ok()
}

/// The line as a `Raw` event when it is not JSON at all.
pub(crate) fn raw_text(line: &[u8]) -> AgentEvent {
    AgentEvent::Raw(Value::String(String::from_utf8_lossy(line).into_owned()))
}

/// First ~512 bytes of `s`, cut on a char boundary.
pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 512;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Move `cur` to `new`, pushing `Status(new)` only when it changes.
pub(crate) fn set_status(cur: &mut AgentStatus, new: AgentStatus, ev: &mut Vec<AgentEvent>) {
    if *cur != new {
        *cur = new;
        ev.push(AgentEvent::Status(new));
    }
}

/// Wall-clock Unix milliseconds, for [`crate::HeldPrompt::queued_at_ms`] (0 if the clock is
/// before 1970).
// ponytail: the one clock read in a sans-IO crate. A test or a daemon that needs to control it
// can pass the time in (a `Launch`/`Attach` field or an argument of `prompt`); nothing needs that yet.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A held prompt, stamped now.
pub(crate) fn held(text: &str, how: femme_proto::rpc::PromptHow) -> crate::HeldPrompt {
    crate::HeldPrompt {
        text: text.to_owned(),
        how,
        queued_at_ms: now_ms(),
    }
}

/// Insert `h` into a held-prompt queue: `Queue` and `Steer` go to the back; `InterruptThenSend`
/// goes behind the interrupt-then-send prompts already waiting and in front of everything else,
/// so two of them run in the order they were given.
pub(crate) fn hold(
    queue: &mut std::collections::VecDeque<crate::HeldPrompt>,
    h: crate::HeldPrompt,
) {
    if h.how == femme_proto::rpc::PromptHow::InterruptThenSend {
        let at = queue
            .iter()
            .take_while(|q| q.how == femme_proto::rpc::PromptHow::InterruptThenSend)
            .count();
        queue.insert(at, h);
    } else {
        queue.push_back(h);
    }
}

/// `PromptDropped` events for everything in `queue` (which is emptied), oldest first.
pub(crate) fn drop_held(
    queue: &mut std::collections::VecDeque<crate::HeldPrompt>,
    reason: &crate::DropReason,
    ev: &mut Vec<AgentEvent>,
) {
    for h in std::mem::take(queue) {
        ev.push(AgentEvent::PromptDropped {
            text: h.text,
            how: h.how,
            reason: reason.clone(),
        });
    }
}

/// A random-looking, valid version-4 UUID string built from a ULID (time prefix + 80 random
/// bits), for agents that take a caller-chosen session id (`claude --session-id`).
pub(crate) fn uuid_v4() -> String {
    let mut n = femme_proto::Id::new().0;
    n = (n & !(0xf_u128 << 76)) | (4_u128 << 76); // version 4
    n = (n & !(0x3_u128 << 62)) | (2_u128 << 62); // variant 10xx
    let h = format!("{n:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// The driver for `Agent::Shell`: no protocol, it only knows its command line. Every call that
/// needs a protocol is `Unsupported`.
#[derive(Debug, Default)]
pub struct ShellDriver;

impl crate::Driver for ShellDriver {
    fn agent(&self) -> femme_proto::state::Agent {
        femme_proto::state::Agent::Shell
    }
    fn spawn(&self, l: &crate::Launch) -> crate::Spawn {
        crate::Spawn {
            cmd: vec![std::env::var("SHELL").unwrap_or_else(|_| "sh".into())],
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        }
    }
    fn tui(&self, _: &crate::Launch) -> Option<crate::Spawn> {
        None
    }
    fn on_start(&mut self, _: &crate::Launch, _: &mut crate::Out) {}
    fn on_line(&mut self, _: u64, line: &[u8], _: &mut crate::Out) -> Vec<AgentEvent> {
        vec![raw_text(line)]
    }
    fn prompt(
        &mut self,
        _: &str,
        _: femme_proto::rpc::PromptHow,
        _: &mut crate::Out,
    ) -> Result<(), crate::Error> {
        Err(crate::Error::Unsupported("no headless protocol"))
    }
    fn interrupt(&mut self, _: &mut crate::Out) -> Result<(), crate::Error> {
        Err(crate::Error::Unsupported("no headless protocol"))
    }
    fn answer(
        &mut self,
        _: &str,
        _: &femme_proto::rpc::Answer,
        _: &mut crate::Out,
    ) -> Result<(), crate::Error> {
        Err(crate::Error::Unsupported("no headless protocol"))
    }
    fn set_model(&mut self, _: &str, _: &mut crate::Out) -> Result<(), crate::Error> {
        Err(crate::Error::Unsupported("no headless protocol"))
    }
    fn attach(&mut self, _: &crate::Launch, _: &crate::Attach, _: &mut crate::Out) {}
    fn resume_offset(&self) -> u64 {
        0
    }
    fn held_prompts(&self) -> Vec<crate::HeldPrompt> {
        Vec::new()
    }
    fn pending_answers(&self) -> Vec<(String, femme_proto::rpc::Answer)> {
        Vec::new()
    }
    fn agent_session_id(&self) -> Option<&str> {
        None
    }
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent> {
        vec![AgentEvent::Exited { code }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Vec<u8> {
        vec![b'x'; n]
    }

    #[test]
    fn offsets_resume_is_oldest_unfinished() {
        let mut o = Offsets::default();
        o.seen(0, &line(9));
        assert_eq!(o.resume(), 10);
        o.turn_begin(10);
        o.seen(10, &line(4));
        o.item_open("a", 15);
        o.seen(15, &line(20));
        assert_eq!(o.resume(), 10);
        o.turn_end();
        assert_eq!(o.resume(), 15);
        o.item_close("a");
        assert_eq!(o.resume(), 36);
    }

    /// A fresh driver reported offset 0 until its first line (finding 7); a re-attached one
    /// starts where the daemon said, and a line takes over from there.
    #[test]
    fn offsets_start_where_the_daemon_replays_from() {
        let mut o = Offsets::default();
        assert_eq!(o.resume(), 0);
        o.start_at(4096);
        assert_eq!(o.resume(), 4096);
        o.seen(4096, &line(9));
        assert_eq!(o.resume(), 4106);
    }

    /// The complete-line contract (finding 19): a chunk with a newline in it would put the
    /// resume offset in the middle of the next line, so debug builds refuse it.
    #[test]
    #[should_panic(expected = "complete line")]
    fn offsets_refuse_a_line_with_a_newline() {
        Offsets::default().seen(0, b"{\"a\":1}\n{\"b\":2}");
    }

    #[test]
    fn interrupt_then_send_prompts_keep_their_order_in_front() {
        use femme_proto::rpc::PromptHow::{InterruptThenSend as I, Queue as Q, Steer as S};
        let mut q = std::collections::VecDeque::new();
        for (t, h) in [("q1", Q), ("i1", I), ("s1", S), ("i2", I), ("q2", Q)] {
            hold(&mut q, held(t, h));
        }
        assert_eq!(
            q.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["i1", "i2", "q1", "s1", "q2"]
        );
        let mut ev = Vec::new();
        drop_held(&mut q, &crate::DropReason::Exited, &mut ev);
        assert!(q.is_empty());
        assert_eq!(ev.len(), 5);
        assert!(
            matches!(&ev[0], AgentEvent::PromptDropped { text, how: I, reason: crate::DropReason::Exited } if text == "i1")
        );
    }

    #[test]
    fn preview_cuts_on_char_boundary() {
        let s = "é".repeat(400);
        let p = preview(&s);
        assert!(p.len() <= 512 && s.starts_with(&p));
    }

    #[test]
    fn uuid_is_v4() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }
}
