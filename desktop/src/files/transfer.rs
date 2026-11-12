//! Moving files between this machine and the session's directory: the rules as plain data, no
//! gpui. Names a taken file gets (` (2)`), where downloads land, the `text/uri-list` a copied file
//! is put on the clipboard as, and the queue of uploads with where each one stands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use femme_proto::rpc::MAX_FS_WRITE;

use super::size::fmt_size;
use super::tree::{base, join, Pending};

/// `name` with ` (n)` before its extension: `x (2).pdf`, `notes (3)`, `.env (2)`. The extension
/// is what follows the last dot, unless that dot starts the name.
pub fn suffixed(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// `name` when `taken` says it is free, else the first free [`suffixed`] one from ` (2)` up.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| suffixed(name, n))
        .find(|c| !taken(c))
        .expect("some suffix is free")
}

/// How many bytes the piece at `offset` of a `total`-byte file carries.
pub fn piece_len(offset: u64, total: u64) -> usize {
    total.saturating_sub(offset).min(MAX_FS_WRITE as u64) as usize
}

/// `len` bytes of the local file `path` from `offset`: one piece, so a big file is never held
/// whole. Fewer bytes than asked means the file shrank since it was measured: refused.
pub fn read_piece(path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = vec![0; len];
    f.read_exact(&mut buf)
        .map_err(|_| "the file changed while it was sent".to_owned())?;
    Ok(buf)
}

/// `sent` of `total` bytes in the unit of `total`: `3.2 / 30 MB`, `512 / 900 B`.
pub fn progress(sent: u64, total: u64) -> String {
    let whole = fmt_size(total);
    let Some((_, unit)) = whole.split_once(' ') else {
        return whole;
    };
    let scale = match unit {
        "KB" => 1u64 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => return format!("{sent} / {whole}"),
    };
    let v = sent as f64 / scale as f64;
    if v < 9.95 {
        format!("{v:.1} / {whole}")
    } else {
        format!("{v:.0} / {whole}")
    }
}

/// `path` as one line of `text/uri-list`: `file://` and the path with every byte that is not
/// unreserved (or `/`) percent-encoded, ended by CRLF.
pub fn uri_list(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push_str("\r\n");
    out
}

/// The files a copied text names, when every line of it is one: a `file://` URI (percent-encoded,
/// as file managers copy them) or an absolute path. gpui's Linux clipboard hands copied files over
/// as that text, never as `ExternalPaths`. `None` for any other text.
pub fn paths_in_text(text: &str) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let raw = match line.strip_prefix("file://") {
            // `file:///x` or `file://localhost/x`.
            Some(rest) => rest.strip_prefix("localhost").unwrap_or(rest),
            None => line,
        };
        if !raw.starts_with('/') {
            return None;
        }
        let bytes = if line.starts_with("file://") {
            unpercent(raw)?
        } else {
            raw.as_bytes().to_vec()
        };
        out.push(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
    }
    (!out.is_empty()).then_some(out)
}

/// `%XX` escapes decoded; `None` for a broken one.
fn unpercent(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Puts the file `path` on the clipboard as a `text/uri-list`, so a paste in the file manager
/// drops the file: through `wl-copy` (Wayland) or `xclip` (X11), whichever works first, the one
/// of the session's display first. Whether one did. Blocks: run it off the UI thread.
pub fn clip_uri(path: &Path) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let text = uri_list(path);
    let wl: (&str, &[&str]) = ("wl-copy", &["-t", "text/uri-list"]);
    let x: (&str, &[&str]) = ("xclip", &["-selection", "clipboard", "-t", "text/uri-list"]);
    let order = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        [wl, x]
    } else {
        [x, wl]
    };
    order.iter().any(|(cmd, args)| {
        // Both fork to serve the clipboard and the parent exits: waiting for it is short. Their
        // output is not read (the child that stays would hold a pipe open).
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        child.wait().is_ok_and(|s| s.success())
    })
}

/// The download folder `user-dirs.dirs` (its `contents`) names, `$HOME` put in: the line
/// `XDG_DOWNLOAD_DIR="$HOME/İndirilenler"`. `None` when it names none or `$HOME` itself.
pub fn xdg_download(contents: &str, home: &Path) -> Option<PathBuf> {
    let line = contents
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("XDG_DOWNLOAD_DIR="))?;
    let value = line["XDG_DOWNLOAD_DIR=".len()..].trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None if value.starts_with('/') => PathBuf::from(value),
        None => return None,
    };
    (path != home).then_some(path)
}

