# femme-holder

One process per session. It owns the session's PTY panes (each with a `femme-term` emulator and
10 000 lines of scrollback), at most one headless agent (piped stdio, not a PTY), and a unix
socket that `femmed` talks to. It exists so the daemon can restart on every deploy without the
agents noticing: the holder runs in its own systemd transient unit and the daemon reconnects.

The crate is a library plus a binary. `femmed` depends on the library with
`default-features = false`, which leaves out the emulator, PTY and CLI dependencies:

| module | what |
| --- | --- |
| `femme_holder::paths` | where the socket, spool and snapshot live |
| `femme_holder::wire` | socket framing (`encode`, async `read`/`write`) |
| `femme_holder::snap` | the on-disk snapshot (`Snapshot`, `SnapPane`) |

## Running it

```
femme-holder --sid <ulid> [--runtime-dir D] [--state-dir D] [--no-sd-notify] [--timing-every N]
```

The line that works on the agentbox (systemd 255, user manager with cgroup delegation; memory,
cpu and pids limits are enforced):

```
systemd-run --user --unit=femme-s-<sid> --slice=femme-sessions.slice --service-type=notify --collect \
  -p KillMode=control-group -p OOMPolicy=continue -p CPUWeight=100 -p MemoryHigh=5G -p MemoryMax=7G \
  -p TasksMax=4096 femme-holder --sid <sid> --runtime-dir <D> --state-dir <D>
```

`systemd-run` returns once the holder has sent `READY=1`, i.e. once the socket accepts
connections. `-p WatchdogSec=N` also works: the holder pings at `N/2`. Pass `--runtime-dir` and
`--state-dir` explicitly: a unit started by `systemd-run --user` gets the user manager's
environment, which may lack `XDG_STATE_HOME`. `--no-sd-notify` is for running outside a service.
`--timing-every N` (default 100) is how often a frame carries a `Timing` record (0 = never).

| path | default | |
| --- | --- | --- |
| socket | `<runtime>/s/<sid>.sock` | `runtime` = `--runtime-dir`, else `$XDG_RUNTIME_DIR/femme`. Mode 0600, dirs 0700. A stale socket is replaced; a live one makes the new holder exit 1. At most 107 bytes of path. |
| spool | `<state>/spool/<sid>.jsonl` | the agent's stdout, append-only, mode 0600 |
| snapshot | `<state>/snap/<sid>.snap` | see below |

`state` = `--state-dir`, else `$XDG_STATE_HOME/femme`, else `~/.local/state/femme`. The holder's
`epoch` (in `Info`) is the unix time in ms at which it started; it changes whenever the process
restarts, so a daemon that sees a new epoch knows its PTYs are gone.

## Wire format

Every message in either direction is a `u32` little-endian length followed by a postcard value, no
compression, at most 16 MiB (`wire::encode` / `wire::read`). Requests are
`femme_proto::holder::HolderReq`, replies `HolderResp`. A connection must come from our own uid
(`SO_PEERCRED`), else it is closed. **The first message decides the connection's role**, and
nothing else is accepted as a first message (reply `Err`, then close). A connection that sends
nothing for 10 s is dropped.

### Control: `Hello { v }`

`v` must be `HOLDER_VERSION` or `HOLDER_VERSION - 1`; otherwise `Err` and close. The reply is
`Info(HolderInfo)`: panes (including exited ones, until `ClosePane`) and the agent. Then
requests and replies alternate strictly in order (there are no request ids), and `Event`s are
pushed whenever they happen, so a reply may be preceded by events. Any number of control
connections may coexist; every one gets every event.

