# Local sessions

A session that runs on the laptop itself, in the same desktop UI, with `local` in its tab.
The laptop is just a second box: a second `femmed` runs there, reachable over a unix socket,
speaking the frames the desktop already speaks. Nothing about sessions, transcripts, terminals,
files or questions changes; only "which daemon" does.

## Pieces and owners

1. **femmed `local` subcommand** (crate `femmed`).
   `femmed local [--state <dir>]` serves one unix socket only: `<state>/femmed.sock`,
   default state `$XDG_STATE_HOME/femme-local` (`~/.local/state/femme-local`). It opens its own
   store there, runs the agent drivers and the socket front, and nothing else: no HTTP/board,
   no QUIC, no ntfy/push, no GitHub scheduler or issue worker, no herdr headless seeding, no
   retention job is fine either way. `femmed local` is what the desktop spawns; it must come up
   in well under a second, log to stderr at `info`, and exit cleanly on SIGTERM. Exposed for the
   desktop: the socket path convention above (the desktop computes it, no shared code).

2. **femme-client unix transport** (crate `femme-client`, maybe `femme-transport`).
   `Config { unix: Option<PathBuf>, .. }`: when set, the client connects with
   `femme_transport::mux_client` over a `tokio::net::UnixStream` (same frames `femmed proxy`
   forwards), reconnects with the same backoff as the other links, and reports
   `Link::Local` in `Event::Connected`. `Config.unix` takes precedence over quic/ssh when set.

3. **Desktop: two boxes** (crate `femme-desktop`).
   - `BoxId { Remote, Local }` (two, not N: ponytail).
   - `Net` keeps one `Model` (sessions, panes, current) but holds a client per box and a
     `sid -> BoxId` map filled from each client's `Changes`/rows; `Net::client_for(sid)` and
     `Net::client_of(BoxId)` replace the 8 `client()` call sites; `client()` stays as
     "the remote box" during the transition and is gone at the end.
   - Startup: the desktop spawns `femmed local` (binary next to its own executable, else
     `femmed` on PATH; skip with a log line if neither) as a child, then connects
     `Config { unix: Some(<state>/femmed.sock) }`. The child is killed when the desktop exits.
     If the socket is already live (a `systemd --user` unit or another window), just connect.
   - New session (palette / menu / sidebar +): a choice of box, remote (default) or
     `this laptop`; `SessionCreate` goes to that box's client; `cwd` for local defaults to
     `$HOME` or the last local cwd.
   - Tab: a session from the Local box shows a small `local` capsule after its name in the
     sidebar row and in the centre header, drawn like the status word. Status line shows
     both links (`quic 23 ms · local`), and `local: off` when the local daemon is down.
   - Everything else (transcript, terminal, files, composer, questions) works unchanged by
     reaching the client through `client_for(sid)`.
   - Panes: pane ids are per daemon; keep the same `BTreeMap<PaneId, Pane>` but key terminal
     opens by `(BoxId, PaneId)` where a client is needed. Session ids are ULIDs: unique across
     boxes, so the merged session map is safe.

## Not in this round
Moving a session between boxes; the phone seeing local sessions (it cannot reach the laptop);
more than two boxes.

## Done when
`femme-desktop` on the laptop shows the remote sessions as today, plus a `+ this laptop`
session that runs `claude` in a laptop folder with `local` in its tab, survives a window
restart while `femmed local` runs, and the remote box keeps working with the local daemon
stopped. Tests: femmed local serves the socket in a tempdir; the client connects over unix and
gets `Link::Local`; `Net` routes a request for a local sid to the local client.
