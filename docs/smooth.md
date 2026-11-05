# Smooth: how the desktop moves

What was researched (Apple HIG motion/accessibility/loading, T3 Code `dd5f98ee`, Zed `e21ff1b`,
VS Code chat, SwiftUI defaults) and what it means for FEMME, in the order it is done. Every item
keeps two things as they are: frame time (`femme-desktop --bench`) and the update/deploy path.

## Rules

- Appear: 180 ms ease-out (`cubic-bezier(.32,.72,0,1)`, T3's drawer curve), fade plus a short move
  (a panel slides from its edge, a menu drops 4 px, a card rises 6 px). Leave: same curve back,
  from wherever the appearance was. Nothing longer than 200 ms, nothing bouncy.
- Motion runs at the display's own rate, as high as the display goes: a transition asks for the
  next frame every frame (`request_animation_frame`), nothing caps it (the only `with_max_fps` in
  the app are the two tiny spinners, 10 and 12 fps on purpose), and it dirties only what it moves
  (panels are `.cached` leaves). Budget: 120 Hz ProMotion is 8.3 ms a frame, 165 Hz is 6.1 ms; the
  bench's frame work is p99 4.4 ms on Cem's laptop at 165 Hz (below) and 5.9 ms software-rendered
  on the box.
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
6. [x] Transcript: rows of items created in the last second fade in (whole row).
7. [x] Transcript: measure the markdown re-parse per delta; only move it off the UI thread if it
   is over a frame's budget on a long message. (40 KB message: 1.75 ms release, 6.5 ms debug, at
   most once per frame: under budget, so it stays as it is; `markdown::tests::parse_time`.)
8. [x] Smooth scroll to the end when the pill is clicked (one 200 ms ease-out, not a jump).
9. [x] Sidebar collapse: 160 ms width transition (the column alone moves; the frame follows it).
10. [x] Bench before/after (`--bench 15 --panes 4`, Xvfb + lavapipe on the box, quick profile):
    frame work p50 3.01 → 3.06 ms, p99 5.67 → 5.91 ms, max 7.46 → 6.13 ms (noise; both PASS the
    10 ms target). `docs/desktop-ui.md` has the motion rules.

Skipped: a pulse on the working line (would re-render the transcript every frame, for a signal the
star already gives); per-word text fades (none of the four apps does it); any spring with bounce.

## Seen, not only tested

Under Xvfb + lavapipe with `FEMME_MOTION_SCALE=10` (burst screenshots ~120 ms apart): the menu
half-transparent as it drops, the panel half-transparent as it slides, the sidebar box shrinking with
its column; against a scratch daemon and the fake `claude` (`slow`: 50 lines over 10 s): the user
card and the streamed rows faint on their first frame and settled on the next, the view holding
after a wheel-up mid-stream while the "↓ new" pill fades in, and after a click on it the list at the
end with the pill fading out; with the glide on the same scale, the eight frames after the click
show it moving fast then settling (ease-out) with the pill fading over it.

Measured on Cem's laptop (Omarchy, 20 cores, 2560x1440 display at 165 Hz, window 1261x1390,
quick profile, `--bench 15 --panes 4`, 2026-11-05):

```
frames 1872 (124.3 fps), late 1 (0.05%)
                       p50     p90     p99   p99.9     max   (ms)
frame work            2.42    3.41    4.42    5.53   10.17
  draw                2.29    2.53    3.50    4.50    8.08
  present             0.41    0.49    0.73    0.92    2.03
frame interval        7.70    9.61   14.45   15.55   16.12
```

p99 frame work 4.42 ms: inside the 6.1 ms a 165 Hz frame allows and the 8.3 ms of 120 Hz, with one
late frame in fifteen seconds. The four 60 Hz terminal panes set the redraw cadence here (the app
draws only when something changed), so 124 fps is the content's rate, not a ceiling.