//! The tree of the files panel as plain data: which directories were listed, which are open, which
//! row the cursor is on. No gpui in here, so every rule is tested with fake listings.
//!
//! Paths are always relative to the session's directory on the box (`""` is that directory): that
//! is what `Req::FsList` and `Req::FsRead` take. They are built only by [`join`], which refuses
//! `..`, `.`, empty names and names with a slash, so no path made here can climb out of the
//! directory the daemon resolves it against, whatever a listing contains.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use femme_proto::rpc::FsEntry;

/// Whether `name` is one plain path component: not empty, not `.` or `..`, no `/` and no NUL.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Whether `path` is `""` or plain components joined by single slashes (no leading or trailing
/// slash, no `.`/`..`).
pub fn clean(path: &str) -> bool {
    path.is_empty() || path.split('/').all(valid_name)
}

/// `dir` and `name` as one path, or `None` when `name` is not a plain component (see
/// [`valid_name`]). `dir` is a path this module made (or `""`).
pub fn join(dir: &str, name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    Some(if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    })
}

/// The directory `path` is in (`""` for a top-level name and for `""`).
pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The last component of `path`.
pub fn base(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// How many components `path` has (`0` for `""`).
pub fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('/').count() + 1
    }
}

/// What the header shows for a view rooted at `root`: the session's directory `cwd` with `root`
/// below it. An unknown (empty) `cwd` is the home directory, `~`.
pub fn under(cwd: &str, root: &str) -> String {
    let base = if cwd.is_empty() {
        "~"
    } else {
        cwd.trim_end_matches('/')
    };
    match (base, root) {
        ("", "") => "/".to_owned(),
        (base, "") => base.to_owned(),
        (base, root) => format!("{base}/{root}"),
    }
}

/// What one line of the tree is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `..`: back up one directory (shown while the view is below the session's directory).
    Up,
    /// A directory; `open` when its children follow.
    Dir {
        /// Whether the children are listed below it.
        open: bool,
    },
    /// A file.
    File {
        /// Its size in bytes.
        len: u64,
    },
    /// Below an open directory whose listing has not arrived.
    Loading,
    /// Below an open directory that has no entries.
    Empty,
    /// Below a directory whose listing failed: a click asks again.
    Failed,
}

/// One line of the flattened tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// For a directory or a file its own path. For [`Kind::Up`] the directory it leads to, for the
    /// placeholders the directory they stand in.
    pub path: String,
    /// Indentation steps below the view's root.
    pub depth: usize,
    /// What the line is.
    pub kind: Kind,
}

impl Row {
    /// What the line is called: the last path component (`..` for [`Kind::Up`], empty for a
    /// placeholder).
    pub fn name(&self) -> &str {
        match self.kind {
            Kind::Up => "..",
            Kind::Dir { .. } | Kind::File { .. } => base(&self.path),
            Kind::Loading | Kind::Empty | Kind::Failed => "",
        }
    }

    /// What the cursor remembers the line by: its path, `..` for the up line, nothing for the
    /// placeholders (the cursor skips them).
    pub fn key(&self) -> Option<&str> {
        match self.kind {
            Kind::Up => Some(".."),
            Kind::Dir { .. } | Kind::File { .. } => Some(&self.path),
            Kind::Loading | Kind::Empty | Kind::Failed => None,
        }
    }

    /// A dot-file or dot-directory.
    pub fn hidden(&self) -> bool {
        matches!(self.kind, Kind::Dir { .. } | Kind::File { .. }) && self.name().starts_with('.')
    }
}

/// One directory's listing and where its (re)load stands.
#[derive(Debug, Default)]
struct Dir {
    /// What the box said last, names checked and sorted; `None` until the first answer.
    entries: Option<Vec<FsEntry>>,
    /// A request for it is out.
    loading: bool,
    /// The last request failed, why. Cleared by the next answer.
    error: Option<String>,
    /// What is held is old: ask again, keep showing it until the new answer.
    stale: bool,
    /// When the last request was answered (well or not).
    answered: Option<Instant>,
}

/// The directories that were listed, the ones that are open, the view's root and the cursor.
#[derive(Debug)]
pub struct Tree {
    /// The directory the view starts at, a path under the session's directory (`""`: that one).
    root: String,
    dirs: HashMap<String, Dir>,
    open: HashSet<String>,
    selected: Option<String>,
    rows: Vec<Row>,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// A tree at the session's directory with nothing listed yet.
    pub fn new() -> Self {
        let mut t = Tree {
            root: String::new(),
            dirs: HashMap::new(),
            open: HashSet::new(),
            selected: None,
            rows: Vec::new(),
        };
        t.dirs.insert(String::new(), Dir::default());
        t.rebuild();
        t
    }

    /// The directory the view starts at (`""` is the session's own).
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The lines, top to bottom.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Whether the directory `path` shows its children.
    pub fn is_open(&self, path: &str) -> bool {
        self.open.contains(path)
    }

    /// Whether a listing is on its way.
    pub fn busy(&self) -> bool {
        self.dirs.values().any(|d| d.loading)
    }

