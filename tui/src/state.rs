//! Everything the screen shows and every key's effect: machines and their snapshots, the flat row
//! list the sidebar draws, the selection, the composer, dialogs and toasts.
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::text::Text;

use crate::actions;
use crate::herdr::types::{Snapshot, Status};
use crate::keys::Action;
use crate::theme::Theme;
use crate::worker::{Msg, ScreenTarget, Target};

pub const KINDS: [&str; 4] = ["claude", "codex", "pi", "shell"];
pub const NARROW: u16 = 80;
const TOAST_FOR: Duration = Duration::from_secs(4);
const GONE_FOR: Duration = Duration::from_secs(60);

pub struct MachineState {
    pub sock: PathBuf,
    pub local: bool,
    pub up: bool,
    pub note: String,
    pub snapshot: Snapshot,
    /// When each pane entered its current status, as this TUI saw it (herdr keeps no start time).
    pub since: HashMap<String, (Status, Instant)>,
    /// Its socket file disappeared at this time; the group stays greyed a minute, then goes.
    pub gone: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub target: Target,
    pub workspace_id: String,
    pub label: String,
    pub kind: String,
    pub status: Status,
    pub dir: String,
    pub since: Instant,
    pub shell: bool,
}

pub enum Row {
    Header { machine: String, note: String },
    Agent(Entry),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Filter,
    Help,
    New,
    Close,
}

/// The prompt box: a string and a cursor (in chars).
#[derive(Default)]
pub struct Composer {
    pub text: String,
    pub cursor: usize,
}

