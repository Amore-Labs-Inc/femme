//! The context XP bar above the composer's `send`: how full the Claude session's context window
//! is, drawn like a World of Warcraft experience bar (a thin notched bar that fills up) with the
//! numbers in its tooltip. When the session says where Claude compacts by itself (its
//! `autoCompactWindow`, often far below the window), the bar is full there, not at the window. A
//! compaction empties the bar and is the level up: the level is the
//! number of compactions plus one, and a neon ring pulses once around the bar when it happens,
//! nothing bigger than that. The numbers and the level-up bookkeeping are pure, so they are
//! tested without a window.

use std::time::{Duration, Instant};

use femme_proto::state::Context;
use femme_proto::Sid;
use gpui::{
    div, ease_out_quint, px, relative, Animation, AnimationExt as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Rgba, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::tooltip::Tooltip;

use crate::theme::{
    accent_text, bad, ink, neon, panel, paper, FONT_MONO, FONT_PIXEL, META_SIZE, RULE,
};
use crate::ui::ruled;

/// The bar's height: the permission square's size, so the two corners match.
pub const H: Pixels = px(10.);
/// Notches: the bar is divided into this many segments by ink ticks.
const SEGMENTS: usize = 5;
/// Past this share the fill turns red: a compaction is near.
const WARN: f32 = 0.9;
/// How long the level-up ring plays.
const RING: Duration = Duration::from_millis(700);

/// What fills the bar: the point where the agent compacts by itself when it said one, else the
/// window; 0 while neither is known.
fn full(c: &Context) -> u64 {
    c.compact_at.filter(|n| *n > 0).unwrap_or(c.window)
}

/// How far the context is on its way to the next compaction, 0 to 1; 0 while that is unknown.
pub fn fill(c: &Context) -> f32 {
    let full = full(c);
    if full == 0 {
        return 0.;
    }
    (c.used as f32 / full as f32).clamp(0., 1.)
}

/// The level: one more than the compactions.
pub fn level(c: &Context) -> u32 {
    c.compactions + 1
}

/// The fill colour: pink, red once a compaction is near.
pub fn color(c: &Context) -> Rgba {
    if fill(c) >= WARN {
        bad()
    } else {
        accent_text()
    }
}

/// The tooltip's rows, label and value, in the game's order: the numbers, where Claude compacts
/// (when it said), the share, what is left until then, the level. `?` while a size is unknown.
pub fn rows(c: &Context) -> Vec<(String, String)> {
    let full = full(c);
    let or = |known: bool, s: String| if known { s } else { "?".to_owned() };
    let level = match c.compactions {
        0 => format!("{} (never compacted)", level(c)),
        1 => format!("{} (1 compaction)", level(c)),
        n => format!("{} ({n} compactions)", level(c)),
    };
    let mut rows = vec![(
        "current context".into(),
        format!(
            "{} / {}",
            thousands(c.used),
            or(c.window > 0, thousands(c.window))
        ),
    )];
    if let Some(at) = c.compact_at.filter(|n| *n > 0) {
        rows.push(("compacts at".into(), thousands(at)));
    }
    rows.extend([
        (
            "level progress".into(),
            or(full > 0, format!("{:.1}%", fill(c) * 100.)),
        ),
        (
            "room left".into(),
            or(full > 0, thousands(full.saturating_sub(c.used))),
        ),
        ("level".into(), level),
    ]);
    rows
}

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Remembers the level last shown per session, so a compaction that arrives while the session is
/// on screen plays the ring once and a session opened at level 7 does not.
#[derive(Default)]
pub struct LevelUp {
    seen: Option<(Sid, u32)>,
    /// The level reached and when.
    rang: Option<(u32, Instant)>,
}

impl LevelUp {
    /// Called every frame with the selected session and its level: the level whose ring should
    /// show, for [`RING`] after it was reached (the ring's last frame is invisible; this only
    /// stops building it).
    pub fn step(&mut self, sid: Option<Sid>, level: Option<u32>, now: Instant) -> Option<u32> {
        match (self.seen, sid.zip(level)) {
            (Some((was, from)), Some((is, to))) if was == is && to > from => {
                self.rang = Some((to, now));
            }
            (Some((was, _)), Some((is, _))) if was != is => self.rang = None,
            (_, None) => self.rang = None,
            _ => {}
        }
        self.seen = sid.zip(level);
        self.rang
            .filter(|(l, at)| Some(*l) == level && now.duration_since(*at) < RING)
            .map(|(l, _)| l)
    }
}

/// The bar for `c`, stretched to its column; `ring` is the level just reached while its ring
/// should play.
pub fn bar(c: Context, ring: Option<u32>) -> impl IntoElement {
    let rows = rows(&c);
    let ticks = (1..SEGMENTS).map(|i| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .w(RULE)
            .left(relative(i as f32 / SEGMENTS as f32))
            .bg(ink())
    });
    // A neon ring that grows from the bar's size to about three times it and fades, centred on
    // the bar: the whole show stays within a few pixels of the bar.
    let ring = ring.map(|level| {
        div().absolute().with_animation(
            ("c-xp-ring", level),
            Animation::new(RING).with_easing(ease_out_quint()),
            |el, t| {
                let d = px(14. + 22. * t);
                el.size(d)
                    .left(relative(0.5))
                    .top(relative(0.5))
                    .ml(-d / 2.)
                    .mt(-d / 2.)
                    .rounded_full()
                    .border_2()
                    .border_color(neon())
                    .opacity(1. - t)
            },
        )
    });
    ruled(div().id("c-xp").relative().w_full().h(H).bg(panel()))
        .child(div().h_full().w(relative(fill(&c))).bg(color(&c)))
        .children(ticks)
        .children(ring)
        .tooltip(move |window, cx| {
            let rows = rows.clone();
            Tooltip::element(move |_, _| card(&rows)).build(window, cx)
        })
}

