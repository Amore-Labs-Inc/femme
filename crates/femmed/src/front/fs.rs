//! `FsList` and `FsRead`: the desktop's file panel looks at a session's folder through the daemon.
//!
//! A path is relative to the session's `cwd` (an absolute one is taken as written). Whatever it
//! names is resolved with every symlink followed and must then lie under `$HOME`, or in the
//! session's own uploads folder (where `Prompt` attachments are saved, which may be outside home
//! when `FEMME_STATE` is). A `..` anywhere in the path is refused before anything is looked at,
//! and an absolute path outside home is refused before the disk is asked whether it exists, so
//! the answer is no oracle for the rest of the box. All the I/O runs on the blocking pool.
//!
//! `FsWrite` saves a file under the same rules, checked on the folder it goes in (the file itself
//! may not exist yet), in pieces: see [`write_piece`].
//!
//! `Folders` looks the same way at home alone, before any session runs there: the desktop picks
//! a project's folder with it, and may have it made.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use femme_proto::codec::MAX_FRAME;
use femme_proto::rpc::{Err as WireErr, ErrCode, FsEntry, Resp, MAX_FS_WRITE};
use femme_proto::Sid;
use sha2::{Digest, Sha256};

use super::req::err;
use crate::daemon::Daemon;

/// The most `FsRead` sends: a frame holds under 16 MiB, and the answer's envelope takes a few
/// bytes of that, so a file of exactly 16 MiB could not be delivered. Bigger is `Bad`.
pub(crate) const READ_MAX: u64 = (MAX_FRAME - 4096) as u64;
/// The most entries `FsList` answers with (after sorting): a folder with more shows its first
/// ones instead of a frame the connection cannot carry.
const LIST_MAX: usize = 20_000;
/// The longest path the desktop may ask for.
const PATH_MAX: usize = 4096;
/// How far a taken name is numbered (` (2)`, ` (3)`, ...) before `FsWrite` gives up on it.
const COPIES_MAX: u32 = 1000;

fn bad(msg: impl Into<String>) -> WireErr {
    err(ErrCode::Bad, msg)
}

/// What a request's path is resolved against.
struct Scope {
    /// `$HOME` as configured, and as the disk resolves it.
    home: [PathBuf; 2],
    /// The session's uploads folder: `<state>/uploads/<sid>`, as configured and resolved.
    uploads: [PathBuf; 2],
    /// The session's folder.
    cwd: PathBuf,
}

