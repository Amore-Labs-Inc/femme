//! Desktop notifications from the raw byte stream. Alacritty's parser drops OSC 9 and OSC 777, so
//! this tiny scanner watches the bytes alongside it. It only looks for `ESC ] ... (BEL | ESC \)`.
//! It follows the parser's own states closely enough to know when the parser is inside an OSC,
//! because the parser keeps every payload byte of the OSC it is in until the OSC ends.

use super::EmuEvent;

/// Longest OSC payload kept; longer ones are dropped, not truncated. The parser is not given more
/// than this either (see [`Found::Cut`]).
const MAX_OSC: usize = 4096;

/// What [`OscScan::scan`] found in a chunk of bytes.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Found {
    /// A desktop notification. It is queued once the bytes before its offset have been parsed.
    Notify(EmuEvent),
    /// The payload of an oversized OSC past `MAX_OSC` bytes: the chunk's bytes from this offset up
    /// to the entry's own offset are not passed on. The terminator is, so the parser ends the
    /// OSC, truncated, where the program does (or never, if the program never does).
    Cut(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Just saw ESC.
    Esc,
    /// Inside `ESC ]`, collecting the payload.
    Osc,
    /// Saw ESC inside the payload: it ends it, and the next byte follows that ESC.
    OscEsc,
}

pub(super) struct OscScan {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
}

impl OscScan {
    pub(super) fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflow: false,
        }
    }

    /// Scan `bytes` (state carries across calls) and push a `Notify` for each OSC 9 / 777 seen,
    /// with the offset in `bytes` just past its terminator (so the caller can keep stream order),
    /// and a `Cut` for the excess of each oversized OSC, with the offset where the excess ends.
    pub(super) fn scan(&mut self, bytes: &[u8], out: &mut Vec<(usize, Found)>) {
        // Where the excess of the OSC being skipped starts in `bytes`.
        let mut cut = (self.state == State::Osc && self.overflow).then_some(0);
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match self.state {
                State::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Esc;
                    }
                    None => return,
                },
                State::Esc => {
                    i += 1;
                    match b {
                        b']' => {
                            self.buf.clear();
                            self.overflow = false;
                            self.state = State::Osc;
                        }
                        // The parser stays in its escape state for control bytes (it runs them),
                        // DEL and bytes above ASCII; only CAN and SUB leave it.
                        0x1b | 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f.. => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Osc => {
                    i += 1;
                    if matches!(b, 0x07 | 0x18 | 0x1a | 0x1b) {
                        // The parser's OSC ends at these too: the excess stops just before.
                        if let Some(from) = cut.take() {
                            out.push((i - 1, Found::Cut(from)));
                        }
                    }
                    match b {
                        0x07 => self.end(out, i),
                        0x1b => self.state = State::OscEsc,
                        0x18 | 0x1a => self.state = State::Ground,
                        _ if self.buf.len() < MAX_OSC => self.buf.push(b),
                        _ => {
                            self.overflow = true;
                            cut.get_or_insert(i - 1);
                        }
                    }
                }
                State::OscEsc => {
                    if b == b'\\' {
                        i += 1;
                        self.end(out, i);
                    } else {
                        // The parser ends the OSC at the ESC whatever follows; this byte is then
                        // the one after an ESC.
                        self.end(out, i);
                        self.state = State::Esc;
                    }
                }
            }
        }
        if let Some(from) = cut {
            out.push((bytes.len(), Found::Cut(from)));
        }
    }

    fn end(&mut self, out: &mut Vec<(usize, Found)>, at: usize) {
        self.state = State::Ground;
        if self.overflow {
            return;
        }
        let payload = String::from_utf8_lossy(&self.buf);
        if let Some(n) = parse(&payload) {
            out.push((at, Found::Notify(n)));
        }
    }
}