/// The tooltip: the game's "Experience" box, in FEMME's dress.
fn card(rows: &[(String, String)]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(6.))
        .bg(paper())
        .text_color(ink())
        .child(
            div()
                .font_family(FONT_PIXEL)
                .text_size(px(18.))
                .child("context"),
        )
        .children(rows.iter().map(|(k, v)| {
            div()
                .flex()
                .gap(px(12.))
                .justify_between()
                .font_family(FONT_MONO)
                .text_size(META_SIZE)
                .child(k.clone())
                .child(v.clone())
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::Id;

    fn ctx(used: u64, window: u64, compactions: u32) -> Context {
        Context {
            used,
            window,
            compactions,
            compact_at: None,
        }
    }

    /// The same, for a session that said where Claude compacts.
    fn ctx_at(used: u64, window: u64, compact_at: u64) -> Context {
        Context {
            compact_at: Some(compact_at),
            ..ctx(used, window, 0)
        }
    }

    #[test]
    fn the_bar_is_full_where_claude_compacts_not_at_the_window() {
        // 451,479 of a 1M window: not half full, yet 90% of the way to the compaction
        let plain = ctx(451_479, 1_000_000, 0);
        assert_eq!((fill(&plain) * 100.).round(), 45.);
        assert_eq!(color(&plain), accent_text());
        let tuned = ctx_at(451_479, 1_000_000, 501_643);
        assert_eq!((fill(&tuned) * 1000.).round(), 900.);
        assert_eq!(color(&tuned), bad());
        assert_eq!(fill(&ctx_at(600_000, 1_000_000, 501_643)), 1.);
        // the point alone is enough: the window may still be unknown
        assert_eq!(fill(&ctx_at(25_000, 0, 50_000)), 0.5);
        let rows = rows(&tuned);
        let rows: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (&**k, &**v)).collect();
        assert_eq!(
            rows,
            [
                ("current context", "451,479 / 1,000,000"),
                ("compacts at", "501,643"),
                ("level progress", "90.0%"),
                ("room left", "50,164"),
                ("level", "1 (never compacted)"),
            ]
        );
    }

    #[test]
    fn fill_is_the_share_of_the_window_clamped_and_zero_while_unknown() {
        assert_eq!(fill(&ctx(50, 200, 0)), 0.25);
        assert_eq!(fill(&ctx(300, 200, 0)), 1.);
        assert_eq!(fill(&ctx(300, 0, 0)), 0.);
    }

    #[test]
    fn the_level_is_one_more_than_the_compactions() {
        assert_eq!(level(&ctx(0, 0, 0)), 1);
        assert_eq!(level(&ctx(0, 0, 4)), 5);
    }

    #[test]
    fn the_fill_turns_red_near_a_compaction() {
        assert_eq!(color(&ctx(89, 100, 0)), accent_text());
        assert_eq!(color(&ctx(90, 100, 0)), bad());
    }

    #[test]
    fn the_rows_carry_the_games_numbers() {
        let rows = rows(&ctx(10_328, 12_900, 2));
        let rows: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (&**k, &**v)).collect();
        assert_eq!(
            rows,
            [
                ("current context", "10,328 / 12,900"),
                ("level progress", "80.1%"),
                ("room left", "2,572"),
                ("level", "3 (2 compactions)"),
            ]
        );
        let unknown = rows_of(&ctx(4_300, 0, 0));
        assert_eq!(unknown[0].1, "4,300 / ?");
        assert_eq!(unknown[1].1, "?");
        assert_eq!(unknown[2].1, "?");
        assert_eq!(unknown[3].1, "1 (never compacted)");
        assert_eq!(rows_of(&ctx(1, 2, 1))[3].1, "2 (1 compaction)");
    }

    fn rows_of(c: &Context) -> Vec<(String, String)> {
        rows(c)
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_ring_plays_once_for_a_compaction_seen_on_screen() {
        let a = Id::new();
        let b = Id::new();
        let t0 = Instant::now();
        let mut up = LevelUp::default();
        // opened at level 3: no ring
        assert_eq!(up.step(Some(a), Some(3), t0), None);
        // a compaction while shown: the ring, for RING
        assert_eq!(up.step(Some(a), Some(4), t0), Some(4));
        assert_eq!(up.step(Some(a), Some(4), t0 + RING / 2), Some(4));
        assert_eq!(up.step(Some(a), Some(4), t0 + RING), None);
        // switching to a session at a higher level is not a level up
        assert_eq!(up.step(Some(b), Some(9), t0), None);
        assert_eq!(up.step(Some(b), Some(9), t0), None);
        // no session, then back: nothing rings
        assert_eq!(up.step(None, None, t0), None);
        assert_eq!(up.step(Some(a), Some(4), t0), None);
    }
}
