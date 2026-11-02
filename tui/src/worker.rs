//! Background threads, all talking to the main loop over one channel: the keyboard, a worker per
//! machine (snapshot + event subscription), the screen poller for the selected pane, and the
//! socket-directory scan that finds machines.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use ratatui::text::Text;
use serde_json::json;

use crate::herdr::client::Client;
use crate::herdr::events;
use crate::herdr::machines::{discover, Machine};
use crate::herdr::types::{parse_snapshot, Snapshot, Status};

/// A pane on a machine. Shown as `laptop~w1:p1` where an id is printed, like the board.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub machine: String,
    pub pane: String,
}

/// What the screen poller reads: the selected pane, its socket, and its status (the poll rate).
#[derive(Clone, Debug)]
pub struct ScreenTarget {
    pub target: Target,
    pub sock: PathBuf,
    pub status: Status,
}

pub type Shared = Arc<Mutex<Option<ScreenTarget>>>;

pub fn shared_target() -> Shared {
    Arc::new(Mutex::new(None))
}

pub enum Msg {
    Input(crossterm::event::Event),
    Machines(Vec<Machine>),
    Snapshot(String, Snapshot),
    /// A machine's herdr could not be reached: name, reason.
    Down(String, String),
    Screen(Target, Text<'static>),
    ScreenErr(Target, String),
    Toast(String),
    /// The user pressed r: every worker takes a snapshot now.
    Refresh,
}

/// A machine worker's controls: `stop` ends it, `poke` asks for a snapshot now.
pub struct Handle {
    stop: Arc<AtomicBool>,
    poke: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn poke(&self) {
        self.poke.store(true, Ordering::Relaxed);
    }
}

pub fn spawn_input(tx: Sender<Msg>) {
    thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
}

/// Rescans ~/.config/herdr/remote every few seconds: a laptop that links or unlinks shows up.
pub fn spawn_discovery(home: PathBuf, local: PathBuf, local_name: String, tx: Sender<Msg>) {
    thread::spawn(move || loop {
        if tx.send(Msg::Machines(discover(&home, &local, &local_name))).is_err() {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    });
}

pub fn spawn_machine(m: Machine, tx: Sender<Msg>) -> Handle {
    let stop = Arc::new(AtomicBool::new(false));
    let poke = Arc::new(AtomicBool::new(false));
    let (s, p) = (stop.clone(), poke.clone());
    thread::spawn(move || machine_loop(m, tx, s, p));
    Handle { stop, poke }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Snapshot, subscribe, re-snapshot on events (debounced) and every 5 s anyway; on any failure
/// report the machine down and retry with backoff. The subscription names each agent pane, so it
/// is renewed when the set of agent panes changes.
fn machine_loop(m: Machine, tx: Sender<Msg>, stop: Arc<AtomicBool>, poke: Arc<AtomicBool>) {
    let mut client = Client::new(m.sock.clone());
    let mut backoff = 1u64;
    let snapshot = |client: &mut Client| -> Result<Snapshot, String> {
        let r = client.call("session.snapshot", json!({})).map_err(|e| e.to_string())?;
        parse_snapshot(&r)
    };
    while !stop.load(Ordering::Relaxed) {
        let snap = match snapshot(&mut client) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(Msg::Down(m.name.clone(), e));
                sleep_unless(&stop, Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        backoff = 1;
        let mut panes = agent_panes(&snap);
        if tx.send(Msg::Snapshot(m.name.clone(), snap)).is_err() {
            return;
        }

        let dirty = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let sub_stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        {
            let (sock, params, dirty, dead, sub_stop) = (m.sock.clone(), events::list_subscriptions(&panes), dirty.clone(), dead.clone(), sub_stop.clone());
            thread::spawn(move || {
                let _ = events::subscribe(&sock, params, &sub_stop, |_| dirty.store(true, Ordering::Relaxed));
                dead.store(true, Ordering::Relaxed);
            });
        }

        let mut last = Instant::now();
        let mut dirty_since: Option<Instant> = None;
        loop {
            if stop.load(Ordering::Relaxed) {
                sub_stop.store(true, Ordering::Relaxed);
                return;
            }
            if dead.load(Ordering::Relaxed) {
                // A subscription that dies at once would spin: pause before trying again.
                if started.elapsed() < Duration::from_secs(1) {
                    sleep_unless(&stop, Duration::from_secs(2));
                }
                break;
            }
            if dirty.swap(false, Ordering::Relaxed) {
                dirty_since.get_or_insert_with(Instant::now);
            }
            let due = poke.swap(false, Ordering::Relaxed)
                || dirty_since.is_some_and(|t| t.elapsed() >= Duration::from_millis(150))
                || last.elapsed() >= Duration::from_secs(5);
            if due {
                dirty_since = None;
                last = Instant::now();
                match snapshot(&mut client) {
                    Ok(s) => {
                        let now = agent_panes(&s);
                        if tx.send(Msg::Snapshot(m.name.clone(), s)).is_err() {
                            sub_stop.store(true, Ordering::Relaxed);
                            return;
                        }
                        if now != panes {
                            panes = now;
                            break; // renew the subscription for the new pane set
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Down(m.name.clone(), e));
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        sub_stop.store(true, Ordering::Relaxed);
    }
}

fn agent_panes(s: &Snapshot) -> Vec<String> {
    s.panes.iter().filter(|p| p.agent.is_some()).map(|p| p.pane_id.clone()).collect()
}

/// Reads the selected pane's visible screen with its colours. herdr has no output stream, so this
/// polls: 400 ms while the agent is busy or asking, 1 s otherwise (the board polls at 250 ms).
pub fn spawn_screen(sel: Shared, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut clients: HashMap<PathBuf, Client> = HashMap::new();
        let mut last: Option<(Target, String)> = None;
        loop {
            let Some(t) = sel.lock().unwrap().clone() else {
                thread::sleep(Duration::from_millis(200));
                continue;
            };
            let client = clients.entry(t.sock.clone()).or_insert_with(|| Client::new(t.sock.clone()));
            let params = json!({"pane_id": t.target.pane, "source": "visible", "format": "ansi", "strip_ansi": false});
            match client.call("pane.read", params) {
                Ok(r) => {
                    let text = r["read"]["text"].as_str().unwrap_or("").to_string();
                    let same = last.as_ref().is_some_and(|(lt, ls)| *lt == t.target && *ls == text);
                    if !same {
                        let rendered = text.as_str().into_text().unwrap_or_else(|_| Text::raw(text.clone()));
                        if tx.send(Msg::Screen(t.target.clone(), rendered)).is_err() {
                            return;
                        }
                        last = Some((t.target.clone(), text));
                    }
                }
                Err(e) => {
                    last = None;
                    if tx.send(Msg::ScreenErr(t.target.clone(), e.to_string())).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
            let wait = if matches!(t.status, Status::Working | Status::Blocked) { 400 } else { 1000 };
            thread::sleep(Duration::from_millis(wait));
        }
    });
}