    /// The directories on show (the root and every open one reachable from it) that must be asked
    /// for: never listed, or stale. They are marked as loading, so each is returned once until its
    /// answer ([`Tree::loaded`]) is in.
    pub fn wanted(&mut self) -> Vec<String> {
        let mut want = Vec::new();
        for path in self.shown() {
            let d = self.dirs.entry(path.clone()).or_default();
            let fresh = d.entries.is_none() && d.error.is_none();
            if !d.loading && (fresh || d.stale) {
                d.loading = true;
                d.stale = false;
                want.push(path);
            }
        }
        want
    }

    /// The answer to a listing request for `dir`. Entries with names that are not plain components
    /// are dropped, the rest sorted (directories first, then by name without regard to case). An
    /// error keeps what was held before, if anything, and shows a retry line.
    pub fn loaded(&mut self, dir: &str, answer: Result<Vec<FsEntry>, String>) {
        let Some(d) = self.dirs.get_mut(dir) else {
            return;
        };
        d.loading = false;
        d.answered = Some(Instant::now());
        match answer {
            Ok(mut entries) => {
                entries.retain(|e| valid_name(&e.name));
                entries.sort_by_cached_key(|e| (!e.dir, e.name.to_lowercase(), e.name.clone()));
                entries.dedup_by(|a, b| a.name == b.name && a.dir == b.dir);
                d.entries = Some(entries);
                d.error = None;
            }
            Err(why) => d.error = Some(why),
        }
        self.rebuild();
    }

    /// Marks the listings on show that were answered more than `ttl` ago as stale, so the next
    /// [`Tree::wanted`] asks for them again while they stay on screen. A directory that failed is
    /// left alone (its retry line is there for that). Whether any was marked.
    pub fn expire(&mut self, ttl: Duration) -> bool {
        let mut any = false;
        for path in self.shown() {
            let Some(d) = self.dirs.get_mut(&path) else {
                continue;
            };
            let old = d.answered.is_some_and(|at| at.elapsed() >= ttl);
            if old && d.entries.is_some() && d.error.is_none() && !d.loading && !d.stale {
                d.stale = true;
                any = true;
            }
        }
        any
    }

    /// Asks again for `dir` after a failure: the next [`Tree::wanted`] returns it.
    pub fn retry(&mut self, dir: &str) {
        if let Some(d) = self.dirs.get_mut(dir) {
            if !d.loading {
                d.error = None;
                d.stale = true;
                self.rebuild();
            }
        }
    }

    /// Forgets what the box said. What is on show stays on screen until the new listing arrives
    /// (the next [`Tree::wanted`] asks for all of it again); what is not on show is dropped.
    /// Requests that were out are given up: their answers are no longer expected.
    pub fn refresh(&mut self) {
        let shown: HashSet<String> = self.shown().into_iter().collect();
        self.dirs.retain(|path, _| shown.contains(path));
        for d in self.dirs.values_mut() {
            d.loading = false;
            d.error = None;
            d.stale = true;
        }
        self.rebuild();
    }

