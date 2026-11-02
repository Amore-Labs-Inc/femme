//! What the keyboard and the wheel say to a terminal, in the proto's words. The holder turns a
//! [`Key`] into bytes from the program's live modes, so nothing here knows about escape codes.

use femme_proto::term::{Key, KeyCode, Modes, Mods};
use gpui::{Keystroke, Modifiers, Pixels, Point};

/// The key a keystroke stands for. `None` for what is not the program's: chords with the platform
/// key (`cmd`, `super`) belong to the app, `ctrl+shift+c` is copy (there is no selection yet, so it
/// must not become `^C`), and modifier-only presses and keys the proto has no code for.
pub fn key(ks: &Keystroke) -> Option<Key> {
    let m = ks.modifiers;
    if m.platform || (m.control && m.shift && ks.key == "c") {
        return None;
    }
    let code = match ks.key.as_str() {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "escape" => KeyCode::Escape,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        name => match name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            Some(n) if (1..=24).contains(&n) => KeyCode::F(n),
            _ => KeyCode::Char(typed(ks)?),
        },
    };
    Some(Key {
        code,
        mods: mods(&m),
    })
}

/// The proto's modifier flags for gpui's.
pub fn mods(m: &Modifiers) -> Mods {
    let mut mods = Mods::empty();
    mods.set(Mods::SHIFT, m.shift);
    mods.set(Mods::ALT, m.alt);
    mods.set(Mods::CTRL, m.control);
    mods.set(Mods::SUPER, m.platform);
    mods
}

/// The character of a key that is not a named one: what was typed (shift and layout are already in
/// it), unless ctrl or alt is held, which name the key and not the text. One character only.
// ponytail: a `key_char` of several characters (a composed string) is dropped, and ctrl/alt chords
// name the key as the layout reports it (macOS Option is always meta). Upgrade: send such text as
// a paste.
fn typed(ks: &Keystroke) -> Option<char> {
    let m = ks.modifiers;
    let chord = m.control || m.alt;
    let text = ks.key_char.as_deref().filter(|_| !chord).unwrap_or(&ks.key);
    let mut chars = text.chars();
    let c = chars.next().filter(|_| chars.next().is_none())?;
    Some(if chord && m.shift {
        c.to_ascii_uppercase()
    } else {
        c
    })
}

/// `ctrl+shift+v` (the terminal convention) and `cmd+v` (macOS) paste.
pub fn is_paste(ks: &Keystroke) -> bool {
    let m = ks.modifiers;
    ks.key == "v" && ((m.control && m.shift && !m.alt && !m.platform) || (m.platform && !m.control))
}

/// Whether a wheel turn is for the program (it reports the mouse, or sits on the alternate screen
/// where the holder turns a notch into arrow keys) rather than for the pane's scrollback.
pub fn wheel_is_the_programs(modes: Modes) -> bool {
    modes.intersects(Modes::MOUSE_CLICK | Modes::MOUSE_DRAG | Modes::MOUSE_MOTION)
        || modes.contains(Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL)
}

/// Adds `lines` of wheel travel to `acc` and takes out the whole steps of `per` lines each (signed:
/// positive is up), so a trackpad's fractions add up instead of being lost.
pub fn steps(acc: &mut f32, lines: f32, per: f32) -> i32 {
    *acc += lines;
    let n = (*acc / per).trunc();
    *acc -= n * per;
    n as i32
}

