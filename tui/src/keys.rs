//! Keys to actions, by mode. Pure, so the table is testable without a terminal.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::state::Mode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Jump(usize),
    Open,
    Attach,
    Compose,
    Send,
    Newline,
    Esc,
    New,
    Close,
    Filter,
    Refresh,
    ToggleShells,
    ToggleSidebar,
    Help,
    Quit,
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Paste(String),
    Confirm,
    Tab,
}

pub fn map(ev: &Event, mode: &Mode) -> Option<Action> {
    match ev {
        Event::Paste(s) => Some(Action::Paste(s.clone())),
        Event::Key(k) if k.kind != KeyEventKind::Release => key(k, mode),
        _ => None,
    }
}

fn key(k: &KeyEvent, mode: &Mode) -> Option<Action> {
    use Action::*;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('c') {
        return Some(Quit);
    }
    let a = match mode {
        Mode::Normal => match k.code {
            KeyCode::Char('q') => Quit,
            KeyCode::Char('j') | KeyCode::Down => Down,
            KeyCode::Char('k') | KeyCode::Up => Up,
            KeyCode::Char(c @ '1'..='9') => Jump(c as usize - '0' as usize),
            KeyCode::Enter => Open,
            KeyCode::Char('a') => Attach,
            KeyCode::Char('i') => Compose,
            KeyCode::Tab => Tab,
            KeyCode::Char('n') => New,
            KeyCode::Char('x') => Close,
            KeyCode::Char('/') => Filter,
            KeyCode::Char('r') => Refresh,
            KeyCode::Char('s') => ToggleShells,
            KeyCode::Char('b') => ToggleSidebar,
            KeyCode::Char('?') => Help,
            KeyCode::Esc => Esc,
            _ => return None,
        },
        Mode::Insert => match k.code {
            KeyCode::Enter if k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => Newline,
            KeyCode::Enter => Send,
            KeyCode::Char('j') if ctrl => Newline,
            KeyCode::Esc => Esc,
            KeyCode::Backspace => Backspace,
            KeyCode::Delete => Delete,
            KeyCode::Left => Left,
            KeyCode::Right => Right,
            KeyCode::Home => Home,
            KeyCode::End => End,
            KeyCode::Tab => Tab,
            KeyCode::Char(c) if !ctrl => Char(c),
            _ => return None,
        },
        Mode::Filter | Mode::New => match k.code {
            KeyCode::Enter => Confirm,
            KeyCode::Esc => Esc,
            KeyCode::Backspace => Backspace,
            KeyCode::Up => Up,
            KeyCode::Down => Down,
            KeyCode::Tab => Tab,
            KeyCode::Char(c) if !ctrl => Char(c),
            _ => return None,
        },
        Mode::Help => Esc,
        Mode::Close => match k.code {
            KeyCode::Enter | KeyCode::Char('y') => Confirm,
            _ => Esc,
        },
    };
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn normal_mode_table() {
        let n = Mode::Normal;
        assert_eq!(map(&press(KeyCode::Char('j'), KeyModifiers::NONE), &n), Some(Action::Down));
        assert_eq!(map(&press(KeyCode::Up, KeyModifiers::NONE), &n), Some(Action::Up));
        assert_eq!(map(&press(KeyCode::Char('3'), KeyModifiers::NONE), &n), Some(Action::Jump(3)));
        assert_eq!(map(&press(KeyCode::Char('q'), KeyModifiers::NONE), &n), Some(Action::Quit));
        assert_eq!(map(&press(KeyCode::Char('z'), KeyModifiers::NONE), &n), None);
    }

    #[test]
    fn insert_mode_sends_on_enter_and_breaks_lines_with_modifiers() {
        let i = Mode::Insert;
        assert_eq!(map(&press(KeyCode::Enter, KeyModifiers::NONE), &i), Some(Action::Send));
        assert_eq!(map(&press(KeyCode::Enter, KeyModifiers::SHIFT), &i), Some(Action::Newline));
        assert_eq!(map(&press(KeyCode::Char('j'), KeyModifiers::CONTROL), &i), Some(Action::Newline));
        assert_eq!(map(&press(KeyCode::Char('q'), KeyModifiers::NONE), &i), Some(Action::Char('q')));
        assert_eq!(map(&press(KeyCode::Char('c'), KeyModifiers::CONTROL), &i), Some(Action::Quit));
    }

    #[test]
    fn releases_are_ignored() {
        let mut k = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(map(&Event::Key(k), &Mode::Normal), None);
    }
}