/// Where downloads land on this machine: the XDG download folder, else `~/Downloads`.
pub fn downloads_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let config =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|c| xdg_download(&c, &home))
        .unwrap_or_else(|| home.join("Downloads"))
}

/// `path` with the home folder written `~`.
pub fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) && !home.as_os_str().is_empty() => {
            let rest = path.strip_prefix(&home).unwrap_or(path);
            Path::new("~").join(rest).display().to_string()
        }
        _ => path.display().to_string(),
    }
}

/// Saves `data` as `name` in `dir` (made if missing) under a free name; the path written.
pub fn save_new(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", tilde(dir)))?;
    let name = free_name(name, |n| dir.join(n).exists());
    let path = dir.join(name);
    std::fs::write(&path, data).map_err(|e| format!("{}: {e}", tilde(&path)))?;
    Ok(path)
}

/// Where an upload's bytes come from.
#[derive(Clone, Debug)]
pub enum Origin {
    /// A file on this machine, read a piece at a time.
    Local(PathBuf),
    /// Bytes held already (a pasted picture).
    Bytes(Arc<Vec<u8>>),
}

/// Where an upload stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpState {
    /// Waits for the ones before it.
    Queued,
    /// Its pieces are going out.
    Sending,
    /// Whole on the box.
    Done,
    /// A piece was refused or could not be read: why.
    Failed(String),
}

/// One file going up.
#[derive(Clone, Debug)]
pub struct Upload {
    /// Which one it is (the tree's [`Pending::id`]).
    pub id: u64,
    /// Where its bytes come from.
    pub origin: Origin,
    /// The directory it goes into (relative to the session's).
    pub dir: String,
    /// The name it was asked under.
    pub asked: String,
    /// The name it has on the box: [`Upload::asked`] until the first answer gives the final one.
    pub name: String,
    /// Its size.
    pub total: u64,
    /// The bytes the box has.
    pub sent: u64,
    /// Where it stands.
    pub state: UpState,
}

impl Upload {
    /// Its path on the box.
    pub fn path(&self) -> String {
        join(&self.dir, &self.name).unwrap_or_else(|| self.name.clone())
    }
}

/// What the next piece of an upload is: where it comes from, the path to send, the offset, the
/// total.
pub type Next = (Origin, String, u64, u64);

/// The uploads, in the order they were asked for; they go one after the other.
#[derive(Debug, Default)]
pub struct Uploads {
    list: Vec<Upload>,
    next_id: u64,
    /// The count in the header: uploads in the batch under way, and how many of them ended.
    batch: usize,
    ended: usize,
}

impl Uploads {
    /// Queues `name` (of `total` bytes) for the directory `dir`; its id. `None` for a name that
    /// is not a plain path component.
    pub fn add(&mut self, origin: Origin, dir: &str, name: &str, total: u64) -> Option<u64> {
        join(dir, name)?;
        self.next_id += 1;
        if !self.active() {
            self.batch = 0;
            self.ended = 0;
        }
        self.batch += 1;
        self.list.push(Upload {
            id: self.next_id,
            origin,
            dir: dir.to_owned(),
            asked: name.to_owned(),
            name: name.to_owned(),
            total,
            sent: 0,
            state: UpState::Queued,
        });
        Some(self.next_id)
    }