| request | reply |
| --- | --- |
| `Hello` | `Info` again |
| `SpawnPty { no, cmd, cwd, env, cols, rows }` | `Ok`, or `Err` (empty command, pane `no` exists, bad cwd, spawn failed). Empty `cwd` = the holder's own. |
| `ClosePane { no }` | `Ok` at once; the child gets SIGHUP (to its process group), SIGKILL after 2 s. The pane leaves `Info` immediately; `ChildExited` and the views' `Exited` follow. `Err` if unknown. |
| `SpawnAgent { cmd, cwd, env }` | `Ok`, or `Err("agent already running")` while one is alive. After it exits another may be spawned; the spool keeps growing. `alive` goes false the moment the child is reaped, but `ChildExited` follows only once its pipes have drained (at most 1 s later); a `SpawnAgent` in between gets `Err("the previous agent is still shutting down")`. |
| `AgentWrite(bytes)` | `Ok` = queued (never blocks), `Err` if no agent / it exited / 8 MiB (or 256 writes) are already waiting for it. A write that finds nothing waiting is taken whatever its size, so one message over 8 MiB gets through, and nothing after it until the agent has read it. |
| `AgentCloseStdin` | `Ok`; the EOF is queued behind earlier writes |
| `SpoolCommit { offset }` | `Ok`. Remembered only; the spool is never truncated (`spool_lo` stays 0). |
| `Signal { target, sig }` | `Ok`. Signals the child itself, not its process group. |
| `Screen { pane, fmt }` | `Screen(text)` (plain or ANSI), or `Err` (unknown pane; or the text does not fit in one 16 MiB message). A reply that cannot be encoded always becomes an `Err`: the connection is never closed for it. |
| `Shutdown` | `Ok`, then the holder stops (see lifecycle) |
| `Attach`, `Term`, `SpoolRead` | `Err`: these start a connection of their own |

Events (`HolderEvent`):

- `ChildExited { target, status }`: see "status" below. For the agent it is preceded by
  `AgentStderr(tail)`, the last 64 KiB of its stderr as text.
- `Title { pane, title }`: only when the title changes (at most 2 KiB), at most one per 50 ms
  per pane, and the latest title always goes out (before `ChildExited` if the child exits first).
  A program that sets its title in a loop costs a few events, not one per change.
- `Bell { pane }`: at most one per 100 ms.
- `Notify { pane, title, body }`: OSC 9 / 777, at most 10 per second per pane; the rest are
  dropped.

Events are not replayed. A control connection that arrives late learns the state from `Info`
(pane exit and title, agent exit) but not the agent's stderr tail. Events are lossy but never
cost a connection: once a control connection has fewer than 32 free slots of its 256-message
queue, `Bell`, `Title` and `Notify` are dropped for it (titles are in `Info` anyway), and
`ChildExited` / `AgentStderr` are dropped only when the queue is completely full. Replies are not
events and are never dropped, so a request is always answered (a peer that stops reading stalls
only itself).

### Pane view: `Attach { pane, epoch, have_rev, client_instance }`

No reply except what follows; an unknown pane gets `Err` and a close. Everything is
`HolderResp::Term(TermDown::..)` down and `HolderReq::Term(TermUp::..)` up. `Open` is how a client
resynchronizes (see "Resync").

- **First frame.** Sent at once. A full snapshot (`base_rev == 0`) unless `epoch` is the pane's
  current epoch and `have_rev` is a rev the pane has reached, in which case it is a delta from
  `have_rev`, or nothing if the screen did not change. A pane's `Frame::epoch` is
  `holder epoch + n`; it changes when the emulator is reset after a panic, and every view then
  gets a snapshot under the new epoch. A client that already holds rows cannot apply it
  (`RowCache::apply` reports `EpochChanged`): it drops its cache and sends `Open` with
  `have_rev: 0`, which is answered with a full snapshot (see "Resync").
- **Resync.** `TermUp::Open { pane, epoch, have_rev, .. }` on an open view restarts the view as an
  `Attach` with the same numbers would: `have_rev` 0, or an epoch that is not the pane's, gets a
  full snapshot; the pane's epoch and a rev the pane has reached gets a delta from `have_rev` (or
  nothing, if the screen has not changed). The answer goes out at once, and frames still in
  flight are forgotten: they no longer count against the flow-control window, so a client that
  stopped acking is not left waiting. A client sends it on `NeedResync { have_rev }` (a frame did
  not follow what it holds) or `EpochChanged`. An `Open` naming another pane is ignored.