fn resolved(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl Scope {
    fn new(d: &Daemon, sid: Sid, cwd: &str) -> Scope {
        let uploads = d.cfg.state_path("uploads").join(sid.to_string());
        Scope {
            home: [resolved(&d.cfg.home), d.cfg.home.clone()],
            uploads: [resolved(&uploads), uploads],
            cwd: PathBuf::from(cwd),
        }
    }

    fn holds(&self, p: &Path) -> bool {
        self.home
            .iter()
            .chain(&self.uploads)
            .any(|r| p.starts_with(r))
    }

    /// A scope of home alone, for a request no session is behind: the uploads slot repeats home.
    fn home(d: &Daemon) -> Scope {
        let home = [resolved(&d.cfg.home), d.cfg.home.clone()];
        Scope {
            uploads: home.clone(),
            home,
            cwd: d.cfg.home.clone(),
        }
    }

    /// The real path `path` names, if it is one the desktop may see.
    fn resolve(&self, path: &str) -> Result<PathBuf, WireErr> {
        // Every symlink on the way is followed here: what is left is where the bytes are.
        let real = std::fs::canonicalize(self.join(path)?).map_err(io_err)?;
        if self.holds(&real) {
            Ok(real)
        } else {
            Err(outside())
        }
    }

    /// `path` against the session's folder, refused if it climbs or names a place out of scope,
    /// before the disk is asked anything.
    fn join(&self, path: &str) -> Result<PathBuf, WireErr> {
        if path.len() > PATH_MAX || path.contains('\0') {
            return Err(bad("not a path"));
        }
        let asked = Path::new(path);
        if asked
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(bad("`..` is not allowed in a path"));
        }
        if asked.is_absolute() {
            if !self.holds(asked) {
                return Err(outside());
            }
            Ok(asked.to_path_buf())
        } else {
            Ok(self.cwd.join(asked))
        }
    }

    /// The folder a file `path` names would be in, resolved and in scope, and the file's name.
    /// The folder must exist: nothing is made on the way.
    fn parent<'a>(&self, path: &'a str) -> Result<(PathBuf, &'a str), WireErr> {
        let joined = self.join(path)?;
        let name = path.rsplit('/').next().unwrap_or_default();
        if name.is_empty() || name == "." {
            return Err(bad("not a file name"));
        }
        let dir = joined.parent().ok_or_else(|| bad("not a file name"))?;
        let real = std::fs::canonicalize(dir).map_err(|e| match e.kind() {
            ErrorKind::NotFound => bad("no such folder"),
            _ => io_err(e),
        })?;
        if !self.holds(&real) {
            return Err(outside());
        }
        if !real.is_dir() {
            return Err(bad("not a folder"));
        }
        Ok((real, name))
    }

    /// Makes the folder `path` names, with any folders missing on the way. The deepest part that
    /// is already there is resolved first and must lie in scope, so a symlink on the way cannot
    /// have the folders made somewhere else.
    fn make(&self, path: &str) -> Result<(), WireErr> {
        let joined = self.join(path)?;
        let there = joined
            .ancestors()
            .find_map(|a| std::fs::canonicalize(a).ok())
            .ok_or_else(outside)?;
        if !self.holds(&there) {
            return Err(outside());
        }
        std::fs::create_dir_all(&joined).map_err(io_err)
    }
}

fn outside() -> WireErr {
    bad("that path is outside the home folder")
}

/// The answer to an I/O failure. Every call here is about the path the desktop sent, so what is
/// missing is `NotFound` and the rest (a loop of symlinks, a name too long, no permission) is
/// the path's fault: `Bad`, with the system's words.
fn io_err(e: std::io::Error) -> WireErr {
    match e.kind() {
        ErrorKind::NotFound => err(ErrCode::NotFound, "no such file or folder"),
        ErrorKind::PermissionDenied => bad("permission denied"),
        ErrorKind::NotADirectory => bad("not a folder"),
        ErrorKind::IsADirectory => bad("a folder, not a file"),
        _ => bad(format!("cannot read that path: {e}")),
    }
}

/// Runs the disk work off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, WireErr> + Send + 'static,
) -> Result<T, WireErr> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(err(ErrCode::Internal, format!("file access: {e}"))))
}

/// The scope of a request for the open session `sid`; `NotFound` when there is none.
fn scope(d: &Daemon, sid: Sid) -> Result<Scope, WireErr> {
    let s = d
        .session(sid)
        .map_err(|_| err(ErrCode::NotFound, "no such session"))?;
    Ok(Scope::new(d, sid, &s.cwd))
}

/// `FsList`: the entries of a folder, hidden ones too, folders first, each group by name.
pub(crate) async fn list(d: &Daemon, sid: Sid, path: String) -> Result<Resp, WireErr> {
    let scope = scope(d, sid)?;
    blocking(move || list_dir(&scope.resolve(&path)?))
        .await
        .map(Resp::FsEntries)
}

/// `FsRead`: a file's bytes, at most [`READ_MAX`] of them.
pub(crate) async fn read(d: &Daemon, sid: Sid, path: String) -> Result<Resp, WireErr> {
    let scope = scope(d, sid)?;
    blocking(move || read_file(&scope.resolve(&path)?))
        .await
        .map(Resp::FsData)
}

