# femme-desktop (prototype P4)

An all-Rust desktop shell for FEMME on [gpui](https://crates.io/crates/gpui-pre) and
[gpui-component](https://crates.io/crates/gpui-component): N terminal panes fed by
`femme_proto::term::Frame`s next to the transcript of a session, in FEMME's retro skin
(desk pink with a dot grid, paper cards, 1.2px ink rules, black VT323 title bars, hard 3px shadows).
It exists to measure one thing: whether frame time stays under budget (p99 <= 10 ms on a 120 Hz
display, <= 16 ms on 60 Hz) while every terminal repaints at 60 Hz.
The app shows the sessions of a `femmed` through the `femme-client` crate (see "The frame" and "Talking
to a daemon"); without `--daemon`/`--ssh` it shows the same frame with "no daemon configured" and nothing
in it. The synthetic terminals (`src/terminal/synth.rs`) exist for `--bench` only: they speak the real
wire protocol (frames up, `Open`/`Ack`/`Resize` down) and the client cache is the real
`femme_term::client::RowCache`.

## Build and run

Needs stable Rust (tested with 1.97.1) and a GPU with Vulkan (Linux) or Metal (macOS).

```sh
cd desktop
cargo run --release -- [--daemon ADDR:PORT --fingerprint HEX64] [--ssh HOST]    # the app (see "Talking to a daemon")
# --bench needs gpui's frame profiler, which only a `--features bench` build contains:
cargo run --release --features bench -- --bench 20      # measure 20 s (after a 2.5 s warm-up), print the report, exit 0
cargo run --release --features bench -- --bench 20 --panes 1    # one pane
cargo run --release --features bench -- --bench 20 --target-ms 10 --panes 5 --mode fire,fire,fire,fire,cat
cargo run --release --features bench -- --bench 20 --target-ms 10 --panes 5 --mode cat --full-height   # text-shaping stress
```

| Flag | Default | Meaning |
|---|---|---|
| `--bench SECONDS` | off | Measure that many seconds, print percentiles, quit. Needs a `--features bench` build (without it the flag is refused). |
| `--target-ms MS` | the display's: 10 at >= 100 Hz, else 16 | p99 frame-work target of the verdict, whatever the display's refresh. |
| `--panes N` | 4 | Synthetic terminal panes of the benchmark. |
| `--hz H` | 60 | Frame rate of each synthetic terminal. |
| `--mode a,b,..` | `fire` | What the panes show, repeated over the panes (`--panes 5 --mode fire,cat` is fire, cat, fire, cat, fire). `fire`: DOOM fire on the alt screen, every cell rewritten every frame (repaint worst case). `shell`: one new line per frame (the delta path). `cat`: a screenful of new, unique text every frame (the text-shaping worst case). |
| `--full-height` | off | The benchmark's panes take the whole window (no frame around them): tall panes. |
| `--seed S` | 1 | Seed of every synthetic source. |
| `--size WxH` | 1440x900 | Requested window size in logical pixels. A compositor may override it; the bench report prints the real size. |

`--panes`, `--hz`, `--mode`, `--full-height`, `--target-ms` and `--seed` only drive `--bench`: the app
itself has no synthetic panes. `--bench` ignores a daemon that only the environment or the env file
names, and refuses the `--daemon` and `--ssh` flags.

## The frame

```text
+--------------------------------------------------------------------+
| femme v   quic 23 ms                                               |  top bar, 36 px
+-----------+------------------------------------------+-------------+
| sessions <| transcript (Claude)  or  terminal (shell)| terminal    |  strip, 28 px:
|           +------------------------------------------+ files       |  hover opens the panel
|           | composer                                 |             |
+-----------+------------------------------------------+-------------+
```

- **Top bar**: the `femme` menu (version, check for updates, accounts, reconnect, quit) and the link's
  status, nothing else.
- **Sessions**: the left column. `ctrl-b` or the `‹` in its title bar collapses it to a 28 px strip of status
  dots (a click on a dot opens that session); it draws its own box and the frame follows its width.
- **Centre**: what the current session needs. A Claude session shows its transcript with the composer
  under it. A shell session (and any session in TUI mode) shows its terminal filling the centre, "starting"
  until the pane is attached; a closed one says so. With no session the centre is a card that says why:
  `no daemon configured`, `connecting`, `offline: <reason>`, or `no session`.
- **Right panel**: a 28 px strip on the right edge with two icons, terminal and files. The pointer resting
  on the strip opens the panel (320 to 480 px, drag its left edge); it floats over the centre, so nothing
  reflows and no terminal is resized. It closes when the pointer has left it and nothing inside has
  keyboard focus, on `ctrl-k`, or on its `x`. A click on an icon opens that tab with the keyboard in it.
- **Accounts**: femme > accounts opens a modal layer over the centre; `escape` closes it.
- **At start**: once connected, the newest live Claude session is shown; with none, a headless Claude
  session is started in the box's home directory; if the box cannot, a shell (`src/session.rs`). The
  link's status line says what is being started, or why nothing could be.

Keys (`cmd` instead of `ctrl` on macOS): `ctrl-b` sidebar, `ctrl-j` terminal tab (opens it with the keyboard
in it, closes it when it is on show), `ctrl-k` closes the right panel, `F6` / `shift-F6` walk the focus
sidebar, transcript (a Claude session has one), composer or terminal, right panel, `escape` closes the
accounts overlay. These chords belong to the app even when a terminal has the keyboard (the shell never sees
`ctrl-b`, `ctrl-j`, `ctrl-k`); every other key goes to whatever has the focus. The transcript, when it has
the keyboard (F6 gives it, and so does a click unless a text field such as the composer has it: a click on a
row never takes the keyboard from a field), takes PageUp / PageDown / Home / End; its box turns neon while
it has it. The composer takes the keyboard when a Claude session becomes current, the terminal pane when a
shell one does. `src/frame.rs` holds the pure rules (centre choice, the panel's hover and width state
machine, the focus cycle) and their tests.

## Talking to a daemon

With a daemon the sidebar lists its real sessions and the centre shows the one picked there; a Claude session's
transcript shows the real items of the session (its messages, tool calls and the text as it streams in).

```sh
femme-desktop --show-spki --ssh myvps          # prints this device's key and the line to run on the box:
#   ssh myvps femmed enroll --name <this host> --spki <64 hex>   (prints fingerprint=<hex> quic_port=<n> ...)
femme-desktop --daemon 203.0.113.7:7788 --fingerprint <hex from enroll> --ssh myvps
```

| Flag | Env / `~/.config/femme/desktop.env` | Meaning |
|---|---|---|
| `--daemon ADDR:PORT` | `FEMME_DAEMON` | The daemon's QUIC address (resolved once at start). Needs a fingerprint. |
| `--fingerprint HEX64` | `FEMME_FINGERPRINT` | SHA-256 of the daemon's certificate, what `femmed enroll` prints. |
| `--ssh HOST` | `FEMME_SSH` | ssh alias of the box: the fallback (`ssh -T HOST femmed proxy`, BatchMode, so keys or an agent) and the host the enroll hint names. Alone it runs on ssh only. |
| `--identity DIR` | | This device's certificate and key, default `~/.config/femme/device` (created mode 0700). |
| `--show-spki` | | Print the device's SPKI hash and the exact enroll line, exit 0. Works before there is any daemon. |

Flags win over the environment, which wins over the file (`KEY=value` lines, `#` comments, one pair of
quotes dropped, no `export`). Without `--daemon` and `--ssh` (or their keys) the app shows its frame with
"no daemon configured" and empty panels; nothing is mocked.

On a PC linked to the box (`laptop/pc-setup.sh`, docs/laptop.md) the app is installed and kept at the commit
the box runs by `femme-desktop-update.timer` (`laptop/femme-desktop-update.sh`): a build from `git archive`
of that commit into this crate's `target/`, copied to `~/.local/bin/femme-desktop`, plus a launcher entry.

- **Top bar** shows the link: `quic 23 ms`, `ssh`, `connecting…`, `offline: <reason>`. When QUIC
  refused this device while ssh worked it adds the enroll line to run on the box; a failed request
  (new session, close) is appended to it.
- **Sidebar**: live sessions, newest first, dot = status (working green, blocked/asks yellow, idle or
  starting grey, interrupted/paused/crashed/failed red), title = name, else label, else the last
  directory component, then agent and directory. The list scrolls; a click shows the session's first
  pane (and opens the drawer).
- **Drawer**: one tab per pane of a live session (label: the program's terminal title, else the pane
  title, else `<session> · <no>`), `+` starts a shell session in the directory of the one on show,
  `close` closes the session when the tab is its only pane, else the pane. Only the tab on show is
  drawn; terminals are attached at start for up to 8 panes, else when their tab opens.
- **Terminals**: keys (`femme_proto::term::Key`), paste (`ctrl-shift-v`, `cmd-v`), the wheel (to the program
  when it reports the mouse or is on the alternate screen, else it scrolls the holder's scrollback and
  any new output snaps back), resize, and focus in/out (for programs that ask). A holder that was
  restarted is taken as a new screen; a frame that does not apply re-opens the stream.
- The client reconnects by itself (QUIC, else ssh), terminals pick up where they were.

Tested on a Linux VPS with no GPU: Xvfb + lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`),
a scratch `femmed` (own `FEMME_STATE`, `XDG_RUNTIME_DIR`, `QUIC_PORT`, `FEMME_HOLDER_DIRECT=1`) and input
injected with XTEST. Xvfb has no window manager, so the window never counts as active (no focus in/out
reports) until a client gives it X input focus (`SetInputFocus`).

Environment:

| Variable | Meaning |
|---|---|
| `FEMME_DISPLAY` | Display for the window: `builtin` (default when gpui lists more than one display), `primary`, or an index into gpui's display list. See "Which display". |

**Linux.** Build: `pkg-config` and the X11 keyboard dev package (Debian/Ubuntu:
`libxkbcommon-x11-dev`; Arch: `libxkbcommon-x11`). Run: a Vulkan loader and driver
(`libvulkan1` + `mesa-vulkan-drivers`, or the vendor driver; Arch: `vulkan-icd-loader`),
`fontconfig`, and a Wayland compositor or X11 (`libwayland-client`, `libxcb`, loaded at run time).
The binary only links libc, libxcb and libxkbcommon directly. Backend is wgpu on Vulkan, presenting
with Fifo (vsync) at whatever the compositor's refresh is.

**macOS.** `gpui_platform` is built with `runtime_shaders`, so Metal shaders compile when the app
starts and no full Xcode (the `metal` offline compiler) is needed, only the command line tools.
Not built or run here: the `cmd` key binding and the Metal path are untested.

## Which display

The window never opens on "the main display" by default: with more than one display, `FEMME_DISPLAY`
unset means the built-in one, so a monitor somebody is working on is left alone. `FEMME_DISPLAY`
accepts `builtin`, `primary` (the platform's main display; the first one on Wayland, which has no
such notion) or an index into `cx.displays()`; anything else, an index past the last display included,
is ignored with a message on stderr. The chosen
display is logged to stderr at startup, for example
`femme-desktop: display 1 of 2: 1920x1200 at 60 Hz (FEMME_DISPLAY=builtin). Wayland: ...`.

How "built-in" is decided (`src/display.rs`, pure and unit-tested): the display flagged built-in, else
the smallest non-primary display. Only macOS can flag it (`CGDisplayIsBuiltin` on the display's
CGDirectDisplayID, not built or run here); elsewhere the size heuristic stands in. The window
goes through `WindowOptions { display_id, window_bounds: Windowed(rect centred inside that display),
focus: false when benchmarking }`, and a benchmark does not activate the app either.

What gpui can honour depends on the platform. macOS and X11 screens take `display_id`. On X11
gpui lists X *screens*, and one screen normally spans every monitor, so the window manager decides. On
Wayland gpui only uses the target output for layer-shell surfaces: an ordinary toplevel is placed by the
compositor, and the startup log says so. The app id is `femme-desktop` (`WindowOptions::app_id`), so a
compositor rule can pin it, e.g. Sway `for_window [app_id="femme-desktop"] move to output eDP-1`.

### Benchmarking where nobody is looking (Hyprland)

A bench must not put a window on a screen somebody is working on, so on Hyprland 0.56 (Lua config:
`hyprctl keyword` is gone, `hyprctl eval` runs Lua) it runs on a special workspace that is never shown.
Nothing is written to the user's config; the rules die with the Hyprland instance:

```sh
hyprctl eval "hl.window_rule({match={class='^femme-desktop$'}, workspace='special:femme silent', monitor='eDP-1 silent', no_initial_focus=true, float=true, size={1100,700}, render_unfocused=true})"
hyprctl eval "hl.config({misc={render_unfocused_fps=60}})"      # put back to 15 afterwards
hyprctl dispatch 'hl.dsp.exec_cmd("[workspace special:femme silent] env FEMME_DISPLAY=builtin target/release/femme-desktop --bench 20")'   # a --features bench build
```

Check with `hyprctl clients -j | jq '.[] | select(.class=="femme-desktop") | .workspace'` (must say
`special:femme`) and that `hyprctl activeworkspace` did not change. Traps met on the way: `monitor`
without ` silent` moves keyboard focus to that output; and Hyprland sends **no frame callbacks to a
window on a hidden workspace**, so gpui (which draws only on a compositor frame callback) presents 0
frames and `--bench` warns about it. `render_unfocused` makes Hyprland tick such a window at
`misc:render_unfocused_fps` (default 15, only for windows carrying that rule). Those ticks are a timer,
not a vblank: fps and frame interval of a hidden run measure the timer, frame work (the pass metric)
does not depend on it.

## gpui pin

`gpui-component 0.8.1` depends on the crates.io snapshot **`gpui-pre =0.3.10`**, not on a Zed git
revision, so there is no rev to pin: `Cargo.toml` takes exactly `gpui-pre =0.3.10` (a second gpui in
the graph would make every gpui type mismatch) and `Cargo.lock` sits next to `Cargo.toml` (it is not
git-ignored by the root or the `desktop/` `.gitignore`; commit it with the crate). That snapshot
is Zed's gpui as of `zed@08cbb88a152c3751a43e853425a70e9f631a5918` (zed-version 0.2.2). Its platform
backend is a separate crate, `gpui-pre-platform =0.3.10` (`gpui_platform::application()`), also pinned.
Features: `font-kit`, `wayland`, `x11`, `runtime_shaders`; `profiler` (frame timings for `--bench`) only in
`--features bench` builds, so ordinary builds do not stamp every frame.

## Layout

| File | What |
|---|---|
| `src/main.rs` | CLI, mimalloc, calls `run`. |
| `src/lib.rs` | Builds the 2-worker tokio side runtime (time and IO), starts the daemon's client on it when asked, boots gpui, installs theme, keys, window. |
| `src/connect.rs` | `--daemon`/`--fingerprint`/`--ssh`/`--identity` and their env and `desktop.env` keys, the device identity (dir 0700), `--show-spki`. |
| `src/net.rs` | The daemon's state as `Model` (link, sessions, panes from `Changes{Global}`), `Net` (drains `Client::events`, runs requests), tab labels and the status line. |
| `src/app.rs` | The shell: top bar (menu, link), sessions column and its edge handle, the centre (transcript over composer, a terminal, or a card), the right-hand strip and its hover panel, the accounts layer, key bindings, focus, the benchmark's synthetic panes and driver (`bench` feature). |
| `src/frame.rs` | Pure and unit-tested: what the centre shows for the current session, the right panel's hover/focus/width state machine, the F6 focus cycle. |
| `src/display.rs` | `FEMME_DISPLAY`: picks the display (built-in by default) and the window rectangle on it. |
| `src/theme.rs` | Colour tokens from `board/src/index.html`, embedded fonts, mapping onto gpui-component's `Theme`. |
| `src/ui.rs` | `win`, `tb`, `card`, `dialog`, `button`, `st` pill, `DotGrid` desk. |
| `src/terminal/runs.rs` | Pure: `RowPatch` to background runs and same-style text runs, palette resolve. |
| `src/terminal/element.rs` | `TerminalElement`: batches every row, paints all backgrounds then all text (shaped every frame), keeps the bottom rows and the cursor when the pane is shorter than the grid. |
| `src/terminal/pane.rs` | `TerminalPane` view: owns a `femme_term::client::RowCache`, applies and acks the frames that arrived when it is drawn, re-opens the stream when one does not apply (resets the cache when the holder's epoch changed), takes keys, paste, the wheel (scrollback, `GetRows`) and focus, requests resizes; side-runtime source plumbing. |
| `src/terminal/bridge.rs` | The pump between a pane and the client's `Term` (acks, resync re-opens, refused input counted), so a daemon pane looks like a synthetic one. |
| `src/terminal/input.rs` | Pure: gpui keystroke to `femme_proto::term::Key`, paste chord, wheel steps, pointer to cell. |
| `src/terminal/synth.rs` | Synthetic holders: `Holder` speaks the wire protocol (silent until `Open`, at most 8 unacked frames, merges while blocked) over `Fire` (60 Hz full redraw), `ShellScroll` (one new line per frame) and its `cat` mode (a screenful of new text per frame). |
| `src/transcript/live.rs` | `Transcript` view: joins the session's scope, applies rows and deltas, the `list()` pinned to the end (with a "new" pill when the user has scrolled up), older pages on scrolling to the top, the requests for long items, pictures and answers. |
| `src/transcript/items.rs` | Pure: the items of a session by `seq` (newest rev wins, tombstones), the text of the item in progress merged from deltas by offset (replays, late joins, out-of-order), fetched blobs. |
| `src/transcript/turns.rs` | Pure: which rows the list shows: a result shares its call's row, a finished turn's thinking and tool calls fold behind one line ("worked for 3m · 12 tools"). |
| `src/transcript/{body,question,attach}.rs` | Pure: item bodies (JSON, also the cut ones of the blob tier), questions and their answers, `Attached:` lines. |
| `src/transcript/{markdown,md_view}.rs` | pulldown-cmark folded into a block model (pure), and the block model as elements. |
| `src/transcript/faces.rs`, `faces/` | The bold, italic and bold-italic faces of Schibsted Grotesk (static instances cut from the family's variable files; gpui draws a variable file at its default weight only, so `**bold**` and `*italic*` came out plain), registered once by `Transcript::new`. |
| `src/transcript/{rows,ask,thumb,splice}.rs` | One builder per kind of row; open questions and sending answers; attachment chips, thumbnails and the hover preview (the viewer's `FileView`); the list diff (pure). |
| `src/bench.rs` | Frame-time histograms from gpui's profiler (`bench` feature), probes, the report. |
| `assets/fonts/` | VT323, JetBrains Mono NL (no ligatures), Schibsted Grotesk, Kaushan Script (all OFL). |
| `assets/dots-{1x,2x}.png` | Desk dot-grid tiles (192 logical px, a multiple of the board's 6px period). |

## How a frame stays cheap

- The sidebar, the transcript and the right panel's terminals and files are separate entities mounted
  with `Entity::cached`, so a frame where only terminals changed replays their recorded paint (the
  composer is not cached: it sizes itself, which a cached view cannot). Only the shell (a few dozen
  divs) and the pane views re-render.
- `TerminalElement` batches every visible row into background runs and same-style text runs, then
  paints all background quads first and all text second: two instanced draws, not two per row.
- Text runs are shaped every frame (gpui keeps the line layouts across frames) and painted glyph by glyph
  inside one layer. A 4096-entry shaped-line LRU keyed by (text, style) was tried first and removed: it was no
  faster (see Results).
- The cell size is snapped to whole device pixels so adjacent background quads do not seam.
- Terminal frames are produced on a 2-thread tokio runtime and arrive over a channel bounded at 8 frames per
  pane; a `cx.spawn` task moves them into the pane's inbox and notifies. The pane applies the inbox to its
  `RowCache` when it is drawn and acks the last rev, which is what lets the holder send more: at most 8 are
  unacked, and a holder that is blocked keeps parsing and sends one merged frame. A pane nobody draws (hidden
  window, closed drawer) stops acking, so nothing is applied and the holder idles at its window. A frame that
  does not apply (a rev gap, another holder epoch) makes the pane say `Open` again; the answer is a snapshot.
- The transcript is `gpui::list` with `FollowMode::Tail`: only the rows near the viewport are built each frame, the
  others keep the height measured last. Text is shown as it arrives (the daemon sends it in batches of about
  16 ms, nothing paces it); a delta changes the version of one row, so the list measures that row again and no
  other, and a message's markdown is parsed once per version. A turn's thinking and tool calls fold behind one
  line, so a long session is a few rows per turn.

## What the benchmark measures

gpui stamps every frame inside the platform loop (`profiler` feature, compiled in only by `--features bench`;
an ordinary build refuses `--bench`). "Frame work" is the main-thread time from the later of (compositor frame
signal, first invalidation) until the frame is submitted to the GPU; it excludes the vsync wait inside present,
which is not work. The report also splits `Window::draw` into layout / prepaint / paint, shows present time, and
adds two probes for costs gpui cannot attribute: terminal-grid painting (summed over panes, per frame, inside
paint) and applying frames to the row caches (summed per frame, inside the layout phase: a pane applies what
arrived when it is drawn). Per pane it prints the cell grid it really painted (cols x rows), the frames
applied while measuring and the resyncs (`resyncs ... in total` counts the whole run). RSS is read from `/proc`.
Verdict: p99 frame work <= `--target-ms` if given, else <= 10 ms when the display is >= 100 Hz and <= 16 ms
below that.

The synthetic terminals are phase-staggered (pane i starts i/N of a period late): independent
terminals do not tick in the same instant, and aligned ones would halve the number of frames drawn.

## Results

The runs below were made with the synthetic 50 000-row streaming transcript that has since been replaced by the
session's real one (`src/transcript/`): the terminal numbers stand, the transcript's share of them is not measured again.

The PC from the plan: Omarchy Linux 7.2.8, Hyprland 0.56.2 on Wayland, Intel Alder Lake iGPU plus NVIDIA
RTX 4050 Laptop (Hyprland composites on the NVIDIA GPU; the app renders on it through Vulkan, wgpu, Fifo),
20 threads, 15 GB, rustc 1.97.1. Displays: built-in eDP-1 1920x1200 at 60.0 Hz, external DP-2 2560x1440 at
165 Hz. `release` profile with `--features bench`, `--bench 20 --target-ms 10` (after a 2.5 s warm-up), window
1100x700. Frame work in ms; the bar is p99 <= 10 ms.

**What these runs are, and are not.** Every run below is a window on Hyprland's hidden `special:femme` workspace,
on the 60 Hz built-in panel. Hyprland sends such a window no vsync frame callbacks; it ticks it from its own
`render_unfocused` timer (`misc:render_unfocused_fps`, 60 here, so about 62 draws a second). fps and the frame
interval therefore measure that timer, not a display, and a hidden window leaves out compositing and scan-out.

- The "120 Hz" row of the earlier matrix below is the same thing with the timer set to 120: 123.7 draws a second
  is the timer (1000/8 ms steps) on a hidden window of a 60 Hz panel. It is not 120 Hz presentation.
- The runs on the 165 Hz external monitor (a visible window, before the bench moved out of sight) presented 60-72
  fps, not 165: the compositor's frame callbacks came every 1st to 3rd vblank, varying between runs. Why was not
  established.
- **Real 120 Hz presentation on the external monitor is not demonstrated.** It needs a visible run on DP-2 with
  `--target-ms 10`, which only the user can allow; nobody has run one.
- Frame work, the pass metric, is the app's own main-thread time and does not depend on who ticks the window.

Current build, hidden window, 4 fire panes + 1 `cat` pane (a screenful of new text every frame) + the (since removed)
synthetic 50 000-row streaming transcript, `--target-ms 10` (the grid size and applied-frame counts are in the report so nobody has to guess):

| | |
|---|---|
| fps, late frames | 61.7, 0 |
| frame work p50 / p90 / p99 / p99.9 / max | 1.10 / 1.95 / 2.71 / 3.64 / 3.86 |
| terminals paint p99, terminals apply p99 (summed over the panes) | 0.55, 0.09 |
| each pane | 25x8 cells, 1203 frames applied (60.0/s), 0 resyncs |
| RSS | 266 MiB |
| verdict | p99 frame work 2.71 ms vs 10 ms target (--target-ms): PASS |

Five panes share a 1100x700 window under the columns, so a grid is only 25x8 cells and a frame shapes little text.
`--full-height` gives the panes the whole window (5 panes: 25x29 cells each, transcript left out): p99 frame work 4.40
and 3.83 ms (p50 2.70 / 1.59, max 4.68 / 5.10, RSS 211-217 MiB), two runs of the build without the shaped-line LRU, which
is the final code before formatting. The synthetic holders never skip a rev, so `resyncs` stays 0 in a run; recovery
from a gap and from a restarted holder is covered by the tests `pane_reopens_the_stream_after_a_lost_frame` and
`pane_resets_and_reopens_after_a_holder_restart`.

Shaped-line LRU, before and after: p99 frame work of alternating hidden runs of two builds that differ only by the
4096-entry LRU keyed by (text, style):

| Config | with LRU | without LRU |
|---|---|---|
| 4 fire + 1 cat + 50k transcript, 25x8 per pane (3 runs each) | 3.10, 2.52, 6.70 | 2.44, 2.43, 2.37 |
| 5 panes full height, 25x29 per pane (2 runs each) | 4.62, 4.27 | 4.40, 3.83 |

Removing it never cost anything near the 0.5 ms the removal rule allowed (the LRU build had the one slow run, max
18.7 ms), so it is gone: `lru.rs`, the FNV key and their tests. gpui's own line layout cache already carries repeated
runs from one frame to the next.

### Earlier matrix (before the audit fixes)

Measured with the previous code: a local stand-in for the row cache, no acks, and no grid sizes in the report (the
audit put them at about 31x12 cells per pane and 54 distinct (text, style) pairs per frame, so those runs hardly shaped any
text). Hidden `special:femme` window, Hyprland tick 60 (120 for the last row), four DOOM-fire panes at 60 Hz unless stated:

| Scenario | fps | p50 | p99 | p99.9 | max | interval p99 | RSS |
|---|---|---|---|---|---|---|---|
| 1 pane, no transcript | 60.0 | 0.88 | 1.69 | 2.98 | 3.29 | 17.9 | 215 MiB |
| 4 panes | 62.0 | 0.99 | 2.40 | 2.63 | 2.65 | 17.2 | 220 MiB |
| 4 panes + 50k-row transcript | 62.0 | 1.22 | 2.83 | 4.28 | 5.23 | 17.7 | 278 MiB |
| same, `--mode shell` | 62.0 | 1.13 | 2.48 | 7.77 | 9.31 | 17.3 | 283 MiB |
| same, `--hz 120` sources, Hyprland tick 120 (a timer on a 60 Hz panel, not 120 Hz presentation) | 123.7 | 1.21 | 2.60 | 3.98 | 5.71 | 9.3 | 277 MiB |

Same scenarios with the window visible and presenting at real vsync (earlier runs, before the window was moved out of
sight), p99 frame work: built-in 60 Hz: 2.90 / 2.90 / 3.73 (1 pane / 4 panes / 4 panes + transcript; p99.9 up to 5.3, one
24 ms outlier frame in the 4 + transcript run), shell 2.77, `--hz 120` 3.59 (a second matrix run hotter: p99 3.5-4.2);
external 165 Hz (window 1440x900): 2.35 / 2.55 / 2.84, shell 2.63, `--hz 120` 2.76, with 60-72 fps (see above). RSS
218-289 MiB. The first release build, before the paint fix below, had p99 5.4 ms (1 pane) and 7.3 ms at 88 fps (4 + transcript).

Thermals matter: right after a fat-LTO build, with the package at 80 C, frame work doubled (p99 up to 7.5 ms). Each run
of this round waited for the package to cool to 60 C or less (46-52 C at the start); in the earlier matrix only the first
run did (58 C), later runs started at 68-76 C and are the conservative side.

### What made it fast: gpui's BoundsTree

gpui orders every primitive it paints by inserting its bounds into a `BoundsTree` (about a microsecond per
insert once thousands overlap). Painting each background run and each text run on its own put the tree at
half of a fire frame. Primitives inside one `paint_layer` share the layer's order and skip the tree, so the
terminal element paints all backgrounds in one layer and all glyphs (`paint_glyph` from a
`ShapedLine`) in a second: terminal paint got 8-9x cheaper and p99 went from 7.3 to 2.8 ms.

## Plan versus reality

- **gpui cannot be git-pinned.** The plan says "gpui (git, pinned Zed commit)", but gpui-component 0.8.1
  depends on the crates.io snapshot `gpui-pre =0.3.10`; a second gpui in the graph breaks every type. The pin is that exact
  crate plus `Cargo.lock` (zed@08cbb88a, see "gpui pin"; the lockfile is not git-ignored but nothing in `desktop/` is committed yet).
- **The backend is another crate**: `gpui_platform::application()` from `gpui-pre-platform`, not `Application::new()`.
- **No terminal element exists** in gpui or gpui-component (Zed's is GPL, read for approach only), and the
  obvious path is slow: `ShapedLine::paint` opens a layer per run, so the element paints glyphs itself
  (`paint_glyph` from a shaped line) in the two-layer scheme above.
- **gpui draws only on a compositor frame callback** (Wayland). A window that gets none (hidden workspace,
  occluded, minimised) draws nothing, so a windowed benchmark needs a window the compositor is serving; fps is
  the compositor's cadence, not a property of the app. The snapshot also ships `gpui::bench` and a wgpu
  `WgpuHeadlessRenderer` (feature `bench-support`), a windowless route not tried here: the bench drives the
  real window so the profiler sees the real present path.
- **Display API is thin**: no display names, no "built-in" flag outside macOS, `primary_display()` is `None` on
  Wayland, and `display_id` is honoured only for layer-shell surfaces there, so the display choice is a
  heuristic and the real placement is the compositor's (`src/display.rs`).
- **`Entity::cached` works as advertised** but only helps where the view is not notified; every terminal pane
  repaints each frame, so the saving is the other panels, not the terminals.
- **`femme_term::client::RowCache`** was a stub when this was first written; it has public fields now, the pane
  uses it, and the local stand-in `cache.rs` is gone. The synthetic holders follow its server rules: a frame
  without a title clears the title, an alt-screen frame has `history_lo_abs == top_abs`, a new epoch resets
  the cache.
- The `profiler` feature (frame timings with the compositor's frame signal) is what makes frame work
  measurable at all; the plan does not mention it. It sits behind the `bench` cargo feature.

## Known gaps

- Terminal panes have no selection or copy, send no mouse press or motion (only the wheel), ignore the bell, and
  hide the cursor while scrolled back. `femme-proto` has no `KeyCode` for Menu, PrintScreen, Pause or ScrollLock, so those are
  dropped (a TODO in `terminal/input.rs`), and `ctrl+shift+c` (copy) is not sent.
- With a daemon only the first pane is chosen by a sidebar click and there is no way to open a second pane
  of a session from the UI (`PaneOpen`); the drawer draws one tab at a time.
- The lanes, accounts, composer and right panel are mock even with a daemon.
- The transcript has no text selection or copy, its code blocks are not highlighted and its links do not open. A
  session joined while a message is being written shows that message from where the daemon's replay begins (marked
  with `…`) until the finished row arrives.
- Block and box-drawing glyphs come from the font and can seam by a pixel; double/dotted/dashed underlines draw as single lines;
  combining marks on a base char drift (`force_width` is per glyph).
- The composer input is a toy (append/backspace), the palette is a placeholder.
- macOS (Metal, `cmd` bindings, `CGDisplayIsBuiltin`) is untested; AccessKit and IME are not wired.
- Hidden-window numbers leave out compositing: the visible runs of the earlier matrix were 0.5-1.2 ms higher at p99 (they also
  pay vsync pacing), both far under the bar.

