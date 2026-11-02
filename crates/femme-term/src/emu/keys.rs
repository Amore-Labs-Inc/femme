//! Input encoding: keys, paste, mouse and focus events to the bytes the program expects, decided
//! from the terminal's *live* mode flags (so the client never races a mode change).

use alacritty_terminal::term::TermMode;
use femme_proto::term::{Key, KeyCode, Mods, MouseEv, MouseKind};

const ESC: u8 = 0x1b;
const ETX: u8 = 0x03;

/// xterm modifier parameter: `1 + shift(1) + alt(2) + ctrl(4)`, plus `super(8)` where the
/// protocol has it (kitty mode only, also in the CSI forms it leaves to `legacy`; without kitty
/// flags there is no way to say "super").
fn mod_param(mods: Mods, with_super: bool) -> u8 {
    let mut m = 1;
    if mods.contains(Mods::SHIFT) {
        m += 1;
    }
    if mods.contains(Mods::ALT) {
        m += 2;
    }
    if mods.contains(Mods::CTRL) {
        m += 4;
    }
    if with_super && mods.contains(Mods::SUPER) {
        m += 8;
    }
    m
}

pub(super) fn encode_key(mode: TermMode, key: Key) -> Vec<u8> {
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        if let Some(v) = kitty(mode, key.code, key.mods) {
            return v;
        }
    }
    legacy(mode, key.code, key.mods)
}

/// Control byte for `ctrl+c` (xterm's table, including the digit row spellings).
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// `ESC [ 1 ; m L` with modifiers, `ESC O L` in application cursor mode, `ESC [ L` otherwise.
fn cursor_key(letter: u8, app: bool, m: u8) -> Vec<u8> {
    if m > 1 {
        format!("\x1b[1;{m}{}", letter as char).into_bytes()
    } else if app {
        vec![ESC, b'O', letter]
    } else {
        vec![ESC, b'[', letter]
    }
}

/// `ESC [ n ~`, or `ESC [ n ; m ~` with modifiers.
fn tilde(n: u8, m: u8) -> Vec<u8> {
    if m > 1 {
        format!("\x1b[{n};{m}~").into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn fkey(n: u8, m: u8) -> Vec<u8> {
    match n {
        1..=4 => {
            let letter = b"PQRS"[usize::from(n) - 1];
            if m > 1 {
                format!("\x1b[1;{m}{}", letter as char).into_bytes()
            } else {
                vec![ESC, b'O', letter]
            }
        }
        // F5..F20: 15 17 18 19 20 21 23 24 25 26 28 29 31 32 33 34
        5..=20 => tilde(
            [
                15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
            ][usize::from(n) - 5],
            m,
        ),
        _ => Vec::new(),
    }
}

fn legacy(mode: TermMode, code: KeyCode, mods: Mods) -> Vec<u8> {
    let alt = mods.contains(Mods::ALT);
    let ctrl = mods.contains(Mods::CTRL);
    let shift = mods.contains(Mods::SHIFT);
    // With kitty flags set this is the fallback for the keys kitty leaves in their CSI form, and
    // kitty's modifier parameter has the super bit.
    let m = mod_param(mods, mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
    let app = mode.contains(TermMode::APP_CURSOR);
    // Alt is "meta sends escape": the plain encoding behind an ESC.
    let meta = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, ESC);
        }
        v
    };
    match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let plain = match ctrl.then(|| ctrl_byte(c)).flatten() {
                Some(b) => vec![b],
                None => c.encode_utf8(&mut buf).as_bytes().to_vec(),
            };
            meta(plain)
        }
        KeyCode::Enter => meta(b"\r".to_vec()),
        KeyCode::Tab if shift => meta(b"\x1b[Z".to_vec()),
        KeyCode::Tab => meta(b"\t".to_vec()),
        KeyCode::Backspace => meta(vec![if ctrl { 0x08 } else { 0x7f }]),
        KeyCode::Escape => meta(vec![ESC]),
        KeyCode::Up => cursor_key(b'A', app, m),
        KeyCode::Down => cursor_key(b'B', app, m),
        KeyCode::Right => cursor_key(b'C', app, m),
        KeyCode::Left => cursor_key(b'D', app, m),
        KeyCode::Home => cursor_key(b'H', app, m),
        KeyCode::End => cursor_key(b'F', app, m),
        KeyCode::PageUp => tilde(5, m),
        KeyCode::PageDown => tilde(6, m),
        KeyCode::Insert => tilde(2, m),
        KeyCode::Delete => tilde(3, m),
        KeyCode::F(n) => fkey(n, m),
    }
}

