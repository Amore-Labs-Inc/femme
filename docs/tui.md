# TUI: every agent, one screen, a prompt box

`femme-tui` is FEMME's own view of herdr: the list of agents on the left, the selected agent's live screen in the middle, a prompt box underneath. The shape of [T3 Code](https://github.com/pingdotgg/t3code) in a terminal, without the detail: no chat transcript, no diff panel. herdr stays the engine; this is a face on its socket API.

```
┌ agentbox ────────────────┐┌ #14 add csv export · claude · working 12m · agentbox ──────────┐
│ ● claude #14 add csv  12m││ (the pane's screen, colours and all)                           │
│   ~/femme-work/shop      ││                                                                │
│ ● codex  #15 fix login 3m││                                                                │
│ laptop ──────────────────││                                                                │
│ ● claude refactor     41m││                                                                │
└──────────────────────────┘└────────────────────────────────────────────────────────────────┘
┌ to #14 · Enter sends · Shift+Enter newline · Esc ──────────────────────────────────────────┐
│ ▌                                                                                          │
└────────────────────────────────────────────────────────────────────────────────────────────┘
 NORMAL  3 working · 1 need you   j/k move  Enter open  n new  x close  / filter  ? help  q
```

## Two ways to open it

- **On its own:** `ssh <HOST> femme-tui` from the laptop, or as the command of a mosh profile on the phone ([notifications.md](notifications.md)). No herdr client in between: it talks to herdr's socket directly, so it works where a full multiplexer is awkward, on a phone above all. Under 80 columns it shows the list or the agent, and Tab switches.
- **Inside herdr:** `prefix+f` (the prefix is `ctrl+b` unless `keys.prefix` says otherwise) opens it as an overlay over the current layout; `q` closes it. Enter on an agent focuses its real pane. The key belongs to the machine whose pane is selected (see [laptop.md](laptop.md)), so on a server pane it runs the server's copy, which is the useful one: that is where the linked machines' sockets are. On the laptop's own panes it needs a local build (`laptop-setup.sh` makes one when cargo is there).

## What it shows

Machines first: this box, then every socket in `~/.config/herdr/remote` (a laptop running `femme-link`, the Instinct session), the same list the board's Sessions tab uses. Under each, one row per agent pane: a dot (green working, yellow blocked or done, grey idle), the agent, the workspace label, the folder, and how long it has been in that state. `s` shows plain shells too. The screen in the middle is the pane's visible area with its colours, read twice a second while the agent is busy. The status bar is pink: `N working · M need you`, and the keys.

Times count from when this screen first saw the state, since herdr keeps no start time: at launch everything reads `0s`. The screen is the pane's own width; a narrower window clips it on the right.

## Keys

| Key | Does |
|---|---|
| `j`/`k`, `↑`/`↓` | move between agents |
| `1`-`9` | jump to the nth agent |
| `Enter` | inside herdr: open the agent's pane; on its own: write to it (narrow: open the agent) |
| `i`, `Tab` | write a message; `Enter` sends it (herdr pastes and submits), `Shift+Enter` a newline, `Esc` back |
| `a` | open the agent's pane in herdr (inside herdr only) |
| `n` | new agent: machine, folder, claude / codex / pi / shell, label |
| `x` | close the selected workspace (asks first) |
| `/` | filter by label, agent, folder, machine |
| `s` | show shells without an agent |
| `b` | hide the list |
| `r` | refresh now |
| `?` | this list |
| `q` | quit |

A message to a blocked agent is refused: it is asking you something, answer it in its terminal first (`Enter` inside herdr). Lines sent to a plain shell go in as typed, with Enter.

## Under the hood

`tui/` is a Rust crate (ratatui), built on the server by `bootstrap.sh` next to the board and put in `~/.local/bin/femme-tui` (and `/usr/local/bin`, for non-interactive ssh). It keeps one connection per machine for `events.subscribe` and refreshes with `session.snapshot` on each event (and every 5 s anyway), reads the screen with `pane.read`, and sends with `agent.prompt`. New agents follow the board's recipe: `workspace.create`, wait for the shell, `agent.start`. A machine whose socket stops answering is greyed and retried with backoff; one whose socket file disappears is dropped after a minute. `femme-tui --demo` runs with canned agents and no herdr.