- **Scheduling.** If any view sent input in the last 50 ms, frames go out right after each PTY
  read. Otherwise output is coalesced: a frame goes out once the PTY has been quiet for 8 ms, and
  at the latest 16 ms after the first unsent output (about 60 Hz under a flood, never above 120).
- **Flow control.** At most 8 unacked frames or 128 KiB per view. A blocked view gets nothing; the
  emulator keeps parsing; the `Ack { rev }` that unblocks it triggers one merged frame.
  `Exited` is sent after the last frame, so a blocked view gets it only after it acks.
- **Input.** `Key`, `Paste`, `Mouse` are encoded by the emulator with its live modes; `Bytes`
  is written as is. `seq` is per `client_instance`: a seq at or below the highest already
  accepted is dropped (a reconnect replaying its queue). The holder remembers the last seq of a
  `client_instance` for 10 minutes, so the first frame after a reconnect already carries it in
  `applied_seq`. `seq` 0 means "untracked" (the compat layer's `/input`): always written, never
  deduped. `applied_seq` = highest seq written to the PTY; `echo_seq` = the `applied_seq` at the
  time of the latest PTY read, i.e. the first read after a write confirms it. Output that was
  already in flight can confirm a seq slightly early.
- **Resize.** The most recent resizer owns the size: a `Resize` from another view within 2 s of the
  owner's is ignored (the phone never resizes). Sizes are clamped to 2..=1000 columns x 1..=500
  rows (the emulator's minimum is 2 columns). The PTY gets the new size (and the child SIGWINCH)
  and every view a snapshot (a resize starts a new layout). Reflowing the scrollback is not free
  and a window drag sends a resize per frame, so a pane applies the first resize at once and then
  at most one per 100 ms, the latest size winning.
- **Synchronized updates.** A program inside DEC 2026 (`CSI ? 2026 h`) holds frames back until it
  ends the update; the pane arms a timer for the emulator's deadline and flushes when it fires, so
  a program that never ends the update still shows up.
- **Focus.** `Focus(true/false)` sends `ESC [ I` / `ESC [ O` only while the program has enabled
  focus reporting (mode 1004).
- **GetRows.** `Rows(..)` for `[from_abs, to_abs)`, at most 2000 rows per request.
- **Bell and exit.** `Bell`, `Exited { status }` as events happen.
- **Too big.** A message for a view that does not fit in 16 MiB (a frame, or `Rows`) cannot be
  sent, and only a cell holding millions of combining characters gets there (the emulator does not
  cap them). The view gets `Err("screen too large to send: ..")` and is closed, since its next
  delta could no longer follow what it holds; a new `Attach` hears the same until the row is gone.
  The pane and the other views are untouched.
- **Timing.** About 1 % of frames (every `--timing-every`th) carry a `Timing`; the first frame in
  response to an input whose `seq % N == 0` carries the full hop breakdown. Nanoseconds are
  monotonic since the holder started.
- **Isolation.** A view that disconnects, or whose 64-message queue overflows (it stopped
  reading), is dropped; the pane and the other views are unaffected. Input a view sent before it
  hung up is still delivered.

### Spool: `SpoolRead { from }`

`Spool { offset, data }` chunks of at most 64 KiB, contiguous from `from` to the end of the spool
file, then more as the agent writes, until the connection closes. `from` past the end is an `Err`
and a close. Each read of the agent's stdout is written and flushed immediately, so partial
lines are visible; `Info.agent.spool_hi` is the end of the spool. A write that fails (disk full)
stalls the agent and is retried every second; whatever the failed attempt left behind is cut off
first, so the spool never holds the same bytes twice, and `spool_hi` is always the file's length.
Log lines that cannot be written (stderr is a file on the same full disk) are dropped: they never
panic the task that logged them.

## Lifecycle

1. Start: create dirs, claim the socket, listen, `READY=1` (unless `--no-sd-notify`), then
   `WATCHDOG=1` at half of `WATCHDOG_USEC` if systemd set it.
2. The holder is event driven. The only recurring timer is the 30 s tick, which writes a snapshot
   if anything changed and checks the idle exit. Pane timers (frame coalescing, the synchronized
   update deadline, the resize throttle, the 2 s SIGHUP grace) exist only while there is
   something to wait for. Nothing polls, except the shutdown wait for children (20 ms, at most
   2 s).
3. **Idle exit:** with no live child and no connection of any role for 10 minutes, the holder
   exits 0 (checked on the tick, so up to 30 s late). The daemon normally sends `Shutdown` first.
4. **Shutdown** (`Shutdown`, SIGTERM or SIGINT): SIGTERM to every child's process group, wait up
   to 2 s, SIGKILL the rest, write a final snapshot, unlink the socket, exit 0. Exit 1 only if
   startup failed.
5. If femmed disappears the holder carries on: nothing needs a control connection.

Children get a short environment: `PATH HOME USER LOGNAME SHELL LANG LANGUAGE TZ TMPDIR
XDG_{RUNTIME,CONFIG,DATA,STATE,CACHE}_DIR DBUS_SESSION_BUS_ADDRESS LC_*` from the holder's own,
then `TERM=xterm-256color`, `COLORTERM=truecolor`, `LANG=C.UTF-8` if no locale is set, then
the request's `env`, then `FEMME_SID` and (panes) `FEMME_PANE`, which the request cannot
override. Anything else (API keys, `NOTIFY_SOCKET`) must come in the request's `env`. Panes and
the agent each lead their own process group.

### status

`ChildExited.status`, `HolderPane.exit` and `HolderAgent.exit` are the exit code, or minus the
signal number when the process was killed by a signal (`-9` for SIGKILL, `-15` SIGTERM, `-1`
SIGHUP). `-1` is also what is reported if the status was lost.

## Snapshot

`<state>/snap/<sid>.snap`, written atomically (temp file + rename, mode 0600) every 30 s if
something changed and on shutdown. Postcard, no header, no compression; read it with
`Snapshot::read`. Postcard structs cannot grow fields compatibly: treat a file that does not
decode as "no snapshot".

```rust
pub struct Snapshot { pub sid: Sid, pub written: i64 /* unix ms */, pub panes: Vec<SnapPane>, pub agent_alive: bool }
pub struct SnapPane {
    pub no: u16,
    pub title: String,
    pub screen_text: String,            // the visible screen, as Screen { Text } returns it
    pub last_200_rows: Vec<RowPatch>,   // up to 200 rows ending at the bottom of the screen, oldest first
}
```

## Panic isolation

Every call into the emulator runs under `catch_unwind` (parsing, the synchronized-update timer,
attaching a view, snapshots, frames, rows, the input encoders). On a panic the pane gets a fresh
emulator of the same size and a new epoch (views receive a snapshot), and the child is sent
SIGWINCH so it redraws; the pane keeps running and still announces its exit, and the holder and
the other panes are untouched. Watch for the log line `terminal emulator panicked` and the next
frame's new `epoch`.

Debug builds have a test hook: `FEMME_HOLDER_PANIC_AT=feed|sync|attach|snap` in the holder's
environment makes its first emulator call at that site panic (once per process). The tests use it;
release builds do not contain it.

## Tests

`cargo test -p femme-holder` spawns the real binary with `--no-sd-notify` and temporary
directories. The panic-isolation tests need the debug-build hook above, so `--release` skips them. `cargo test -p femme-holder -- --ignored` additionally runs the `systemd-run` test
(needs a systemd user manager; it starts and removes a unit named `femme-s-test-*`).

ponytail: the spool is never truncated, so rotate it when a session's spool passes 1 GiB; the
idle exit is checked on the 30 s tick; input to a child that does not read its terminal is queued
up to 8 MiB and then dropped; one thread serves every pane of the session, so shard panes over
threads if a flood on one ever shows up in another's latency; the shutdown wait polls every 20 ms.