/// One `FsWrite`: what the request carries besides its session.
pub(crate) struct Piece {
    pub path: String,
    pub offset: u64,
    pub total: u64,
    pub data: Vec<u8>,
    pub replace: bool,
    pub was: Option<[u8; 32]>,
}

/// `FsWrite`: one piece of a file, the last one putting it in place.
pub(crate) async fn write(d: &Daemon, sid: Sid, piece: Piece) -> Result<Resp, WireErr> {
    let scope = scope(d, sid)?;
    blocking(move || write_piece(&scope, piece)).await
}

/// `Folders`: the folders in a folder of home, for picking a project's folder. `~`, `~/` and a
/// relative path start at home; with `make` a missing folder is made first.
pub(crate) async fn folders(d: &Daemon, path: String, make: bool) -> Result<Resp, WireErr> {
    let scope = Scope::home(d);
    blocking(move || {
        let path = match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
            _ => path.as_str(),
        };
        if make {
            scope.make(path)?;
        }
        let real = scope.resolve(path)?;
        let names = list_dir(&real)?
            .into_iter()
            .filter(|e| e.dir)
            .map(|e| e.name)
            .collect();
        Ok(Resp::Folders {
            path: real.to_string_lossy().into_owned(),
            names,
        })
    })
    .await
}