    /// Opens or closes the directory `path`; whether anything changed. A newly opened directory
    /// shows a loading line until [`Tree::wanted`] has asked for it and the answer is in.
    pub fn set_open(&mut self, path: &str, open: bool) -> bool {
        if path.is_empty() || !clean(path) {
            return false;
        }
        let changed = if open {
            self.dirs.entry(path.to_owned()).or_default();
            self.open.insert(path.to_owned())
        } else {
            self.open.remove(path)
        };
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Opens the directory if it is closed and closes it if it is open.
    pub fn toggle(&mut self, path: &str) -> bool {
        let open = !self.is_open(path);
        self.set_open(path, open)
    }

    /// Makes the directory `path` the view's root (the `..` line leads back up); whether the view
    /// moved. The cursor leaves.
    pub fn enter(&mut self, path: &str) -> bool {
        if path.is_empty() || !clean(path) || path == self.root {
            return false;
        }
        self.dirs.entry(path.to_owned()).or_default();
        self.root = path.to_owned();
        self.selected = None;
        self.rebuild();
        true
    }

    /// Moves the view's root up one directory (never above the session's); the cursor lands on the
    /// directory just left. Whether the view moved.
    pub fn up(&mut self) -> bool {
        if self.root.is_empty() {
            return false;
        }
        let from = std::mem::take(&mut self.root);
        self.root = parent(&from).to_owned();
        self.dirs.entry(self.root.clone()).or_default();
        self.selected = Some(from);
        self.rebuild();
        true
    }

    /// The path the cursor is on (`..` for the up line).
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Index of the line the cursor is on.
    pub fn selected_index(&self) -> Option<usize> {
        let sel = self.selected.as_deref()?;
        self.rows.iter().position(|r| r.key() == Some(sel))
    }

    /// The line the cursor is on.
    pub fn selected_row(&self) -> Option<&Row> {
        self.selected_index().map(|i| &self.rows[i])
    }

    /// Puts the cursor on the line `key` names (see [`Row::key`]); `None` takes it off. A key that
    /// is not on show leaves the cursor where it was. Whether the cursor moved.
    pub fn select(&mut self, key: Option<&str>) -> bool {
        match key {
            None => self.selected.take().is_some(),
            Some(k) => {
                if self.selected.as_deref() == Some(k)
                    || !self.rows.iter().any(|r| r.key() == Some(k))
                {
                    return false;
                }
                self.selected = Some(k.to_owned());
                true
            }
        }
    }

    /// Moves the cursor `delta` lines down (negative: up), skipping placeholders and stopping at
    /// either end. With no cursor yet, down lands on the first line and up on the last. Returns
    /// the index of the line it is on.
    pub fn step(&mut self, delta: isize) -> Option<usize> {
        let stops: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].key().is_some())
            .collect();
        let last = stops.len().checked_sub(1)?;
        let at = self
            .selected_index()
            .and_then(|i| stops.iter().position(|&s| s == i));
        let to = match at {
            None if delta < 0 => last,
            None => 0,
            Some(p) => p.saturating_add_signed(delta).min(last),
        };
        let row = stops[to];
        self.selected = self.rows[row].key().map(str::to_owned);
        Some(row)
    }

    /// Puts the cursor on the first (`end` false) or the last line it can stop on.
    pub fn edge(&mut self, end: bool) -> Option<usize> {
        self.selected = None;
        self.step(if end { -1 } else { 1 })
    }

    /// The right arrow: opens the directory the cursor is on, or, when it is open already, moves
    /// onto its first child. Whether anything changed.
    pub fn key_right(&mut self) -> bool {
        let Some(i) = self.selected_index() else {
            return false;
        };
        let Row { path, depth, kind } = self.rows[i].clone();
        match kind {
            Kind::Dir { open: false } => self.set_open(&path, true),
            Kind::Dir { open: true } => {
                let child = self.rows.get(i + 1).filter(|r| r.depth > depth);
                match child.and_then(Row::key).map(str::to_owned) {
                    Some(key) => self.select(Some(&key)),
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// The left arrow: closes the directory the cursor is on, or, on anything else, moves onto the
    /// directory it is in (when that one is on show). Whether anything changed.
    pub fn key_left(&mut self) -> bool {
        let Some(row) = self.selected_row().cloned() else {
            return false;
        };
        match row.kind {
            Kind::Dir { open: true } => self.set_open(&row.path, false),
            Kind::Dir { .. } | Kind::File { .. } => {
                let home = parent(&row.path).to_owned();
                !home.is_empty() && self.select(Some(&home))
            }
            _ => false,
        }
    }

    /// The directories on show: the root, then every open directory reachable from it through
    /// listings that are in.
    fn shown(&self) -> Vec<String> {
        let mut out = vec![self.root.clone()];
        let mut next = 0;
        while next < out.len() {
            let dir = out[next].clone();
            next += 1;
            let Some(entries) = self.dirs.get(&dir).and_then(|d| d.entries.as_ref()) else {
                continue;
            };
            for e in entries.iter().filter(|e| e.dir) {
                if let Some(path) = join(&dir, &e.name) {
                    if self.open.contains(&path) {
                        out.push(path);
                    }
                }
            }
        }
        out
    }

    /// Flattens the directories into lines again, and keeps the cursor on a line that exists: when
    /// its own is gone (its directory was closed) it goes to the nearest directory above.
    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if !self.root.is_empty() {
            rows.push(Row {
                path: parent(&self.root).to_owned(),
                depth: 0,
                kind: Kind::Up,
            });
        }
        self.push_children(&self.root, 0, &mut rows);
        self.rows = rows;
        let Some(sel) = self.selected.clone() else {
            return;
        };
        if self.rows.iter().any(|r| r.key() == Some(&sel)) {
            return;
        }
        let mut above = parent(&sel);
        self.selected = loop {
            if above.is_empty() {
                break None;
            }
            if self.rows.iter().any(|r| r.key() == Some(above)) {
                break Some(above.to_owned());
            }
            above = parent(above);
        };
    }

    /// The lines of `dir`'s children, `depth` steps in; open directories follow with their own.
    fn push_children(&self, dir: &str, depth: usize, out: &mut Vec<Row>) {
        let placeholder = |kind| Row {
            path: dir.to_owned(),
            depth,
            kind,
        };
        let Some(d) = self.dirs.get(dir) else {
            out.push(placeholder(Kind::Loading));
            return;
        };
        let Some(entries) = &d.entries else {
            out.push(placeholder(if d.error.is_some() {
                Kind::Failed
            } else {
                Kind::Loading
            }));
            return;
        };
        if entries.is_empty() && d.error.is_none() {
            out.push(placeholder(Kind::Empty));
        }
        for e in entries {
            let Some(path) = join(dir, &e.name) else {
                continue;
            };
            if e.dir {
                let open = self.open.contains(&path);
                out.push(Row {
                    path: path.clone(),
                    depth,
                    kind: Kind::Dir { open },
                });
                if open {
                    self.push_children(&path, depth + 1, out);
                }
            } else {
                out.push(Row {
                    path,
                    depth,
                    kind: Kind::File { len: e.len },
                });
            }
        }
        if d.error.is_some() {
            out.push(placeholder(Kind::Failed));
        }
    }
}

#[cfg(test)]
mod tests;