impl Composer {
    fn byte(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
    }
    pub fn insert(&mut self, c: char) {
        let at = self.byte(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }
    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
            self.text.replace_range(a..b, "");
            self.cursor -= 1;
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.len() {
            let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
            self.text.replace_range(a..b, "");
        }
    }
    pub fn len(&self) -> usize {
        self.text.chars().count()
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

/// The new-agent dialog, one step at a time: machine, folder, kind, label.
#[derive(Default)]
pub struct NewAgent {
    pub step: usize,
    pub machine: usize,
    /// (shown, cwd to send; None = herdr's default folder). One more implicit item: type a folder.
    pub folders: Vec<(String, Option<String>)>,
    pub folder: usize,
    pub custom: String,
    pub kind: usize,
    pub label: String,
}

pub struct App {
    pub theme: Theme,
    pub hostname: String,
    pub home: PathBuf,
    /// Set by herdr when this runs as a plugin pane: attach works, and this pane is hidden.
    pub own_pane: Option<String>,
    pub demo: bool,
    pub machines: BTreeMap<String, MachineState>,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub selected_id: Option<Target>,
    pub filter: String,
    pub show_shells: bool,
    pub sidebar: bool,
    pub narrow_agent: bool,
    pub width: u16,
    pub mode: Mode,
    pub composer: Composer,
    pub new_agent: NewAgent,
    pub screen: Option<(Target, Text<'static>)>,
    pub screen_err: Option<String>,
    pub toasts: Vec<(String, Instant)>,
}

impl App {
    pub fn new(theme: Theme, hostname: String, own_pane: Option<String>, home: PathBuf) -> App {
        App {
            theme,
            hostname,
            home,
            own_pane,
            demo: false,
            machines: BTreeMap::new(),
            rows: Vec::new(),
            selected: 0,
            selected_id: None,
            filter: String::new(),
            show_shells: false,
            sidebar: true,
            narrow_agent: false,
            width: 120,
            mode: Mode::Normal,
            composer: Composer::default(),
            new_agent: NewAgent::default(),
            screen: None,
            screen_err: None,
            toasts: Vec::new(),
        }
    }

    pub fn narrow(&self) -> bool {
        self.width < NARROW
    }

    /// Machines in sidebar order: this box first, then the linked ones by name.
    pub fn order(&self) -> Vec<String> {
        let mut v: Vec<(bool, &String)> = self.machines.iter().map(|(n, m)| (!m.local, n)).collect();
        v.sort();
        v.into_iter().map(|(_, n)| n.clone()).collect()
    }

    pub fn toast(&mut self, s: impl Into<String>) {
        self.toasts.push((s.into(), Instant::now()));
    }

    pub fn current(&self) -> Option<&Entry> {
        match self.rows.get(self.selected) {
            Some(Row::Agent(e)) => Some(e),
            _ => None,
        }
    }

    pub fn screen_target(&self) -> Option<ScreenTarget> {
        let e = self.current()?;
        let m = self.machines.get(&e.target.machine)?;
        Some(ScreenTarget { target: e.target.clone(), sock: m.sock.clone(), status: e.status })
    }

    pub fn counts(&self) -> (usize, usize) {
        let mut working = 0;
        let mut needs = 0;
        for r in &self.rows {
            if let Row::Agent(e) = r {
                if e.shell {
                    continue;
                }
                if e.status == Status::Working {
                    working += 1;
                }
                if e.status.needs_you() {
                    needs += 1;
                }
            }
        }
        (working, needs)
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Machines(list) => {
                for m in &list {
                    let st = self.machines.entry(m.name.clone()).or_insert_with(|| MachineState {
                        sock: m.sock.clone(),
                        local: m.local,
                        up: false,
                        note: String::new(),
                        snapshot: Snapshot::default(),
                        since: HashMap::new(),
                        gone: None,
                    });
                    st.gone = None;
                    st.sock = m.sock.clone();
                }
                for (name, st) in self.machines.iter_mut() {
                    if !list.iter().any(|m| &m.name == name) && st.gone.is_none() {
                        st.gone = Some(Instant::now());
                        st.up = false;
                    }
                }
                self.rebuild();
            }
            Msg::Snapshot(name, snap) => {
                let st = self.machines.entry(name).or_insert_with(|| MachineState {
                    sock: PathBuf::new(),
                    local: false,
                    up: false,
                    note: String::new(),
                    snapshot: Snapshot::default(),
                    since: HashMap::new(),
                    gone: None,
                });
                st.up = true;
                st.note.clear();
                let now = Instant::now();
                for p in &snap.panes {
                    match st.since.get(&p.pane_id) {
                        Some((s, _)) if *s == p.agent_status => {}
                        _ => {
                            st.since.insert(p.pane_id.clone(), (p.agent_status, now));
                        }
                    }
                }
                st.since.retain(|k, _| snap.panes.iter().any(|p| &p.pane_id == k));
                st.snapshot = snap;
                self.rebuild();
            }
            Msg::Down(name, why) => {
                if let Some(st) = self.machines.get_mut(&name) {
                    st.up = false;
                    st.note = why;
                    self.rebuild();
                }
            }
            Msg::Screen(t, text) => {
                if self.current().is_some_and(|e| e.target == t) {
                    self.screen = Some((t, text));
                    self.screen_err = None;
                }
            }
            Msg::ScreenErr(t, e) => {
                if self.current().is_some_and(|c| c.target == t) {
                    self.screen_err = Some(e);
                }
            }
            Msg::Toast(s) => self.toast(s),
            Msg::Input(_) | Msg::Refresh => {}
        }
    }

    /// Time-based housekeeping: old toasts, machines gone for a minute.
    pub fn tick(&mut self) {
        self.toasts.retain(|(_, t)| t.elapsed() < TOAST_FOR);
        let before = self.machines.len();
        self.machines.retain(|_, m| !m.gone.is_some_and(|t| t.elapsed() > GONE_FOR));
        if self.machines.len() != before {
            self.rebuild();
        }
    }

    /// Machines → workspaces → panes, flattened for the sidebar, filtered, with the selection kept
    /// on the same pane when it is still there.
    pub fn rebuild(&mut self) {
        let now = Instant::now();
        let filter = self.filter.to_lowercase();
        let mut rows = Vec::new();
        for name in self.order() {
            let m = &self.machines[&name];
            let note = if m.gone.is_some() {
                "gone".to_string()
            } else if !m.up {
                if m.note.is_empty() { "connecting…".to_string() } else { "down".to_string() }
            } else {
                String::new()
            };
            rows.push(Row::Header { machine: name.clone(), note });
            for ws in &m.snapshot.workspaces {
                let panes: Vec<_> = m.snapshot.panes.iter().filter(|p| p.workspace_id == ws.workspace_id).collect();
                for p in &panes {
                    if m.local && self.own_pane.as_deref() == Some(p.pane_id.as_str()) {
                        continue;
                    }
                    let shell = p.agent.is_none();
                    if shell && !self.show_shells {
                        continue;
                    }
                    let mut label = if ws.label.trim().is_empty() {
                        p.terminal_title_stripped.clone().unwrap_or_else(|| basename(p.dir()))
                    } else {
                        ws.label.trim().to_string()
                    };
                    if panes.len() > 1 {
                        if let Some(t) = p.terminal_title_stripped.as_deref().filter(|t| !t.is_empty() && *t != label) {
                            label = format!("{label} · {t}");
                        }
                    }
                    let entry = Entry {
                        target: Target { machine: name.clone(), pane: p.pane_id.clone() },
                        workspace_id: ws.workspace_id.clone(),
                        label,
                        kind: p.agent.clone().unwrap_or_else(|| "shell".into()),
                        status: p.agent_status,
                        dir: p.dir().to_string(),
                        since: m.since.get(&p.pane_id).map_or(now, |s| s.1),
                        shell,
                    };
                    if !filter.is_empty() {
                        let hay = format!("{} {} {} {}", entry.label, entry.kind, entry.dir, name).to_lowercase();
                        if !hay.contains(&filter) {
                            continue;
                        }
                    }
                    rows.push(Row::Agent(entry));
                }
            }
        }
        self.rows = rows;
        let found = self.selected_id.as_ref().and_then(|t| self.rows.iter().position(|r| matches!(r, Row::Agent(e) if &e.target == t)));
        self.selected = found.or_else(|| self.first_agent()).unwrap_or(0);
        self.after_move();
    }

    fn first_agent(&self) -> Option<usize> {
        self.rows.iter().position(|r| matches!(r, Row::Agent(_)))
    }

    fn after_move(&mut self) {
        self.selected_id = self.current().map(|e| e.target.clone());
        if self.screen.as_ref().is_some_and(|(t, _)| Some(t) != self.selected_id.as_ref()) {
            self.screen = None;
            self.screen_err = None;
        }
    }

    pub fn move_sel(&mut self, delta: i32) {
        let mut i = self.selected as i32;
        loop {
            i += delta;
            if i < 0 || i as usize >= self.rows.len() {
                return;
            }
            if matches!(self.rows[i as usize], Row::Agent(_)) {
                self.selected = i as usize;
                self.after_move();
                return;
            }
        }
    }

    pub fn jump(&mut self, n: usize) {
        if let Some(i) = self.rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::Agent(_))).nth(n.saturating_sub(1)).map(|(i, _)| i) {
            self.selected = i;
            self.after_move();
        }
    }