fn list_dir(dir: &Path) -> Result<Vec<FsEntry>, WireErr> {
    if !std::fs::metadata(dir).map_err(io_err)?.is_dir() {
        return Err(bad("not a folder"));
    }
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for entry in std::fs::read_dir(dir).map_err(io_err)?.flatten() {
        // A symlink shows as what it points to; one that points nowhere is an empty file.
        let (is_dir, len) = std::fs::metadata(entry.path())
            .map(|m| (m.is_dir(), if m.is_dir() { 0 } else { m.len() }))
            .unwrap_or((false, 0));
        let e = FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: is_dir,
            len,
        };
        if is_dir {
            dirs.push(e);
        } else {
            files.push(e);
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.extend(files);
    if dirs.len() > LIST_MAX {
        tracing::warn!(dir = %dir.display(), n = dirs.len(), "folder listing cut");
        dirs.truncate(LIST_MAX);
    }
    Ok(dirs)
}

fn read_file(file: &Path) -> Result<Vec<u8>, WireErr> {
    let meta = std::fs::metadata(file).map_err(io_err)?;
    // A pipe or a device would never end: only plain files are read.
    if !meta.is_file() {
        return Err(bad(if meta.is_dir() {
            "a folder, not a file"
        } else {
            "not a regular file"
        }));
    }
    let too_big = || {
        bad(format!(
            "files over {} bytes are not sent (this one is bigger)",
            READ_MAX
        ))
    };
    if meta.len() > READ_MAX {
        return Err(too_big());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    // One byte past the cap says the file grew since `metadata`.
    std::fs::File::open(file)
        .map_err(io_err)?
        .take(READ_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > READ_MAX {
        return Err(too_big());
    }
    Ok(bytes)
}

fn conflict(msg: &str) -> WireErr {
    err(ErrCode::Conflict, msg)
}

/// Writes one piece of a file. Every piece of a transfer carries the same `path`, and each finds
/// the same file again:
///
/// - Without `replace`, the file is the first of `name`, `name (2)`, `name (3)`, ... (the number
///   goes before the last dot: `a (2).txt`) that does not exist yet, so a file already there is
///   never touched. A file that has not landed does not exist yet, so later pieces find the name
///   the first one chose. The answer's `path` is the request's with that name in it.
/// - With `replace` it is `name` itself (through a symlink, if it is one, to a file in scope), and
///   `was`, when given, must be the sha256 of what is there: checked on the first piece and again
///   just before the last one renames over it, so an edit made meanwhile is not lost (`Conflict`).
///
/// The bytes go to `<file>.part` beside it: the first piece (`offset` 0) starts it afresh (a
/// stale one from a transfer that never ended is replaced), a later piece must find it exactly
/// `offset` long, else `Conflict` "piece out of order" (pieces go one after another; a daemon
/// restarted halfway loses nothing but the transfer). The piece that reaches `total` puts it in
/// place: a rename over the old file with `replace`, which keeps the old file's mode; a hard link
/// otherwise, which fails rather than overwrite a file that appeared meanwhile. A new file is
/// `0o600`, like the uploads: only the user reads it until they say otherwise. Two transfers to
/// one name at once share one `.part`, and one of them fails "out of order".
fn write_piece(scope: &Scope, p: Piece) -> Result<Resp, WireErr> {
    if p.data.len() > MAX_FS_WRITE {
        return Err(bad(format!("a piece is at most {MAX_FS_WRITE} bytes")));
    }
    let end = p
        .offset
        .checked_add(p.data.len() as u64)
        .filter(|&end| end <= p.total)
        .ok_or_else(|| bad("the piece ends past the file's total"))?;
    let (dir, name) = scope.parent(&p.path)?;
    let (dest, shown) = if p.replace {
        (replaced(scope, dir.join(name))?, p.path.clone())
    } else {
        let free = free_name(&dir, name)?;
        let shown = match p.path.rfind('/') {
            Some(slash) => format!("{}{free}", &p.path[..=slash]),
            None => free.clone(),
        };
        (dir.join(free), shown)
    };
    let part = {
        let mut s = dest.clone().into_os_string();
        s.push(".part");
        PathBuf::from(s)
    };

    let mut file = if p.offset == 0 {
        if p.replace {
            check_was(&dest, p.was)?;
        }
        match std::fs::remove_file(&part) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(io_err(e)),
            _ => {}
        }
        // `create_new` follows no symlink: the bytes land in a file made here, nowhere else.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&part)
            .map_err(io_err)?
    } else {
        let out_of_order = || conflict("piece out of order");
        let meta = std::fs::symlink_metadata(&part).map_err(|_| out_of_order())?;
        if !meta.is_file() || meta.len() != p.offset {
            return Err(out_of_order());
        }
        OpenOptions::new()
            .append(true)
            .open(&part)
            .map_err(io_err)?
    };
    file.write_all(&p.data).map_err(io_err)?;
    let done = end == p.total;
    if done {
        file.sync_all().map_err(io_err)?;
        drop(file);
        land(&part, &dest, p.replace, p.was)?;
    }
    Ok(Resp::FsWritten {
        path: shown,
        written: end,
        done,
    })
}

/// The file `FsWrite` with `replace` writes for `dest`: `dest`, or the file a symlink there
/// points to, which must lie in scope too.
fn replaced(scope: &Scope, dest: PathBuf) -> Result<PathBuf, WireErr> {
    let real = match std::fs::symlink_metadata(&dest) {
        Ok(m) if m.file_type().is_symlink() => {
            let real = std::fs::canonicalize(&dest).map_err(io_err)?;
            if !scope.holds(&real) {
                return Err(outside());
            }
            real
        }
        _ => dest,
    };
    if real.is_dir() {
        return Err(bad("a folder, not a file"));
    }
    Ok(real)
}

/// The first of `name`, `name (2)`, ... that nothing in `dir` is called (a dangling symlink too
/// takes its name).
fn free_name(dir: &Path, name: &str) -> Result<String, WireErr> {
    (1..=COPIES_MAX)
        .map(|n| numbered(name, n))
        .find(|c| {
            matches!(std::fs::symlink_metadata(dir.join(c)),
                Err(e) if e.kind() == ErrorKind::NotFound)
        })
        .ok_or_else(|| bad("too many files by that name"))
}

/// `name` as the `n`th of its name: `a.txt`, `a (2).txt`, ...; a name whose only dot leads it
/// (`.env`) has no extension.
fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        _ if n == 1 => name.to_owned(),
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// `Conflict` unless the file at `dest` hashes to `was` (no `was`, no check).
fn check_was(dest: &Path, was: Option<[u8; 32]>) -> Result<(), WireErr> {
    let Some(was) = was else { return Ok(()) };
    let changed = || conflict("changed on disk");
    let mut file = match File::open(dest) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(changed()),
        r => r.map_err(io_err)?,
    };
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(io_err)?;
    if hash.finalize().as_slice() == was {
        Ok(())
    } else {
        Err(changed())
    }
}

/// Puts the whole `.part` in place at `dest`.
fn land(part: &Path, dest: &Path, replace: bool, was: Option<[u8; 32]>) -> Result<(), WireErr> {
    if replace {
        if let Err(e) = check_was(dest, was) {
            let _ = std::fs::remove_file(part);
            return Err(e);
        }
        if let Ok(old) = std::fs::metadata(dest) {
            std::fs::set_permissions(part, old.permissions()).map_err(io_err)?;
        }
        return std::fs::rename(part, dest).map_err(io_err);
    }
    match std::fs::hard_link(part, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(part);
            return Err(conflict("a file by that name appeared meanwhile"));
        }
        // A file system without hard links: a rename, the name having been free a moment ago.
        Err(_) => return std::fs::rename(part, dest).map_err(io_err),
    }
    std::fs::remove_file(part).map_err(io_err)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    fn scope_in(home: &Path, cwd: &Path) -> Scope {
        Scope {
            home: [resolved(home), home.to_path_buf()],
            uploads: [home.join("nowhere"), home.join("nowhere")],
            cwd: cwd.to_path_buf(),
        }
    }

    #[test]
    fn paths_resolve_under_home_only() {
        let dir = tempfile::tempdir().unwrap();
        let (home, outside) = (dir.path().join("home"), dir.path().join("outside"));
        std::fs::create_dir_all(home.join("proj/sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(home.join("proj/a.txt"), "a").unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        symlink(&outside, home.join("proj/out")).unwrap();
        symlink(outside.join("secret"), home.join("proj/leak")).unwrap();
        symlink(home.join("proj/sub"), home.join("proj/in")).unwrap();
        let s = scope_in(&home, &home.join("proj"));
        let code = |p: &str| s.resolve(p).unwrap_err().code;
        let real = std::fs::canonicalize(&home).unwrap();

        assert_eq!(s.resolve("").unwrap(), real.join("proj"));
        assert_eq!(s.resolve(".").unwrap(), real.join("proj"));
        assert_eq!(s.resolve("a.txt").unwrap(), real.join("proj/a.txt"));
        assert_eq!(s.resolve("in").unwrap(), real.join("proj/sub"));
        let absolute = home.join("proj/a.txt").display().to_string();
        assert_eq!(s.resolve(&absolute).unwrap(), real.join("proj/a.txt"));

        for bad in ["..", "../x", "sub/../a.txt", "a/..", "/home/../etc"] {
            assert_eq!(code(bad), ErrCode::Bad, "{bad}");
        }
        // Symlinks out of home, to a folder or to a file, and absolute paths out of it, are
        // refused whether or not there is anything at the end.
        for out in [
            "out",
            "out/secret",
            "leak",
            "/etc/hostname",
            "/no/such/place",
        ] {
            assert_eq!(code(out), ErrCode::Bad, "{out}");
        }
        assert_eq!(code("missing"), ErrCode::NotFound);
        assert_eq!(code("a\0b"), ErrCode::Bad);
        assert_eq!(code(&"x/".repeat(PATH_MAX)), ErrCode::Bad);
    }

    #[test]
    fn the_sessions_uploads_folder_is_readable_even_outside_home() {
        let dir = tempfile::tempdir().unwrap();
        let (home, state) = (dir.path().join("home"), dir.path().join("state"));
        let (mine, theirs) = (state.join("uploads/mine"), state.join("uploads/theirs"));
        for d in [&home, &mine, &theirs] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(mine.join("a.png"), "a").unwrap();
        std::fs::write(theirs.join("b.png"), "b").unwrap();
        std::fs::write(state.join("worker.token"), "secret").unwrap();
        symlink(state.join("worker.token"), mine.join("link")).unwrap();
        let s = Scope {
            uploads: [resolved(&mine), mine.clone()],
            ..scope_in(&home, &home)
        };
        let ok = s.resolve(&mine.join("a.png").display().to_string());
        assert_eq!(
            ok.unwrap(),
            std::fs::canonicalize(&mine).unwrap().join("a.png")
        );
        // Not another session's uploads, not the state folder around them, and not what a link
        // in the folder leads to.
        for no in [
            theirs.join("b.png"),
            state.join("worker.token"),
            mine.join("link"),
            mine.join("../theirs/b.png"),
        ] {
            let r = s.resolve(&no.display().to_string());
            assert_eq!(r.unwrap_err().code, ErrCode::Bad, "{}", no.display());
        }
    }

    #[test]
    fn a_folder_is_not_made_through_a_symlink_out_of_home() {
        let dir = tempfile::tempdir().unwrap();
        let (home, outside) = (dir.path().join("home"), dir.path().join("outside"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join("out")).unwrap();
        symlink(outside.join("gone"), home.join("dangling")).unwrap();
        let s = scope_in(&home, &home);
        for path in ["out/x", "out/x/y", "dangling/x"] {
            assert_eq!(s.make(path).unwrap_err().code, ErrCode::Bad, "{path}");
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        s.make("a/b").unwrap();
        assert!(home.join("a/b").is_dir());
    }

    #[test]
    fn a_neighbour_of_home_is_not_home() {
        let dir = tempfile::tempdir().unwrap();
        let (home, twin) = (dir.path().join("home"), dir.path().join("home2"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&twin).unwrap();
        std::fs::write(twin.join("f"), "f").unwrap();
        let s = scope_in(&home, &home);
        assert_eq!(
            s.resolve(&twin.join("f").display().to_string())
                .unwrap_err()
                .code,
            ErrCode::Bad
        );
    }

    #[test]
    fn a_listing_has_folders_first_then_files_each_by_name_with_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["zed", "alpha", ".git"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join("b.txt"), "12345").unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::write(root.join(".env"), "x").unwrap();
        symlink(root.join("alpha"), root.join("link-to-folder")).unwrap();
        symlink(root.join("gone"), root.join("dangling")).unwrap();
        let got = list_dir(root).unwrap();
        let shown: Vec<_> = got
            .iter()
            .map(|e| (e.name.as_str(), e.dir, e.len))
            .collect();
        assert_eq!(
            shown,
            [
                (".git", true, 0),
                ("alpha", true, 0),
                ("link-to-folder", true, 0),
                ("zed", true, 0),
                (".env", false, 1),
                ("a.txt", false, 0),
                ("b.txt", false, 5),
                ("dangling", false, 0),
            ]
        );
        assert_eq!(
            list_dir(&root.join("b.txt")).unwrap_err().code,
            ErrCode::Bad
        );
    }

    #[test]
    fn a_read_takes_plain_files_under_the_cap_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(read_file(&f).unwrap(), b"hello");
        assert_eq!(read_file(dir.path()).unwrap_err().code, ErrCode::Bad);
        let big = std::fs::File::create(dir.path().join("big")).unwrap();
        big.set_len(READ_MAX).unwrap();
        assert_eq!(
            read_file(&dir.path().join("big")).unwrap().len() as u64,
            READ_MAX
        );
        big.set_len(READ_MAX + 1).unwrap();
        assert_eq!(
            read_file(&dir.path().join("big")).unwrap_err().code,
            ErrCode::Bad
        );
    }

    fn piece(path: &str, offset: u64, total: u64, data: &[u8]) -> Piece {
        Piece {
            path: path.into(),
            offset,
            total,
            data: data.to_vec(),
            replace: false,
            was: None,
        }
    }

    fn replacing(path: &str, data: &[u8], was: Option<&[u8]>) -> Piece {
        Piece {
            replace: true,
            was: was.map(|b| Sha256::digest(b).into()),
            ..piece(path, 0, data.len() as u64, data)
        }
    }

    /// The path, bytes so far and doneness of a landed piece.
    fn landed(s: &Scope, p: Piece) -> (String, u64, bool) {
        match write_piece(s, p).unwrap() {
            Resp::FsWritten {
                path,
                written,
                done,
            } => (path, written, done),
            other => panic!("not FsWritten: {other:?}"),
        }
    }

    fn code(s: &Scope, p: Piece) -> ErrCode {
        write_piece(s, p).unwrap_err().code
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn pieces_in_order_make_the_file_and_only_the_last_puts_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("proj/sub")).unwrap();
        let s = scope_in(home, &home.join("proj"));
        let f = home.join("proj/f.bin");

        let first = landed(&s, piece("f.bin", 0, 7, b"abc"));
        assert_eq!(first, ("f.bin".into(), 3, false));
        assert!(!f.exists());
        assert_eq!(std::fs::read(home.join("proj/f.bin.part")).unwrap(), b"abc");
        assert_eq!(
            landed(&s, piece("f.bin", 3, 7, b"def")),
            ("f.bin".into(), 6, false)
        );
        assert_eq!(
            landed(&s, piece("f.bin", 6, 7, b"g")),
            ("f.bin".into(), 7, true)
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"abcdefg");
        assert!(!home.join("proj/f.bin.part").exists());
        assert_eq!(mode(&f), 0o600);

        assert_eq!(
            landed(&s, piece("sub/one.txt", 0, 2, b"hi")),
            ("sub/one.txt".into(), 2, true)
        );
        assert_eq!(std::fs::read(home.join("proj/sub/one.txt")).unwrap(), b"hi");
        let absolute = home.join("proj/sub/abs.txt").display().to_string();
        assert!(landed(&s, piece(&absolute, 0, 1, b"x")).2);

        // An empty file is one empty piece.
        assert_eq!(
            landed(&s, piece("empty", 0, 0, b"")),
            ("empty".into(), 0, true)
        );
        assert_eq!(std::fs::metadata(home.join("proj/empty")).unwrap().len(), 0);
    }

    #[test]
    fn a_taken_name_gets_a_number_and_the_file_there_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("d")).unwrap();
        for f in ["a.txt", ".env", "d/b"] {
            std::fs::write(home.join(f), "old").unwrap();
        }
        let s = scope_in(home, home);

        // Every piece names the original path; the later ones find the numbered file again.
        assert_eq!(
            landed(&s, piece("a.txt", 0, 4, b"ne")),
            ("a (2).txt".into(), 2, false)
        );
        assert_eq!(
            landed(&s, piece("a.txt", 2, 4, b"w!")),
            ("a (2).txt".into(), 4, true)
        );
        assert_eq!(std::fs::read(home.join("a (2).txt")).unwrap(), b"new!");
        assert_eq!(std::fs::read(home.join("a.txt")).unwrap(), b"old");
        assert_eq!(landed(&s, piece("a.txt", 0, 1, b"3")).0, "a (3).txt");
        assert_eq!(landed(&s, piece(".env", 0, 1, b"e")).0, ".env (2)");
        assert_eq!(landed(&s, piece("d/b", 0, 1, b"b")).0, "d/b (2)");
        assert_eq!(numbered("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered("a.tar.gz", 1), "a.tar.gz");
    }

    #[test]
    fn a_replace_lands_only_over_the_file_it_was_read_from() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let f = home.join("run.sh");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, PermissionsExt::from_mode(0o755)).unwrap();
        let s = scope_in(home, home);

        assert_eq!(
            landed(&s, replacing("run.sh", b"new", Some(b"old"))),
            ("run.sh".into(), 3, true)
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
        assert_eq!(mode(&f), 0o755, "a replaced file keeps its mode");

        // Read as "old", but it says "new" now.
        assert_eq!(
            code(&s, replacing("run.sh", b"mine", Some(b"old"))),
            ErrCode::Conflict
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
        assert_eq!(
            code(&s, replacing("gone.txt", b"x", Some(b"old"))),
            ErrCode::Conflict
        );
        assert!(!home.join("gone.txt").exists());
        // No `was`, no check: the file is made or overwritten.
        assert!(landed(&s, replacing("gone.txt", b"x", None)).2);
        assert!(landed(&s, replacing("run.sh", b"forced", None)).2);
        assert_eq!(std::fs::read(&f).unwrap(), b"forced");

        // Edited between the first piece and the last: the edit stays.
        let first = Piece {
            total: 4,
            ..replacing("run.sh", b"ab", Some(b"forced"))
        };
        assert!(!landed(&s, first).2);
        std::fs::write(&f, "edited").unwrap();
        let last = Piece {
            offset: 2,
            total: 4,
            ..replacing("run.sh", b"cd", Some(b"forced"))
        };
        assert_eq!(code(&s, last), ErrCode::Conflict);
        assert_eq!(std::fs::read(&f).unwrap(), b"edited");
        assert!(!home.join("run.sh.part").exists());

        std::fs::create_dir(home.join("folder")).unwrap();
        assert_eq!(code(&s, replacing("folder", b"x", None)), ErrCode::Bad);
    }

    #[test]
    fn a_piece_out_of_order_or_too_big_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let s = scope_in(home, home);
        assert_eq!(code(&s, piece("f", 3, 6, b"def")), ErrCode::Conflict);
        assert!(!landed(&s, piece("f", 0, 6, b"abc")).2);
        assert_eq!(code(&s, piece("f", 4, 6, b"ef")), ErrCode::Conflict);
        assert_eq!(code(&s, piece("f", 2, 6, b"cdef")), ErrCode::Conflict);
        assert_eq!(code(&s, piece("f", 3, 6, b"defg")), ErrCode::Bad);
        assert_eq!(code(&s, piece("f", u64::MAX, 6, b"d")), ErrCode::Bad);
        let big = vec![0; MAX_FS_WRITE + 1];
        assert_eq!(
            code(&s, piece("g", 0, big.len() as u64, &big)),
            ErrCode::Bad
        );
        // The transfer that was going on is still whole.
        assert!(landed(&s, piece("f", 3, 6, b"def")).2);
        assert_eq!(std::fs::read(home.join("f")).unwrap(), b"abcdef");
    }

    #[test]
    fn a_write_goes_only_into_a_folder_in_scope_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (home, outside) = (dir.path().join("home"), dir.path().join("outside"));
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::fs::write(home.join("proj/file"), "f").unwrap();
        symlink(&outside, home.join("proj/out")).unwrap();
        symlink(outside.join("secret"), home.join("proj/leak")).unwrap();
        let s = scope_in(&home, &home.join("proj"));
        let outside_abs = outside.join("new").display().to_string();
        for path in [
            "../x",
            "sub/../x",
            "missing/x",
            "file/x",
            "out/x",
            "",
            ".",
            "sub/",
            "a/.",
            outside_abs.as_str(),
        ] {
            assert_eq!(code(&s, piece(path, 0, 1, b"x")), ErrCode::Bad, "{path}");
        }
        assert_eq!(code(&s, replacing("leak", b"x", None)), ErrCode::Bad);
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"s");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        // Without `replace` a link is a taken name like any other: the new file goes beside it.
        assert_eq!(landed(&s, piece("leak", 0, 1, b"x")).0, "leak (2)");
    }
}