    /// The upload `id`.
    pub fn get(&self, id: u64) -> Option<&Upload> {
        self.list.iter().find(|u| u.id == id)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Upload> {
        self.list.iter_mut().find(|u| u.id == id)
    }

    /// The first queued upload, now marked as sending; `None` when there is none.
    pub fn start_next(&mut self) -> Option<u64> {
        let u = self.list.iter_mut().find(|u| u.state == UpState::Queued)?;
        u.state = UpState::Sending;
        Some(u.id)
    }

    /// The next piece of the upload `id` while it is sending: the first goes under the name
    /// asked, the next ones under the name the first answer gave.
    pub fn next(&self, id: u64) -> Option<Next> {
        let u = self.get(id).filter(|u| u.state == UpState::Sending)?;
        Some((u.origin.clone(), u.path(), u.sent, u.total))
    }

    /// The box took a piece of `id`: it is `path` now, with `written` bytes; `done` when whole.
    pub fn landed(&mut self, id: u64, path: &str, written: u64, done: bool) {
        let Some(u) = self.get_mut(id) else { return };
        u.name = base(path).to_owned();
        u.sent = written.min(u.total);
        if done {
            u.state = UpState::Done;
            u.sent = u.total;
            self.ended += 1;
        }
    }

    /// The upload `id` failed: `why`.
    pub fn failed(&mut self, id: u64, why: String) {
        if let Some(u) = self.get_mut(id) {
            u.state = UpState::Failed(why);
            self.ended += 1;
        }
    }

    /// Queues a failed upload again, from the start and under the name asked.
    pub fn retry(&mut self, id: u64) -> bool {
        let fresh = !self.active();
        let Some(u) = self.get_mut(id) else {
            return false;
        };
        if !matches!(u.state, UpState::Failed(_)) {
            return false;
        }
        u.state = UpState::Queued;
        u.sent = 0;
        u.name = u.asked.clone();
        if fresh {
            self.batch = 0;
            self.ended = 0;
        }
        self.batch += 1;
        true
    }

    /// Whether one is queued or sending.
    pub fn active(&self) -> bool {
        self.list
            .iter()
            .any(|u| matches!(u.state, UpState::Queued | UpState::Sending))
    }

    /// `(k, n)`: the `k`th of the `n` uploads of the batch is on its way. `None` when none is,
    /// or the batch is a single file.
    pub fn counter(&self) -> Option<(usize, usize)> {
        (self.active() && self.batch > 1).then(|| ((self.ended + 1).min(self.batch), self.batch))
    }

    /// The rows the tree shows for them; a finished one gives way to the listed file of its name.
    pub fn pending(&self) -> Vec<Pending> {
        self.list
            .iter()
            .map(|u| Pending {
                path: u.path(),
                id: u.id,
                yields: u.state == UpState::Done,
            })
            .collect()
    }

    /// A listing of `dir` came in with `names`: the finished uploads it holds are dropped.
    pub fn listed(&mut self, dir: &str, names: &[&str]) {
        self.list.retain(|u| {
            !(u.state == UpState::Done && u.dir == dir && names.contains(&u.name.as_str()))
        });
    }

    /// Forgets them all (another session).
    pub fn clear(&mut self) {
        self.list.clear();
        self.batch = 0;
        self.ended = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suffix_goes_before_the_extension() {
        assert_eq!(suffixed("x.pdf", 2), "x (2).pdf");
        assert_eq!(suffixed("a.tar.gz", 3), "a.tar (3).gz");
        assert_eq!(suffixed("notes", 2), "notes (2)");
        assert_eq!(suffixed(".env", 2), ".env (2)");
    }

    #[test]
    fn a_free_name_skips_the_taken_ones() {
        let taken = ["x.pdf", "x (2).pdf"];
        assert_eq!(free_name("y.pdf", |n| taken.contains(&n)), "y.pdf");
        assert_eq!(free_name("x.pdf", |n| taken.contains(&n)), "x (3).pdf");
    }

    #[test]
    fn pieces_cover_the_file_and_the_last_is_short() {
        let m = MAX_FS_WRITE as u64;
        assert_eq!(piece_len(0, 10), 10);
        assert_eq!(piece_len(0, 3 * m), MAX_FS_WRITE);
        assert_eq!(piece_len(2 * m, 2 * m + 5), 5);
        assert_eq!(piece_len(5, 5), 0);
        assert_eq!(piece_len(0, 0), 0);
        let (mut offset, total, mut n) = (0, 2 * m + 7, 0);
        loop {
            offset += piece_len(offset, total) as u64;
            n += 1;
            if offset == total {
                break;
            }
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn a_piece_is_read_from_its_offset() {
        let p = std::env::temp_dir().join(format!("femme-piece-{}", std::process::id()));
        std::fs::write(&p, b"0123456789").unwrap();
        assert_eq!(read_piece(&p, 3, 4).unwrap(), b"3456");
        assert!(read_piece(&p, 8, 4).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn progress_shares_the_unit_of_the_total() {
        assert_eq!(progress(3_355_443, 30 << 20), "3.2 / 30 MB");
        assert_eq!(progress(0, 16 << 20), "0.0 / 16 MB");
        assert_eq!(progress(12 << 20, 16 << 20), "12 / 16 MB");
        assert_eq!(progress(100, 900), "100 / 900 B");
    }

    #[test]
    fn a_uri_list_line_encodes_what_is_not_plain() {
        assert_eq!(
            uri_list(Path::new("/home/c/Downloads/a b (2).pdf")),
            "file:///home/c/Downloads/a%20b%20%282%29.pdf\r\n"
        );
        assert_eq!(uri_list(Path::new("/tmp/ı")), "file:///tmp/%C4%B1\r\n");
    }

    #[test]
    fn copied_files_are_read_back_from_text() {
        let line = uri_list(Path::new("/tmp/a b (2).pdf"));
        assert_eq!(
            paths_in_text(&line),
            Some(vec![PathBuf::from("/tmp/a b (2).pdf")])
        );
        assert_eq!(
            paths_in_text("/home/c/x.txt\nfile://localhost/tmp/%C4%B1\n"),
            Some(vec![
                PathBuf::from("/home/c/x.txt"),
                PathBuf::from("/tmp/ı")
            ])
        );
        assert_eq!(paths_in_text("hello"), None);
        assert_eq!(paths_in_text("/a\nnot a path"), None);
        assert_eq!(paths_in_text("file:///bad%zz"), None);
        assert_eq!(paths_in_text("  \n"), None);
    }

    #[test]
    fn the_xdg_download_folder_is_read_with_home_put_in() {
        let home = Path::new("/home/c");
        let dirs = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/İndirilenler\"\n";
        assert_eq!(
            xdg_download(dirs, home),
            Some(PathBuf::from("/home/c/İndirilenler"))
        );
        assert_eq!(
            xdg_download("XDG_DOWNLOAD_DIR=\"/data/dl\"", home),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(xdg_download("XDG_DOWNLOAD_DIR=\"$HOME/\"", home), None);
        assert_eq!(xdg_download("", home), None);
    }

    #[test]
    fn a_download_takes_a_free_name() {
        let dir = std::env::temp_dir().join(format!("femme-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = save_new(&dir, "x.pdf", b"1").unwrap();
        let b = save_new(&dir, "x.pdf", b"2").unwrap();
        assert_eq!(a.file_name().unwrap(), "x.pdf");
        assert_eq!(b.file_name().unwrap(), "x (2).pdf");
        assert_eq!(std::fs::read(&b).unwrap(), b"2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn bytes() -> Origin {
        Origin::Bytes(Arc::new(vec![1, 2, 3]))
    }

    #[test]
    fn uploads_go_one_after_the_other_and_take_the_final_name() {
        let mut ups = Uploads::default();
        let a = ups.add(bytes(), "", "a.txt", 10).unwrap();
        let b = ups.add(bytes(), "sub", "b.txt", 0).unwrap();
        assert!(ups.add(bytes(), "", "../x", 1).is_none());
        assert_eq!(ups.counter(), Some((1, 2)));
        assert_eq!(ups.start_next(), Some(a));
        assert!(ups.next(b).is_none(), "b waits");
        let (_, path, offset, total) = ups.next(a).unwrap();
        assert_eq!((path.as_str(), offset, total), ("a.txt", 0, 10));
        ups.landed(a, "a (2).txt", 4, false);
        let (_, path, offset, _) = ups.next(a).unwrap();
        assert_eq!((path.as_str(), offset), ("a (2).txt", 4));
        ups.landed(a, "a (2).txt", 10, true);
        assert_eq!(ups.get(a).unwrap().state, UpState::Done);
        assert_eq!(ups.counter(), Some((2, 2)));
        assert_eq!(ups.start_next(), Some(b));
        assert_eq!(ups.next(b).unwrap().1, "sub/b.txt");
        ups.failed(b, "Bad: no".into());
        assert!(!ups.active());
        assert_eq!(ups.counter(), None);
        assert_eq!(ups.start_next(), None);

        // A retry starts over under the name asked, as a batch of its own.
        assert!(ups.retry(b));
        assert!(!ups.retry(a), "a is done");
        assert_eq!(ups.get(b).unwrap().sent, 0);
        assert_eq!(ups.counter(), None, "a batch of one has no count");

        let rows = ups.pending();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].path.as_str(), rows[0].yields), ("a (2).txt", true));
        assert_eq!(
            (rows[1].path.as_str(), rows[1].yields),
            ("sub/b.txt", false)
        );

        // The listing that holds the finished one drops it; the queued one stays.
        ups.listed("", &["a (2).txt", "other"]);
        ups.listed("sub", &["b.txt"]);
        assert!(ups.get(a).is_none());
        assert!(ups.get(b).is_some());
    }
}