    /// One key's effect. False means quit.
    pub fn handle(&mut self, a: Action, tx: &Sender<Msg>) -> bool {
        if a == Action::Quit {
            return false;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(a, tx),
            Mode::Insert => self.handle_insert(a, tx),
            Mode::Filter => {
                match a {
                    Action::Char(c) => self.filter.push(c),
                    Action::Backspace => {
                        self.filter.pop();
                    }
                    Action::Confirm => self.mode = Mode::Normal,
                    Action::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    _ => {}
                }
                self.rebuild();
            }
            Mode::Help => self.mode = Mode::Normal,
            Mode::Close => {
                if a == Action::Confirm {
                    if let Some(e) = self.current().cloned() {
                        if let Some(m) = self.machines.get(&e.target.machine) {
                            actions::close_workspace(m.sock.clone(), e.workspace_id.clone(), e.label.clone(), tx.clone());
                            self.toast(format!("closing {}…", e.label));
                        }
                    }
                }
                self.mode = Mode::Normal;
            }
            Mode::New => return self.handle_new(a, tx),
        }
        true
    }

    fn handle_normal(&mut self, a: Action, tx: &Sender<Msg>) {
        match a {
            Action::Up => self.move_sel(-1),
            Action::Down => self.move_sel(1),
            Action::Jump(n) => self.jump(n),
            Action::Open => {
                if self.own_pane.is_some() {
                    self.attach(tx);
                } else if self.narrow() {
                    self.narrow_agent = true;
                } else if self.current().is_some() {
                    self.mode = Mode::Insert;
                }
            }
            Action::Attach => self.attach(tx),
            Action::Compose => {
                if self.current().is_some() {
                    self.narrow_agent = true;
                    self.mode = Mode::Insert;
                }
            }
            Action::Tab => {
                if self.narrow() {
                    self.narrow_agent = !self.narrow_agent;
                } else if self.current().is_some() {
                    self.mode = Mode::Insert;
                }
            }
            Action::New => self.open_new(),
            Action::Close => {
                if self.current().is_some() {
                    self.mode = Mode::Close;
                }
            }
            Action::Filter => self.mode = Mode::Filter,
            Action::Refresh => {
                let _ = tx.send(Msg::Refresh);
            }
            Action::ToggleShells => {
                self.show_shells = !self.show_shells;
                self.rebuild();
            }
            Action::ToggleSidebar => self.sidebar = !self.sidebar,
            Action::Help => self.mode = Mode::Help,
            Action::Esc => {
                self.narrow_agent = false;
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                }
            }
            _ => {}
        }
    }

    fn handle_insert(&mut self, a: Action, tx: &Sender<Msg>) {
        match a {
            Action::Send => {
                let text = self.composer.take();
                if text.trim().is_empty() {
                    return;
                }
                if let Some(e) = self.current().cloned() {
                    if let Some(m) = self.machines.get(&e.target.machine) {
                        if e.shell {
                            actions::send_text(m.sock.clone(), e.target.pane.clone(), text, tx.clone());
                        } else {
                            actions::prompt(m.sock.clone(), e.target.pane.clone(), text, tx.clone());
                        }
                        self.toast(format!("→ {}", e.label));
                    }
                }
            }
            Action::Newline => self.composer.insert('\n'),
            Action::Esc | Action::Tab => self.mode = Mode::Normal,
            Action::Char(c) => self.composer.insert(c),
            Action::Paste(s) => self.composer.insert_str(&s),
            Action::Backspace => self.composer.backspace(),
            Action::Delete => self.composer.delete(),
            Action::Left => self.composer.cursor = self.composer.cursor.saturating_sub(1),
            Action::Right => self.composer.cursor = (self.composer.cursor + 1).min(self.composer.len()),
            Action::Home => self.composer.cursor = 0,
            Action::End => self.composer.cursor = self.composer.len(),
            _ => {}
        }
    }

    fn attach(&mut self, tx: &Sender<Msg>) {
        let Some(e) = self.current().cloned() else { return };
        let Some(m) = self.machines.get(&e.target.machine) else { return };
        if self.own_pane.is_none() {
            self.toast("attach needs the herdr client: open FEMME from inside herdr (prefix+f)");
            return;
        }
        if !m.local {
            self.toast(format!("that agent runs on {}: open it in herdr there", e.target.machine));
            return;
        }
        actions::focus(m.sock.clone(), e.target.pane.clone(), e.workspace_id.clone(), e.shell, tx.clone());
        self.toast(format!("opening {}", e.label));
    }

    fn open_new(&mut self) {
        let machines = self.order();
        if machines.is_empty() {
            self.toast("no herdr to talk to yet");
            return;
        }
        let cur = self.current().map(|e| e.target.machine.clone());
        let machine = cur.and_then(|c| machines.iter().position(|m| *m == c)).unwrap_or(0);
        self.new_agent = NewAgent { machine, ..NewAgent::default() };
        self.set_folders();
        self.mode = Mode::New;
    }

    /// Folders to offer: on this box, home, ~/Documents/*, ~/femme-work/* (what the board offers);
    /// elsewhere only herdr's default, since that disk is not here to list.
    fn set_folders(&mut self) {
        let machines = self.order();
        let local = machines.get(self.new_agent.machine).and_then(|n| self.machines.get(n)).is_some_and(|m| m.local);
        let mut folders = Vec::new();
        if local {
            let home = self.home.display().to_string();
            folders.push(("~".to_string(), Some(home.clone())));
            for base in ["Documents", "femme-work"] {
                let mut dirs: Vec<PathBuf> = std::fs::read_dir(self.home.join(base))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|d| d.path())
                    .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
                    .collect();
                dirs.sort();
                for d in dirs {
                    let shown = d.display().to_string().replacen(&home, "~", 1);
                    folders.push((shown, Some(d.display().to_string())));
                }
            }
        } else {
            folders.push(("herdr's default folder".to_string(), None));
        }
        self.new_agent.folders = folders;
        self.new_agent.folder = 0;
    }

    fn handle_new(&mut self, a: Action, tx: &Sender<Msg>) -> bool {
        let n_machines = self.machines.len();
        let d = &mut self.new_agent;
        let list_step = matches!(d.step, 0 | 1 | 3);
        let (len, idx) = match d.step {
            0 => (n_machines, &mut d.machine),
            1 => (d.folders.len() + 1, &mut d.folder),
            3 => (KINDS.len(), &mut d.kind),
            _ => (0, &mut d.kind),
        };
        match a {
            Action::Esc => {
                if d.step == 0 {
                    self.mode = Mode::Normal;
                } else {
                    d.step = if d.step == 2 { 1 } else { d.step - 1 };
                }
            }
            Action::Up | Action::Char('k') if list_step => *idx = idx.saturating_sub(1),
            Action::Down | Action::Char('j') if list_step => *idx = (*idx + 1).min(len.saturating_sub(1)),
            Action::Char(c) if d.step == 2 => d.custom.push(c),
            Action::Char(c) if d.step == 4 => d.label.push(c),
            Action::Backspace if d.step == 2 => {
                d.custom.pop();
            }
            Action::Backspace if d.step == 4 => {
                d.label.pop();
            }
            Action::Confirm | Action::Tab => match d.step {
                0 => {
                    d.step = 1;
                    self.set_folders();
                }
                1 => d.step = if d.folder == d.folders.len() { 2 } else { 3 },
                2 => {
                    if !d.custom.trim().is_empty() {
                        d.step = 3;
                    }
                }
                3 => d.step = 4,
                _ => self.launch_new(tx),
            },
            _ => {}
        }
        true
    }

    fn launch_new(&mut self, tx: &Sender<Msg>) {
        let machines = self.order();
        let d = &self.new_agent;
        let Some(name) = machines.get(d.machine) else { return };
        let Some(m) = self.machines.get(name) else { return };
        let cwd = if d.folder == d.folders.len() {
            Some(expand_home(d.custom.trim(), &self.home))
        } else {
            d.folders[d.folder].1.clone()
        };
        let kind = KINDS[d.kind].to_string();
        actions::new_agent(m.sock.clone(), name.clone(), cwd, kind.clone(), d.label.trim().to_string(), tx.clone());
        self.toast(format!("starting {kind} on {name}…"));
        self.mode = Mode::Normal;
    }
}