/// Kitty keyboard protocol (`CSI > flags u`). Only the keys the protocol changes are handled
/// here; everything else (arrows, F-keys, ...) keeps its legacy CSI form, which kitty shares
/// (with the super bit, see `legacy`). `None` means "use the legacy encoding". The one F-key
/// that changes is F3 with modifiers, `CSI 13 ; mods ~`: `CSI 1 ; mods R` would read as a
/// cursor position report.
///
/// Flag 1 (disambiguate): Escape and any key with ctrl/alt/super become `CSI code ; mods u`;
/// Enter, Tab and Backspace only when modified; text keys (none or shift only) stay text.
/// Flag 8 (all keys as escape codes): every one of those is `CSI u`, text included.
/// Event types, alternate keys and associated text (flags 2, 4, 16) are not produced: `Key`
/// carries presses only.
fn kitty(mode: TermMode, code: KeyCode, mods: Mods) -> Option<Vec<u8>> {
    let all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let m = mod_param(mods, true);
    let csi_u = |cp: u32| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[{cp};{m}u").into_bytes()
        } else {
            format!("\x1b[{cp}u").into_bytes()
        }
    };
    match code {
        KeyCode::Char(c) if all || mods.intersects(Mods::CTRL | Mods::ALT | Mods::SUPER) => {
            // The protocol names a key by its unshifted codepoint.
            // ponytail: un-shifts letters only, symbols need a layout-aware unshifted code on `Key`
            let cp = if c.is_uppercase() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            };
            Some(csi_u(cp as u32))
        }
        KeyCode::Escape => Some(csi_u(27)),
        KeyCode::Enter if all || m > 1 => Some(csi_u(13)),
        KeyCode::Tab if all || m > 1 => Some(csi_u(9)),
        KeyCode::Backspace if all || m > 1 => Some(csi_u(127)),
        // `CSI 1 ; m R` would read as a cursor position report, so modified F3 is a tilde key.
        KeyCode::F(3) if m > 1 => Some(tilde(13, m)),
        _ => None,
    }
}