/// The cell under `at` in a grid of `cols x rows` cells whose top left is `origin`, clamped to it.
pub fn cell(
    at: Point<Pixels>,
    origin: Point<Pixels>,
    (cell_w, line_h): (Pixels, Pixels),
    (cols, rows): (u16, u16),
) -> (u16, u16) {
    let idx = |d: Pixels, size: Pixels, n: u16| {
        ((d / size).floor().max(0.) as u16).min(n.saturating_sub(1))
    };
    (
        idx(at.x - origin.x, cell_w, cols),
        idx(at.y - origin.y, line_h, rows),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    /// `Keystroke::parse` syntax: modifiers, the key, optionally `->` and the text it types.
    fn ks(s: &'static str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    fn k(code: KeyCode, mods: Mods) -> Option<Key> {
        Some(Key { code, mods })
    }

    #[test]
    fn keystrokes_become_keys() {
        use KeyCode::*;
        const NONE: Mods = Mods::empty();
        #[rustfmt::skip]
        let table: &[(Keystroke, Option<Key>)] = &[
            // Text: what was typed, shift and layout included.
            (ks("a->a"), k(Char('a'), NONE)),
            (ks("shift-a->A"), k(Char('A'), Mods::SHIFT)),
            (ks("!->!"), k(Char('!'), NONE)),
            (ks("1"), k(Char('1'), NONE)),
            (ks("\u{e9}->\u{e9}"), k(Char('\u{e9}'), NONE)),
            (ks("space-> "), k(Char(' '), NONE)),
            // Chords name the key; the holder makes the control byte.
            (ks("ctrl-c"), k(Char('c'), Mods::CTRL)),
            (ks("ctrl-shift-a"), k(Char('A'), Mods::CTRL | Mods::SHIFT)),
            (ks("ctrl-space"), k(Char(' '), Mods::CTRL)),
            (ks("alt-b->b"), k(Char('b'), Mods::ALT)),
            (ks("alt-b->\u{222b}"), k(Char('b'), Mods::ALT)),
            // Named keys.
            (ks("enter"), k(Enter, NONE)),
            (ks("shift-tab"), k(Tab, Mods::SHIFT)),
            (ks("backspace"), k(Backspace, NONE)),
            (ks("escape"), k(Escape, NONE)),
            (ks("up"), k(Up, NONE)),
            (ks("ctrl-left"), k(Left, Mods::CTRL)),
            (ks("home"), k(Home, NONE)),
            (ks("pagedown"), k(PageDown, NONE)),
            (ks("delete"), k(Delete, NONE)),
            (ks("f5"), k(F(5), NONE)),
            (ks("shift-f12"), k(F(12), Mods::SHIFT)),
            // Not the program's.
            (ks("cmd-k"), None),
            (ks("cmd-v"), None),
            (ks("ctrl-shift-c"), None),
            (ks("capslock"), None),
            (ks("f99"), None),
            (ks("a->ab"), None),
        ];
        for (stroke, want) in table {
            assert_eq!(key(stroke), *want, "{stroke:?}");
        }
    }

    #[test]
    fn the_paste_chords() {
        assert!(is_paste(&ks("ctrl-shift-v")));
        assert!(is_paste(&ks("cmd-v")));
        assert!(!is_paste(&ks("ctrl-v")));
        assert!(!is_paste(&ks("v->v")));
        assert!(!is_paste(&ks("ctrl-shift-alt-v")));
    }

    #[test]
    fn wheel_goes_to_the_program_that_asked_for_it() {
        assert!(!wheel_is_the_programs(Modes::empty()));
        assert!(!wheel_is_the_programs(Modes::ALT_SCREEN));
        assert!(!wheel_is_the_programs(Modes::ALTERNATE_SCROLL));
        assert!(wheel_is_the_programs(
            Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL
        ));
        assert!(wheel_is_the_programs(Modes::MOUSE_CLICK | Modes::SGR_MOUSE));
        assert!(wheel_is_the_programs(Modes::MOUSE_MOTION));
    }

    #[test]
    fn wheel_fractions_add_up() {
        let mut acc = 0.;
        // A trackpad: tenths of a line.
        let got: i32 = (0..25).map(|_| steps(&mut acc, 0.1, 1.)).sum();
        assert_eq!(got, 2);
        assert!((acc - 0.5).abs() < 1e-4, "{acc}");
        // A wheel notch is three lines; the program is told once per notch.
        let mut acc = 0.;
        assert_eq!(steps(&mut acc, 3., 3.), 1);
        assert_eq!(steps(&mut acc, -3., 3.), -1);
        assert_eq!(steps(&mut acc, 1., 3.), 0);
        let mut acc = 0.;
        assert_eq!(steps(&mut acc, -7., 1.), -7);
    }

    #[test]
    fn cells_under_the_pointer_are_clamped_to_the_grid() {
        let (origin, size) = (point(px(100.), px(50.)), (px(10.), px(20.)));
        let grid = (80, 24);
        assert_eq!(cell(point(px(100.), px(50.)), origin, size, grid), (0, 0));
        assert_eq!(cell(point(px(135.), px(111.)), origin, size, grid), (3, 3));
        assert_eq!(cell(point(px(10.), px(0.)), origin, size, grid), (0, 0));
        assert_eq!(
            cell(point(px(5000.), px(5000.)), origin, size, grid),
            (79, 23)
        );
    }
}