fn basename(p: &str) -> String {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p).to_string()
}

fn expand_home(p: &str, home: &std::path::Path) -> String {
    if p == "~" {
        home.display().to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest).display().to_string()
    } else {
        p.to_string()
    }
}

/// "12s", "12m", "3h", "2d": how long the agent has been in its current state.
pub fn elapsed(since: Instant) -> String {
    let s = since.elapsed().as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// `/home/dora/femme-work/shop` → `~/femme-work/shop`, cut from the left to `max` chars.
pub fn short_dir(dir: &str, home: &std::path::Path, max: usize) -> String {
    let home = home.display().to_string();
    let mut d = if dir.starts_with(&home) { format!("~{}", &dir[home.len()..]) } else { dir.to_string() };
    let n = d.chars().count();
    if n > max && max > 1 {
        d = format!("…{}", d.chars().skip(n - max + 1).collect::<String>());
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::machines::Machine;
    use crate::herdr::types::{parse_snapshot, FIXTURE};

    fn app() -> App {
        let mut app = App::new(Theme::build(true), "agentbox".into(), None, PathBuf::from("/home/dora"));
        app.apply(Msg::Machines(vec![
            Machine { name: "agentbox".into(), sock: PathBuf::from("/tmp/a.sock"), local: true },
            Machine { name: "laptop".into(), sock: PathBuf::from("/tmp/l.sock"), local: false },
        ]));
        let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        app.apply(Msg::Snapshot("agentbox".into(), parse_snapshot(&v["result"]).unwrap()));
        app
    }

    #[test]
    fn rows_hide_shells_until_asked_and_keep_the_selection() {
        let mut app = app();
        let agents = app.rows.iter().filter(|r| matches!(r, Row::Agent(_))).count();
        assert_eq!(agents, 3, "three agent panes in the fixture");
        assert_eq!(app.current().unwrap().target.pane, "w1:p1");
        app.move_sel(1);
        assert_eq!(app.current().unwrap().target.pane, "w2:p1");
        app.show_shells = true;
        app.rebuild();
        assert_eq!(app.current().unwrap().target.pane, "w2:p1", "same pane after a rebuild");
        let agents = app.rows.iter().filter(|r| matches!(r, Row::Agent(_))).count();
        assert_eq!(agents, 5);
        assert_eq!(app.counts(), (1, 2));
    }

    #[test]
    fn filter_matches_label_kind_dir_and_machine() {
        let mut app = app();
        app.filter = "codex".into();
        app.rebuild();
        assert_eq!(app.rows.iter().filter(|r| matches!(r, Row::Agent(_))).count(), 1);
        app.filter = "nothing-like-this".into();
        app.rebuild();
        assert!(app.current().is_none());
    }

    #[test]
    fn composer_edits_by_char() {
        let mut c = Composer::default();
        for ch in "héllo".chars() {
            c.insert(ch);
        }
        c.cursor = 1;
        c.backspace();
        assert_eq!(c.text, "éllo");
        c.delete();
        assert_eq!(c.text, "llo");
        assert_eq!(c.take(), "llo");
        assert!(c.text.is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(short_dir("/home/dora/femme-work/shop", std::path::Path::new("/home/dora"), 40), "~/femme-work/shop");
        assert_eq!(short_dir("/home/dora/femme-work/shop", std::path::Path::new("/home/dora"), 8), "…rk/shop");
        assert_eq!(basename("/home/dora/x/"), "x");
        assert_eq!(expand_home("~/x", std::path::Path::new("/h")), "/h/x");
    }
}