/// Paste text, in bracketed-paste markers when the program asked for them. In bracketed mode
/// every ESC and ETX is dropped from the text: ESC could otherwise build the end marker and let
/// the pasted text run as typed input, and some shells end the paste on ETX. Without the markers
/// the program cannot tell a paste from typing, so each line break (`\r\n` or `\n`) becomes the
/// `\r` that Enter sends and the rest passes unchanged.
pub(super) fn encode_paste(mode: TermMode, text: &str) -> Vec<u8> {
    if !mode.contains(TermMode::BRACKETED_PASTE) {
        return text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend(text.bytes().filter(|&b| b != ESC && b != ETX));
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// `CSI I` / `CSI O` when the program asked for focus reports (`CSI ? 1004 h`).
pub(super) fn encode_focus(mode: TermMode, focused: bool) -> Option<Vec<u8>> {
    mode.contains(TermMode::FOCUS_IN_OUT)
        .then(|| vec![ESC, b'[', if focused { b'I' } else { b'O' }])
}

/// One mouse report in the encoding the program selected: SGR (1006), UTF-8 (1005) or the
/// legacy X10 bytes (coordinates past 222 cannot be expressed there and the event is dropped).
fn report(mode: TermMode, code: u8, col: u16, row: u16, press: bool) -> Option<Vec<u8>> {
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        return Some(format!("\x1b[<{code};{x};{y}{}", if press { 'M' } else { 'm' }).into_bytes());
    }
    let release_code = if press { code } else { 3 | (code & !3) };
    let vals = [u32::from(release_code) + 32, x + 32, y + 32];
    let mut out = vec![ESC, b'[', b'M'];
    if mode.contains(TermMode::UTF8_MOUSE) {
        for v in vals {
            if v > 2047 {
                return None;
            }
            let mut buf = [0u8; 4];
            out.extend(char::from_u32(v)?.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        if vals.iter().any(|&v| v > 255) {
            return None;
        }
        out.extend(vals.iter().map(|&v| v as u8));
    }
    Some(out)
}

/// Mouse event to bytes. `None` when reporting is off or this event is not reported in the active
/// mode. Conventions: press/release use `button` 0 left, 1 middle, 2 right (others are dropped);
/// for `Move`, `button` 0..=2 is the button held (a drag) and anything larger means no button;
/// wheel events are 64 (up) / 65 (down). With the alternate screen active, `ALTERNATE_SCROLL` set
/// and no mouse mode, a wheel notch becomes 3 arrow keys instead.
pub(super) fn encode_mouse(mode: TermMode, ev: MouseEv) -> Option<Vec<u8>> {
    let mut mod_bits = 0u8;
    if ev.mods.contains(Mods::SHIFT) {
        mod_bits |= 4;
    }
    if ev.mods.contains(Mods::ALT) {
        mod_bits |= 8;
    }
    if ev.mods.contains(Mods::CTRL) {
        mod_bits |= 16;
    }
    let reporting = mode.intersects(TermMode::MOUSE_MODE);
    match ev.kind {
        MouseKind::ScrollUp | MouseKind::ScrollDown => {
            let down = ev.kind == MouseKind::ScrollDown;
            if reporting {
                report(mode, 64 + u8::from(down) + mod_bits, ev.col, ev.row, true)
            } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                let letter = if down { b'B' } else { b'A' };
                let one = cursor_key(letter, mode.contains(TermMode::APP_CURSOR), 1);
                Some(one.repeat(3))
            } else {
                None
            }
        }
        MouseKind::Press if reporting && ev.button <= 2 => {
            report(mode, ev.button + mod_bits, ev.col, ev.row, true)
        }
        MouseKind::Release if reporting && ev.button <= 2 => {
            report(mode, ev.button + mod_bits, ev.col, ev.row, false)
        }
        MouseKind::Move => {
            let held = ev.button <= 2;
            let wanted = if held {
                TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION
            } else {
                TermMode::MOUSE_MOTION
            };
            if !mode.intersects(wanted) {
                return None;
            }
            let button = if held { ev.button } else { 3 };
            report(mode, 32 + button + mod_bits, ev.col, ev.row, true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: Mods) -> Key {
        Key { code, mods }
    }

    fn enc(mode: TermMode, code: KeyCode, mods: Mods) -> String {
        String::from_utf8_lossy(&encode_key(mode, key(code, mods))).replace('\x1b', "<ESC>")
    }

    const NONE: Mods = Mods::empty();
    const SHIFT: Mods = Mods::SHIFT;
    const ALT: Mods = Mods::ALT;
    const CTRL: Mods = Mods::CTRL;

    /// The legacy table, rows: key, mods, normal mode, application cursor mode.
    #[test]
    fn legacy_key_table() {
        use KeyCode::*;
        let normal = TermMode::empty();
        let app = TermMode::APP_CURSOR;
        #[rustfmt::skip]
        let rows: &[(KeyCode, Mods, &str, &str)] = &[
            (Char('a'), NONE, "a", "a"),
            (Char('A'), SHIFT, "A", "A"),
            (Char('é'), NONE, "é", "é"),
            (Char('猫'), NONE, "猫", "猫"),
            (Char('a'), CTRL, "\u{1}", "\u{1}"),
            (Char('z'), CTRL, "\u{1a}", "\u{1a}"),
            (Char('c'), CTRL, "\u{3}", "\u{3}"),
            (Char(' '), CTRL, "\u{0}", "\u{0}"),
            (Char('@'), CTRL, "\u{0}", "\u{0}"),
            (Char('['), CTRL, "<ESC>", "<ESC>"),
            (Char('\\'), CTRL, "\u{1c}", "\u{1c}"),
            (Char(']'), CTRL, "\u{1d}", "\u{1d}"),
            (Char('^'), CTRL, "\u{1e}", "\u{1e}"),
            (Char('_'), CTRL, "\u{1f}", "\u{1f}"),
            (Char('?'), CTRL, "\u{7f}", "\u{7f}"),
            (Char('5'), CTRL, "\u{1d}", "\u{1d}"),
            (Char('1'), CTRL, "1", "1"),
            (Char('a'), ALT, "<ESC>a", "<ESC>a"),
            (Char('é'), ALT, "<ESC>é", "<ESC>é"),
            (Char('a'), CTRL.union(ALT), "<ESC>\u{1}", "<ESC>\u{1}"),
            (Enter, NONE, "\r", "\r"),
            (Enter, ALT, "<ESC>\r", "<ESC>\r"),
            (Enter, SHIFT, "\r", "\r"),
            (Tab, NONE, "\t", "\t"),
            (Tab, SHIFT, "<ESC>[Z", "<ESC>[Z"),
            (Tab, ALT, "<ESC>\t", "<ESC>\t"),
            (Backspace, NONE, "\u{7f}", "\u{7f}"),
            (Backspace, ALT, "<ESC>\u{7f}", "<ESC>\u{7f}"),
            (Backspace, CTRL, "\u{8}", "\u{8}"),
            (Escape, NONE, "<ESC>", "<ESC>"),
            (Escape, ALT, "<ESC><ESC>", "<ESC><ESC>"),
            (Up, NONE, "<ESC>[A", "<ESC>OA"),
            (Down, NONE, "<ESC>[B", "<ESC>OB"),
            (Right, NONE, "<ESC>[C", "<ESC>OC"),
            (Left, NONE, "<ESC>[D", "<ESC>OD"),
            (Home, NONE, "<ESC>[H", "<ESC>OH"),
            (End, NONE, "<ESC>[F", "<ESC>OF"),
            (Up, SHIFT, "<ESC>[1;2A", "<ESC>[1;2A"),
            (Up, ALT, "<ESC>[1;3A", "<ESC>[1;3A"),
            (Left, CTRL, "<ESC>[1;5D", "<ESC>[1;5D"),
            (Right, CTRL.union(SHIFT), "<ESC>[1;6C", "<ESC>[1;6C"),
            (Down, CTRL.union(ALT).union(SHIFT), "<ESC>[1;8B", "<ESC>[1;8B"),
            (Home, CTRL, "<ESC>[1;5H", "<ESC>[1;5H"),
            (End, SHIFT, "<ESC>[1;2F", "<ESC>[1;2F"),
            (PageUp, NONE, "<ESC>[5~", "<ESC>[5~"),
            (PageDown, NONE, "<ESC>[6~", "<ESC>[6~"),
            (Insert, NONE, "<ESC>[2~", "<ESC>[2~"),
            (Delete, NONE, "<ESC>[3~", "<ESC>[3~"),
            (PageUp, SHIFT, "<ESC>[5;2~", "<ESC>[5;2~"),
            (Delete, CTRL, "<ESC>[3;5~", "<ESC>[3;5~"),
            (Insert, ALT, "<ESC>[2;3~", "<ESC>[2;3~"),
            (F(1), NONE, "<ESC>OP", "<ESC>OP"),
            (F(2), NONE, "<ESC>OQ", "<ESC>OQ"),
            (F(3), NONE, "<ESC>OR", "<ESC>OR"),
            (F(4), NONE, "<ESC>OS", "<ESC>OS"),
            (F(1), SHIFT, "<ESC>[1;2P", "<ESC>[1;2P"),
            (F(4), CTRL, "<ESC>[1;5S", "<ESC>[1;5S"),
            (F(5), NONE, "<ESC>[15~", "<ESC>[15~"),
            (F(6), NONE, "<ESC>[17~", "<ESC>[17~"),
            (F(7), NONE, "<ESC>[18~", "<ESC>[18~"),
            (F(8), NONE, "<ESC>[19~", "<ESC>[19~"),
            (F(9), NONE, "<ESC>[20~", "<ESC>[20~"),
            (F(10), NONE, "<ESC>[21~", "<ESC>[21~"),
            (F(11), NONE, "<ESC>[23~", "<ESC>[23~"),
            (F(12), NONE, "<ESC>[24~", "<ESC>[24~"),
            (F(5), SHIFT, "<ESC>[15;2~", "<ESC>[15;2~"),
            (F(12), CTRL.union(ALT), "<ESC>[24;7~", "<ESC>[24;7~"),
            (F(13), NONE, "<ESC>[25~", "<ESC>[25~"),
            (F(20), NONE, "<ESC>[34~", "<ESC>[34~"),
            (F(0), NONE, "", ""),
            (F(21), NONE, "", ""),
        ];
        for &(code, mods, want_normal, want_app) in rows {
            assert_eq!(
                enc(normal, code, mods),
                want_normal,
                "{code:?} {mods:?} normal"
            );
            assert_eq!(enc(app, code, mods), want_app, "{code:?} {mods:?} app");
        }
    }

    #[test]
    fn super_is_ignored_by_legacy_encodings() {
        assert_eq!(enc(TermMode::empty(), KeyCode::Char('a'), Mods::SUPER), "a");
        assert_eq!(enc(TermMode::empty(), KeyCode::Up, Mods::SUPER), "<ESC>[A");
    }

    #[test]
    fn kitty_disambiguate_table() {
        use KeyCode::*;
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        #[rustfmt::skip]
        let rows: &[(KeyCode, Mods, &str)] = &[
            (Escape, NONE, "<ESC>[27u"),
            (Escape, SHIFT, "<ESC>[27;2u"),
            (Enter, NONE, "\r"),
            (Tab, NONE, "\t"),
            (Backspace, NONE, "\u{7f}"),
            (Enter, SHIFT, "<ESC>[13;2u"),
            (Enter, CTRL, "<ESC>[13;5u"),
            (Tab, SHIFT, "<ESC>[9;2u"),
            (Tab, CTRL, "<ESC>[9;5u"),
            (Backspace, CTRL, "<ESC>[127;5u"),
            (Backspace, ALT, "<ESC>[127;3u"),
            (Char('a'), NONE, "a"),
            (Char('A'), SHIFT, "A"),
            (Char('a'), CTRL, "<ESC>[97;5u"),
            (Char('c'), CTRL, "<ESC>[99;5u"),
            (Char('a'), ALT, "<ESC>[97;3u"),
            (Char('A'), CTRL.union(SHIFT), "<ESC>[97;6u"),
            (Char('a'), CTRL.union(ALT), "<ESC>[97;7u"),
            (Char('a'), Mods::SUPER, "<ESC>[97;9u"),
            (Char(' '), CTRL, "<ESC>[32;5u"),
            (Char('['), CTRL, "<ESC>[91;5u"),
            // keys kitty leaves in their CSI form
            (Up, NONE, "<ESC>[A"),
            (Up, CTRL, "<ESC>[1;5A"),
            (F(5), SHIFT, "<ESC>[15;2~"),
            (PageDown, NONE, "<ESC>[6~"),
        ];
        for &(code, mods, want) in rows {
            assert_eq!(enc(k, code, mods), want, "{code:?} {mods:?}");
        }
        // any flag at all switches the encoder on
        assert_eq!(enc(TermMode::REPORT_EVENT_TYPES, Escape, NONE), "<ESC>[27u");
        assert_eq!(enc(TermMode::empty(), Escape, NONE), "<ESC>");
    }

    #[test]
    fn kitty_report_all_keys() {
        use KeyCode::*;
        let k = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(enc(k, Char('a'), NONE), "<ESC>[97u");
        assert_eq!(enc(k, Char('A'), SHIFT), "<ESC>[97;2u");
        assert_eq!(enc(k, Enter, NONE), "<ESC>[13u");
        assert_eq!(enc(k, Tab, NONE), "<ESC>[9u");
        assert_eq!(enc(k, Backspace, NONE), "<ESC>[127u");
        assert_eq!(enc(k, Char('猫'), NONE), format!("<ESC>[{}u", '猫' as u32));
    }

    #[test]
    fn kitty_mode_keeps_super_on_keys_it_leaves_to_the_legacy_encoder() {
        use KeyCode::*;
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        #[rustfmt::skip]
        let rows: &[(KeyCode, Mods, &str)] = &[
            (Up, Mods::SUPER, "<ESC>[1;9A"),
            (Home, Mods::SUPER, "<ESC>[1;9H"),
            (Up, CTRL.union(Mods::SUPER), "<ESC>[1;13A"),
            (Delete, Mods::SUPER, "<ESC>[3;9~"),
            (F(5), Mods::SUPER, "<ESC>[15;9~"),
            (F(1), Mods::SUPER, "<ESC>[1;9P"),
        ];
        for &(code, mods, want) in rows {
            assert_eq!(enc(k, code, mods), want, "{code:?} {mods:?}");
        }
        // any kitty flag at all, and without one super is still dropped
        assert_eq!(
            enc(TermMode::REPORT_EVENT_TYPES, Up, Mods::SUPER),
            "<ESC>[1;9A"
        );
        assert_eq!(enc(TermMode::empty(), Up, Mods::SUPER), "<ESC>[A");
    }

    #[test]
    fn kitty_mode_sends_modified_f3_as_tilde_not_a_cursor_position_report() {
        use KeyCode::*;
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        #[rustfmt::skip]
        let rows: &[(KeyCode, Mods, &str)] = &[
            (F(3), NONE, "<ESC>OR"),
            (F(3), SHIFT, "<ESC>[13;2~"),
            (F(3), CTRL, "<ESC>[13;5~"),
            (F(3), CTRL.union(ALT), "<ESC>[13;7~"),
            (F(3), Mods::SUPER, "<ESC>[13;9~"),
            // F1, F2 and F4 have no such collision and keep `CSI 1 ; mods letter`
            (F(1), CTRL, "<ESC>[1;5P"),
            (F(2), CTRL, "<ESC>[1;5Q"),
            (F(4), CTRL, "<ESC>[1;5S"),
        ];
        for &(code, mods, want) in rows {
            assert_eq!(enc(k, code, mods), want, "{code:?} {mods:?}");
        }
        assert_eq!(
            enc(TermMode::REPORT_ALL_KEYS_AS_ESC, F(3), CTRL),
            "<ESC>[13;5~"
        );
        // without a kitty flag it is still xterm's `CSI 1 ; mods R`
        assert_eq!(enc(TermMode::empty(), F(3), CTRL), "<ESC>[1;5R");
    }

    #[test]
    fn paste_brackets_and_strips_escapes() {
        // not bracketed: nothing to break out of, so only the line breaks change
        assert_eq!(
            encode_paste(TermMode::empty(), "a\nb\x1b[201~"),
            b"a\rb\x1b[201~"
        );
        assert_eq!(encode_paste(TermMode::empty(), "a\x03b"), b"a\x03b");
        assert_eq!(
            encode_paste(TermMode::BRACKETED_PASTE, "hi\nthere"),
            b"\x1b[200~hi\nthere\x1b[201~"
        );
        // an embedded end marker (even one reassembled from pieces) cannot survive
        assert_eq!(
            encode_paste(TermMode::BRACKETED_PASTE, "a\x1b[20\x1b[201~1~b"),
            b"\x1b[200~a[20[201~1~b\x1b[201~"
        );
        // some shells end a bracketed paste on ETX, so it goes too
        assert_eq!(
            encode_paste(TermMode::BRACKETED_PASTE, "a\x03b"),
            b"\x1b[200~ab\x1b[201~"
        );
        assert_eq!(
            encode_paste(TermMode::BRACKETED_PASTE, ""),
            b"\x1b[200~\x1b[201~"
        );
    }

    #[test]
    fn non_bracketed_paste_sends_newlines_as_carriage_returns() {
        // a raw-mode program must see Enter, not ^J
        for (text, want) in [
            ("a\nb", "a\rb"),
            ("a\r\nb", "a\rb"),
            ("a\rb", "a\rb"),
            ("a\n\r\nb", "a\r\rb"),
            ("a\r\nb\nc\rd", "a\rb\rc\rd"),
        ] {
            assert_eq!(
                encode_paste(TermMode::empty(), text),
                want.as_bytes(),
                "{text:?}"
            );
        }
        // bracketed paste keeps the line breaks as pasted
        assert_eq!(
            encode_paste(TermMode::BRACKETED_PASTE, "a\r\nb\nc\rd"),
            b"\x1b[200~a\r\nb\nc\rd\x1b[201~"
        );
    }

    #[test]
    fn focus_reports_only_when_asked() {
        assert_eq!(encode_focus(TermMode::empty(), true), None);
        assert_eq!(
            encode_focus(TermMode::FOCUS_IN_OUT, true),
            Some(b"\x1b[I".to_vec())
        );
        assert_eq!(
            encode_focus(TermMode::FOCUS_IN_OUT, false),
            Some(b"\x1b[O".to_vec())
        );
    }

    fn mouse(kind: MouseKind, button: u8, col: u16, row: u16, mods: Mods) -> MouseEv {
        MouseEv {
            kind,
            button,
            col,
            row,
            mods,
        }
    }

    fn menc(mode: TermMode, ev: MouseEv) -> Option<String> {
        encode_mouse(mode, ev).map(|b| {
            b.iter()
                .map(|&b| {
                    if b == ESC {
                        "<ESC>".to_string()
                    } else if b.is_ascii_graphic() {
                        (b as char).to_string()
                    } else {
                        format!("\\x{b:02x}")
                    }
                })
                .collect()
        })
    }

    #[test]
    fn mouse_sgr_encoding() {
        use MouseKind::*;
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            menc(sgr, mouse(Press, 0, 0, 0, NONE)).as_deref(),
            Some("<ESC>[<0;1;1M")
        );
        assert_eq!(
            menc(sgr, mouse(Release, 0, 0, 0, NONE)).as_deref(),
            Some("<ESC>[<0;1;1m")
        );
        assert_eq!(
            menc(sgr, mouse(Press, 2, 9, 4, NONE)).as_deref(),
            Some("<ESC>[<2;10;5M")
        );
        assert_eq!(
            menc(sgr, mouse(Press, 1, 299, 99, NONE)).as_deref(),
            Some("<ESC>[<1;300;100M"),
            "SGR has no coordinate limit"
        );
        assert_eq!(
            menc(sgr, mouse(Press, 0, 0, 0, SHIFT.union(ALT).union(CTRL))).as_deref(),
            Some("<ESC>[<28;1;1M")
        );
        assert_eq!(
            menc(sgr, mouse(Release, 2, 3, 3, CTRL)).as_deref(),
            Some("<ESC>[<18;4;4m")
        );
        assert_eq!(
            menc(sgr, mouse(ScrollUp, 0, 5, 6, NONE)).as_deref(),
            Some("<ESC>[<64;6;7M")
        );
        assert_eq!(
            menc(sgr, mouse(ScrollDown, 0, 5, 6, ALT)).as_deref(),
            Some("<ESC>[<73;6;7M")
        );
        assert_eq!(
            menc(sgr, mouse(Press, 3, 0, 0, NONE)),
            None,
            "buttons above 2 are not reported"
        );
    }

    #[test]
    fn mouse_x10_encoding() {
        use MouseKind::*;
        let x10 = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode_mouse(x10, mouse(Press, 0, 0, 0, NONE)),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        assert_eq!(
            encode_mouse(x10, mouse(Press, 2, 9, 4, NONE)),
            Some(vec![0x1b, b'[', b'M', 34, 42, 37])
        );
        assert_eq!(
            encode_mouse(x10, mouse(Release, 0, 9, 4, NONE)),
            Some(vec![0x1b, b'[', b'M', 35, 42, 37]),
            "release is button 3"
        );
        assert_eq!(
            encode_mouse(x10, mouse(Release, 2, 9, 4, CTRL)),
            Some(vec![0x1b, b'[', b'M', 51, 42, 37])
        );
        assert_eq!(
            encode_mouse(x10, mouse(ScrollUp, 0, 0, 0, NONE)),
            Some(vec![0x1b, b'[', b'M', 96, 33, 33])
        );
        assert_eq!(
            encode_mouse(x10, mouse(ScrollDown, 0, 0, 0, NONE)),
            Some(vec![0x1b, b'[', b'M', 97, 33, 33])
        );
        assert_eq!(
            encode_mouse(x10, mouse(Press, 0, 222, 0, NONE)).map(|v| v[4]),
            Some(255)
        );
        assert_eq!(
            encode_mouse(x10, mouse(Press, 0, 223, 0, NONE)),
            None,
            "column 224 does not fit a byte"
        );
        // UTF-8 encoding lifts the limit
        let utf8 = x10 | TermMode::UTF8_MOUSE;
        assert_eq!(
            encode_mouse(utf8, mouse(Press, 0, 299, 0, NONE)),
            Some("\x1b[M \u{14c}!".as_bytes().to_vec())
        );
    }

    #[test]
    fn mouse_mode_gating() {
        use MouseKind::*;
        let off = TermMode::SGR_MOUSE;
        for k in [Press, Release, Move, ScrollUp, ScrollDown] {
            assert_eq!(
                menc(off, mouse(k, 0, 1, 1, NONE)),
                None,
                "{k:?} with reporting off"
            );
        }
        let click = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let motion = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        // motion with a button held: drag and motion modes only
        assert_eq!(menc(click, mouse(Move, 0, 1, 1, NONE)), None);
        assert_eq!(
            menc(drag, mouse(Move, 0, 1, 1, NONE)).as_deref(),
            Some("<ESC>[<32;2;2M")
        );
        assert_eq!(
            menc(motion, mouse(Move, 2, 1, 1, NONE)).as_deref(),
            Some("<ESC>[<34;2;2M")
        );
        // motion with no button: motion mode only
        assert_eq!(menc(drag, mouse(Move, 3, 1, 1, NONE)), None);
        assert_eq!(
            menc(motion, mouse(Move, 3, 1, 1, NONE)).as_deref(),
            Some("<ESC>[<35;2;2M")
        );
        assert_eq!(
            menc(motion, mouse(Move, 3, 1, 1, SHIFT)).as_deref(),
            Some("<ESC>[<39;2;2M")
        );
        assert_eq!(
            menc(drag, mouse(Press, 0, 1, 1, NONE)).as_deref(),
            Some("<ESC>[<0;2;2M")
        );
    }

    #[test]
    fn alternate_scroll_becomes_arrows() {
        use MouseKind::*;
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            menc(alt, mouse(ScrollUp, 0, 1, 1, NONE)).as_deref(),
            Some("<ESC>[A<ESC>[A<ESC>[A")
        );
        assert_eq!(
            menc(alt, mouse(ScrollDown, 0, 1, 1, NONE)).as_deref(),
            Some("<ESC>[B<ESC>[B<ESC>[B")
        );
        assert_eq!(
            menc(alt | TermMode::APP_CURSOR, mouse(ScrollUp, 0, 1, 1, NONE)).as_deref(),
            Some("<ESC>OA<ESC>OA<ESC>OA")
        );
        // not on the primary screen, and a mouse mode wins over it
        assert_eq!(
            menc(TermMode::ALTERNATE_SCROLL, mouse(ScrollUp, 0, 1, 1, NONE)),
            None
        );
        assert_eq!(
            menc(
                alt | TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE,
                mouse(ScrollUp, 0, 1, 1, NONE)
            )
            .as_deref(),
            Some("<ESC>[<64;2;2M")
        );
        // clicks do not turn into arrows
        assert_eq!(menc(alt, mouse(Press, 0, 1, 1, NONE)), None);
    }
}
