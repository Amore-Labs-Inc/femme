//! femme-tui: every agent on every linked machine, the selected one's live screen, and a prompt
//! box. T3 Code's shape (a list, one agent, a composer) in a terminal, over herdr's socket API.
//!
//! Runs two ways from the same binary: as a herdr plugin pane (plugin/tui.sh opens it as an
//! overlay; herdr then sets HERDR_PANE_ID), or on its own over ssh/mosh, `ssh agentbox femme-tui`,
//! with no herdr client in between. Machines are the local herdr socket plus every socket a linked
//! laptop (or the Instinct session) lends in ~/.config/herdr/remote, as the board lists them.
mod actions;
mod demo;
mod herdr;
mod keys;
mod state;
mod theme;
mod ui;
mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use worker::{Handle, Msg};

const USAGE: &str = "\
femme-tui: your agents, one screen, a prompt box (FEMME)

  femme-tui                 talk to herdr at ~/.config/herdr/herdr.sock and every linked machine
  femme-tui --session NAME  a named herdr session instead (~/.config/herdr/sessions/NAME)
  femme-tui --socket PATH   an explicit socket (HERDR_SOCKET_PATH does the same)
  femme-tui --demo          canned agents, no herdr: a look at the screen
  femme-tui --truecolor     force 24-bit colours (--no-truecolor: the 256-colour palette)
  femme-tui --version

Keys: j/k move, Enter open, i write, n new agent, x close, / filter, ? help, q quit.";

struct Opts {
    socket: Option<PathBuf>,
    session: Option<String>,
    demo: bool,
    truecolor: Option<bool>,
}

fn parse_args() -> Opts {
    let mut o = Opts { socket: None, session: None, demo: false, truecolor: None };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => o.socket = args.next().map(PathBuf::from),
            "--session" => o.session = args.next(),
            "--demo" => o.demo = true,
            "--truecolor" => o.truecolor = Some(true),
            "--no-truecolor" => o.truecolor = Some(false),
            "--version" | "-V" => {
                println!("femme-tui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0)
            }
            _ => {
                eprintln!("femme-tui: unknown option {a}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    o
}

/// The machine's short name, as the sidebar heads the local group.
fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().split('.').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

fn main() {
    let opts = parse_args();
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let local = herdr::machines::local_socket(&home, opts.socket.as_deref(), opts.session.as_deref());
    let theme = theme::Theme::detect(opts.truecolor);
    let own_pane = std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty());
    let mut app = state::App::new(theme, hostname(), own_pane, home.clone());
    app.demo = opts.demo;

    let (tx, rx) = mpsc::channel();
    let selected = worker::shared_target();
    if opts.demo {
        demo::spawn(tx.clone(), selected.clone());
    } else {
        worker::spawn_discovery(home, local, app.hostname.clone(), tx.clone());
        worker::spawn_screen(selected.clone(), tx.clone());
    }
    worker::spawn_input(tx.clone());

    // The panic hook restores the terminal first, or a panic would leave it raw and blank.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let result = run(&mut terminal, &mut app, rx, tx, selected, opts.demo);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    if let Err(e) = result {
        eprintln!("femme-tui: {e}");
        std::process::exit(1);
    }
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut state::App,
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    selected: worker::Shared,
    demo: bool,
) -> std::io::Result<()> {
    let mut workers: HashMap<String, Handle> = HashMap::new();
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        // Wait for one message (or half a second, for clocks and toasts), then take what else is queued.
        let mut batch = Vec::new();
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => batch.push(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        batch.extend(rx.try_iter());
        for msg in batch {
            match msg {
                Msg::Machines(list) if !demo => {
                    for m in &list {
                        if !workers.contains_key(&m.name) {
                            workers.insert(m.name.clone(), worker::spawn_machine(m.clone(), tx.clone()));
                        }
                    }
                    workers.retain(|name, h| {
                        let keep = list.iter().any(|m| &m.name == name);
                        if !keep {
                            h.stop();
                        }
                        keep
                    });
                    app.apply(Msg::Machines(list));
                }
                Msg::Input(ev) => {
                    if let Some(action) = keys::map(&ev, &app.mode) {
                        if !app.handle(action, &tx) {
                            return Ok(());
                        }
                    }
                }
                Msg::Refresh => {
                    for h in workers.values() {
                        h.poke();
                    }
                    app.toast("refreshing…");
                }
                other => app.apply(other),
            }
        }
        app.tick();
        *selected.lock().unwrap() = app.screen_target();
    }
}