/// `9;body` (iTerm2) or `777;notify;title;body` (urxvt, kitty, foot, ghostty).
fn parse(p: &str) -> Option<EmuEvent> {
    if let Some(body) = p.strip_prefix("9;") {
        // ConEmu uses OSC 9 for other things too (`9;4;state;progress`, `9;9;cwd`, `9;12`, ...):
        // a body that is a number, alone or followed by `;`, is one of those, not a message.
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && matches!(body.as_bytes().get(digits), None | Some(&b';')) {
            return None;
        }
        return Some(EmuEvent::Notify {
            title: String::new(),
            body: body.to_string(),
        });
    }
    let rest = p.strip_prefix("777;notify;")?;
    let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
    Some(EmuEvent::Notify {
        title: title.to_string(),
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<EmuEvent> {
        let mut s = OscScan::new();
        let mut out = Vec::new();
        for c in chunks {
            s.scan(c, &mut out);
        }
        out.into_iter()
            .filter_map(|(_, f)| match f {
                Found::Notify(e) => Some(e),
                Found::Cut(_) => None,
            })
            .collect()
    }

    #[test]
    fn offsets_point_just_past_the_terminator() {
        let mut s = OscScan::new();
        let mut out = Vec::new();
        s.scan(b"ab\x1b]9;one\x07cd\x1b]9;two\x1b\\ef", &mut out);
        assert_eq!(out.iter().map(|o| o.0).collect::<Vec<_>>(), [10, 21]);
    }

    #[test]
    fn the_excess_of_an_oversized_osc_is_cut_whatever_the_chunking() {
        let mut s = OscScan::new();
        let mut out = Vec::new();
        let mut first = b"\x1b]".to_vec();
        first.extend(std::iter::repeat_n(b'a', MAX_OSC + 10));
        s.scan(&first, &mut out);
        assert_eq!(out, [(first.len(), Found::Cut(2 + MAX_OSC))]);
        // the rest of it is cut from the start of the chunk up to the terminator, which is passed on
        out.clear();
        s.scan(b"aaaaa\x07ok", &mut out);
        assert_eq!(out, [(5, Found::Cut(0))]);
        // and the next OSC starts fresh
        out.clear();
        s.scan(b"\x1b]9;x\x07", &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].1, Found::Notify(_)));
    }

    fn notify(t: &str, b: &str) -> EmuEvent {
        EmuEvent::Notify {
            title: t.into(),
            body: b.into(),
        }
    }

    #[test]
    fn osc9_and_osc777_with_both_terminators() {
        assert_eq!(
            scan_all(&[b"x\x1b]9;hello\x07y"]),
            vec![notify("", "hello")]
        );
        assert_eq!(
            scan_all(&[b"\x1b]777;notify;Claude;done; really\x1b\\"]),
            vec![notify("Claude", "done; really")]
        );
        assert_eq!(
            scan_all(&[b"\x1b]777;notify;only title\x07"]),
            vec![notify("only title", "")]
        );
    }

    #[test]
    fn split_across_feeds_and_ignored_kinds() {
        assert_eq!(
            scan_all(&[b"\x1b", b"]9;ab", b"cd\x1b", b"\\"]),
            vec![notify("", "abcd")]
        );
        assert!(scan_all(&[b"\x1b]9;4;1;50\x07\x1b]0;title\x07\x1b]777;other;a;b\x07"]).is_empty());
        assert_eq!(
            scan_all(&[b"\x1b]9;100 percent\x07"]),
            vec![notify("", "100 percent")]
        );
    }

    #[test]
    fn conemu_sequences_are_not_notifications_whatever_their_digit_count() {
        for body in ["4;1;50", "9;9;/tmp", "12", "123;x", "1234;", "7"] {
            let s = format!("\x1b]9;{body}\x07");
            assert!(scan_all(&[s.as_bytes()]).is_empty(), "{body}");
        }
        // text that only starts with digits is a message
        for body in ["100 percent", "3 files changed", "1st"] {
            let s = format!("\x1b]9;{body}\x07");
            assert_eq!(scan_all(&[s.as_bytes()]), vec![notify("", body)]);
        }
    }

    #[test]
    fn an_escape_inside_an_osc_ends_it_like_the_terminal_does() {
        // the parser dispatches the OSC at the ESC, whatever follows
        assert_eq!(
            scan_all(&[b"\x1b]9;one\x1b[31mtext"]),
            vec![notify("", "one")]
        );
        assert_eq!(
            scan_all(&[b"\x1b]9;one\x1b]9;two\x07"]),
            vec![notify("", "one"), notify("", "two")]
        );
        // split between the ESC and the byte after it
        assert_eq!(
            scan_all(&[b"\x1b]9;one\x1b", b"[31m"]),
            vec![notify("", "one")]
        );
    }

    #[test]
    fn control_and_high_bytes_after_esc_do_not_hide_an_osc() {
        // the parser stays in its escape state for these, then `]` starts an OSC
        assert_eq!(
            scan_all(&[b"\x1b\x01]9;a\x07\x1b\x7f]9;b\x07\x1b\xc3\xa9]9;c\x07"]),
            vec![notify("", "a"), notify("", "b"), notify("", "c")]
        );
        // but CAN and SUB leave it
        assert!(scan_all(&[b"\x1b\x18]9;a\x07\x1b\x1a]9;b\x07"]).is_empty());
    }

    #[test]
    fn aborted_and_oversized_payloads_are_dropped() {
        assert!(scan_all(&[b"\x1b]9;cancelled \x18 text\x07"]).is_empty());
        let big = vec![b'a'; MAX_OSC + 10];
        let mut s = b"\x1b]9;".to_vec();
        s.extend(&big);
        s.push(0x07);
        assert!(scan_all(&[&s]).is_empty());
        // and the scanner recovers for the next one
        assert_eq!(scan_all(&[&s, b"\x1b]9;ok\x07"]), vec![notify("", "ok")]);
    }
}
