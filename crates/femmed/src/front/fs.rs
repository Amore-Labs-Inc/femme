//! `FsList` and `FsRead`: the desktop's file panel looks at a session's folder through the daemon.
//!
//! A path is relative to the session's `cwd` (an absolute one is taken as written). Whatever it
//! names is resolved with every symlink followed and must then lie under `$HOME`, or in the
//! session's own uploads folder (where `Prompt` attachments are saved, which may be outside home
//! when `FEMME_STATE` is). A `..` anywhere in the path is refused before anything is looked at,
//! and an absolute path outside home is refused before the disk is asked whether it exists, so
//! the answer is no oracle for the rest of the box. All the I/O runs on the blocking pool.

use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use femme_proto::codec::MAX_FRAME;
use femme_proto::rpc::{Err as WireErr, ErrCode, FsEntry, Resp};
use femme_proto::Sid;

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

    /// The real path `path` names, if it is one the desktop may see.
    fn resolve(&self, path: &str) -> Result<PathBuf, WireErr> {
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
        let outside = || bad("that path is outside the home folder");
        let joined = if asked.is_absolute() {
            if !self.holds(asked) {
                return Err(outside());
            }
            asked.to_path_buf()
        } else {
            self.cwd.join(asked)
        };
        // Every symlink on the way is followed here: what is left is where the bytes are.
        let real = std::fs::canonicalize(joined).map_err(io_err)?;
        if self.holds(&real) {
            Ok(real)
        } else {
            Err(outside())
        }
    }
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

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
}
