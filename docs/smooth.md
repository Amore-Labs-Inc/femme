# Smooth: how the desktop moves

What was researched (Apple HIG motion/accessibility/loading, T3 Code `dd5f98ee`, Zed `e21ff1b`,
VS Code chat, SwiftUI defaults) and what it means for FEMME, in the order it is done. Every item
keeps two things as they are: frame time (`femme-desktop --bench`) and the update/deploy path.

## Rules

- Appear: 180 ms ease-out (`cubic-bezier(.32,.72,0,1)`, T3's drawer curve), fade plus a short move
  (a panel slides from its edge, a menu drops 4 px, a card rises 6 px). Leave: same curve back,
  from wherever the appearance was. Nothing longer than 200 ms, nothing bouncy.
- Motion runs at the display's own rate: no frame caps on a transition, and a transition only
  dirties what it moves (panels are `.cached` leaves), so it is cheap enough to stay at full rate.
- Hover, selection, tab switch, typing: instant. Motion never blocks input; it can be reversed
  mid-way (`Presence` does this).
- Streamed text appends; nothing per token is animated, laid out again, or re-scrolled by hand. The
  list follows the tail (`FollowMode::Tail`); any upward scroll by the user releases it; the "↓"
  pill shows 150 ms after that and goes at once when they are back at the end.
- New rows of the current turn fade in once (150 ms, whole row, never per word); history does not.
- A working indicator is slow and low-contrast (the 1 Hz star stays: it is the house style).
- Under "reduce motion" everything above becomes an instant change (`cx.reduce_motion()`,
  honoured inside `Presence`).

## Items

1. [x] `desktop/src/motion.rs`: one `appear(id, present)` helper over gpui-base `Presence`.
2. [x] Right panel: slides in 16 px from the right and fades; closes the same way (the tab stays
   drawn while it leaves).
3. [x] Top-bar menu: drops 4 px and fades; leaves the same way.
4. [x] Accounts card and add-project palette: scrim fades, card rises 6 px and fades.
5. [x] Transcript "↓" pill: 150 ms show debounce, fade in, instant hide.
6. [ ] Transcript: rows of items created in the last second fade in (whole row).
7. [x] Transcript: measure the markdown re-parse per delta; only move it off the UI thread if it
   is over a frame's budget on a long message. (40 KB message: 1.75 ms release, 6.5 ms debug, at
   most once per frame: under budget, so it stays as it is; `markdown::tests::parse_time`.)
8. [ ] Smooth scroll to the end when the pill is clicked (one 200 ms ease-out, not a jump).
9. [x] Sidebar collapse: 160 ms width transition (the column alone moves; the frame follows it).
10. [ ] Bench before/after; `docs/desktop-ui.md` gets the motion rules above.

Skipped: a pulse on the working line (would re-render the transcript every frame, for a signal the
star already gives); per-word text fades (none of the four apps does it); any spring with bounce.
