//! The board's look (board/src/index.html, assets/src/board.css): a pink desk, ink, paper, one neon
//! accent. Truecolor when the terminal says so, else the nearest xterm-256 colours.
use ratatui::style::{Color, Modifier, Style};

use crate::herdr::types::Status;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub desk: Color,
    pub paper: Color,
    pub panel: Color,
    pub ink: Color,
    pub muted: Color,
    pub neon: Color,
    pub work: Color,
    pub pause: Color,
    pub pr: Color,
}

/// name, 24-bit value, xterm-256 fallback
pub const TOKENS: [(&str, (u8, u8, u8), u8); 9] = [
    ("desk", (0xF3, 0x86, 0xA1), 211),
    ("paper", (0xFE, 0xFE, 0xFE), 231),
    ("panel", (0xE5, 0xE5, 0xE5), 254),
    ("ink", (0x1E, 0x1E, 0x1E), 234),
    ("muted", (0x8A, 0x8A, 0x8A), 245),
    ("neon", (0xFF, 0x4F, 0xB4), 205),
    ("work", (0x03, 0xAA, 0x5C), 35),
    ("pause", (0xF2, 0xB3, 0x3D), 220),
    ("pr", (0x09, 0xAE, 0xA1), 37),
];

impl Theme {
    /// `force` from the command line; otherwise COLORTERM decides (truecolor/24bit).
    pub fn detect(force: Option<bool>) -> Theme {
        let truecolor = force.unwrap_or_else(|| {
            matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor") | Ok("24bit"))
        });
        Theme::build(truecolor)
    }

    pub fn build(truecolor: bool) -> Theme {
        let c = |i: usize| {
            let (_, (r, g, b), idx) = TOKENS[i];
            if truecolor {
                Color::Rgb(r, g, b)
            } else {
                Color::Indexed(idx)
            }
        };
        Theme { desk: c(0), paper: c(1), panel: c(2), ink: c(3), muted: c(4), neon: c(5), work: c(6), pause: c(7), pr: c(8) }
    }

    /// The status dot: shape and colour. Yellow means "needs you", as on the board.
    pub fn dot(&self, s: Status) -> (&'static str, Color) {
        match s {
            Status::Working => ("●", self.work),
            Status::Blocked | Status::Done => ("●", self.pause),
            Status::Idle => ("○", self.muted),
            Status::Unknown => ("·", self.muted),
        }
    }

    pub fn status_color(&self, s: Status) -> Color {
        self.dot(s).1
    }

    pub fn sidebar(&self) -> Style {
        Style::new().bg(self.ink).fg(self.paper)
    }
    pub fn selected(&self) -> Style {
        Style::new().bg(self.neon).fg(self.ink).add_modifier(Modifier::BOLD)
    }
    pub fn header(&self) -> Style {
        Style::new().bg(self.ink).fg(self.neon).add_modifier(Modifier::BOLD)
    }
    pub fn status_bar(&self) -> Style {
        Style::new().bg(self.desk).fg(self.ink)
    }
    pub fn accent(&self) -> Style {
        Style::new().fg(self.neon).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_has_both_forms() {
        let t = Theme::build(true);
        assert!(matches!(t.neon, Color::Rgb(0xFF, 0x4F, 0xB4)));
        let f = Theme::build(false);
        assert!(matches!(f.neon, Color::Indexed(205)));
        for (_, _, idx) in TOKENS {
            assert!(idx >= 16, "fallbacks avoid the 16 terminal-defined colours");
        }
    }

    #[test]
    fn needs_you_is_yellow() {
        let t = Theme::build(true);
        assert_eq!(t.dot(Status::Blocked).1, t.pause);
        assert_eq!(t.dot(Status::Done).1, t.pause);
        assert_eq!(t.dot(Status::Working).1, t.work);
    }
}
