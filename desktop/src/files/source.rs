//! Where the panel gets directory listings and file contents: the daemon, or, as a debug aid,
//! a local directory (see [`Source::new`]).

use std::path::{Path, PathBuf};

use femme_proto::rpc::{FsEntry, Req, Resp};
use femme_proto::Sid;
use gpui::{AsyncApp, Entity};

use super::transfer::free_name;
use super::tree::{base, clean, parent};
use super::zip::zip_dir;
use crate::net::Net;

/// The most the daemon reads for one `FsRead`: a frame holds under 16 MiB and the answer's
/// envelope takes a few bytes of that, so the daemon refuses (`Bad`) anything over a frame less 4
/// KiB. The panel does not offer to open a bigger file.
pub const MAX_READ: u64 = (femme_proto::codec::MAX_FRAME - 4096) as u64;

/// One `FsWrite`: `data` at `offset` of the file `path`, `total` bytes in all (see `Req::FsWrite`).
#[derive(Clone, Debug, Default)]
pub struct Piece {
    /// The file, relative to the session's directory. For the pieces after the first, the path
    /// the first one's answer gave (the name may have got a ` (2)`).
    pub path: String,
    /// Where in the file `data` goes: the bytes sent before it.
    pub offset: u64,
    /// The size of the whole file.
    pub total: u64,
    /// At most `MAX_FS_WRITE` bytes.
    pub data: Vec<u8>,
    /// Write over a file that is there instead of taking a free name.
    pub replace: bool,
    /// With `replace`: the sha256 of the file as it was read; the write is refused when it changed.
    pub was: Option<[u8; 32]>,
}

/// The answer to a [`Piece`]: the path written (with the suffix a taken name got), the bytes of
/// it so far, and whether the file is whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The file's path, relative to the session's directory.
    pub path: String,
    /// The bytes of it on the box so far.
    pub written: u64,
    /// It is whole and in place.
    pub done: bool,
}

/// Where requests go.
#[derive(Clone)]
pub enum Source {
    /// No daemon and no fixture: every request fails and says so.
    Offline,
    /// The daemon, through [`Net`]'s client: `Req::FsList` and `Req::FsRead`, paths relative to
    /// the session's directory.
    Daemon(Entity<Net>),
    /// A directory on this machine, listed and read directly: `FEMME_FILES_FIXTURE`.
    Fixture(PathBuf),
}

impl Source {
    /// The fixture when the environment variable `FEMME_FILES_FIXTURE` names a directory, else
    /// the daemon `net` talks to, else nothing.
    ///
    /// The variable is a debug aid for trying the panel without a daemon that serves `FsList` and
    /// `FsRead`: the panel then lists and reads that local directory (as the session's directory,
    /// whatever session is set, even none), with the daemon's rules for the path asked (relative,
    /// no `..`) and for the size read ([`MAX_READ`]). Unlike the daemon it does not look where
    /// symlinks lead.
    pub fn new(net: Option<Entity<Net>>) -> Self {
        match std::env::var_os("FEMME_FILES_FIXTURE").filter(|v| !v.is_empty()) {
            Some(dir) => Source::Fixture(PathBuf::from(dir)),
            None => net.map_or(Source::Offline, Source::Daemon),
        }
    }

    /// Whether the panel has something to ask when no session is set.
    pub fn is_fixture(&self) -> bool {
        matches!(self, Source::Fixture(_))
    }

    /// The entries of the directory `path` (relative to the session's directory, `""` for it).
    pub async fn list(
        &self,
        sid: Option<Sid>,
        path: &str,
        cx: &mut AsyncApp,
    ) -> Result<Vec<FsEntry>, String> {
        match self {
            Source::Offline => Err(offline()),
            Source::Daemon(net) => {
                let sid = sid.ok_or_else(no_session)?;
                let req = Req::FsList {
                    sid,
                    path: path.to_owned(),
                };
                match ask(net, req, cx).await? {
                    Resp::FsEntries(entries) => Ok(entries),
                    _ => Err(unexpected()),
                }
            }
            Source::Fixture(root) => {
                let (root, path) = (root.clone(), path.to_owned());
                cx.background_executor()
                    .spawn(async move { fixture_list(&root, &path) })
                    .await
            }
        }
    }

    /// The bytes of the file `path`.
    pub async fn read(
        &self,
        sid: Option<Sid>,
        path: &str,
        cx: &mut AsyncApp,
    ) -> Result<Vec<u8>, String> {
        match self {
            Source::Offline => Err(offline()),
            Source::Daemon(net) => {
                let sid = sid.ok_or_else(no_session)?;
                let req = Req::FsRead {
                    sid,
                    path: path.to_owned(),
                };
                match ask(net, req, cx).await? {
                    Resp::FsData(data) => Ok(data),
                    _ => Err(unexpected()),
                }
            }
            Source::Fixture(root) => {
                let (root, path) = (root.clone(), path.to_owned());
                cx.background_executor()
                    .spawn(async move { fixture_read(&root, &path) })
                    .await
            }
        }
    }

    /// Sends one piece of a file (`Req::FsWrite`).
    pub async fn write(
        &self,
        sid: Option<Sid>,
        piece: Piece,
        cx: &mut AsyncApp,
    ) -> Result<Written, String> {
        match self {
            Source::Offline => Err(offline()),
            Source::Daemon(net) => {
                let sid = sid.ok_or_else(no_session)?;
                let req = Req::FsWrite {
                    sid,
                    path: piece.path,
                    offset: piece.offset,
                    total: piece.total,
                    data: piece.data,
                    replace: piece.replace,
                    was: piece.was,
                };
                match ask(net, req, cx).await? {
                    Resp::FsWritten {
                        path,
                        written,
                        done,
                    } => Ok(Written {
                        path,
                        written,
                        done,
                    }),
                    _ => Err(unexpected()),
                }
            }
            Source::Fixture(root) => {
                let root = root.clone();
                cx.background_executor()
                    .spawn(async move { fixture_write(&root, piece) })
                    .await
            }
        }
    }

    /// The folder `path` as a zip archive (`Req::FsZip`).
    pub async fn zip(
        &self,
        sid: Option<Sid>,
        path: &str,
        cx: &mut AsyncApp,
    ) -> Result<Vec<u8>, String> {
        match self {
            Source::Offline => Err(offline()),
            Source::Daemon(net) => {
                let sid = sid.ok_or_else(no_session)?;
                let req = Req::FsZip {
                    sid,
                    path: path.to_owned(),
                };
                match ask(net, req, cx).await? {
                    Resp::FsData(data) => Ok(data),
                    _ => Err(unexpected()),
                }
            }
            Source::Fixture(root) => {
                let (root, path) = (root.clone(), path.to_owned());
                cx.background_executor()
                    .spawn(async move { fixture_zip(&root, &path) })
                    .await
            }
        }
    }
}

fn offline() -> String {
    "not connected to a daemon".to_owned()
}

fn no_session() -> String {
    "no session".to_owned()
}

fn unexpected() -> String {
    "the daemon answered with something else than asked".to_owned()
}

/// Sends `req` with [`Net`]'s client and waits for the answer. Not through [`Net::run`]: that one
/// keeps the reason of a failure in a single place for the nav bar, and the answer to any other
/// request that lands before this one is read (the sessions, the board and the transcripts all
/// ask) wipes it, so the panel would say "the request failed" for a refusal the daemon explained.
pub(super) async fn ask(net: &Entity<Net>, req: Req, cx: &mut AsyncApp) -> Result<Resp, String> {
    // Through a weak handle: it answers with an error, not a panic, once the app is gone.
    let (client, rt) = net
        .downgrade()
        .read_with(cx, |n, _| (n.client(), n.rt()))
        .map_err(|_| "the app is closing".to_owned())?;
    match rt.spawn(async move { client.request(req).await }).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(why)) => Err(why.to_string()),
        Err(why) => Err(why.to_string()),
    }
}

/// `rel` under `root`, refusing anything that is not a clean relative path.
fn fixture_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if !clean(rel) {
        return Err(format!("Bad: {rel:?} is not a path under the directory"));
    }
    Ok(if rel.is_empty() {
        root.to_owned()
    } else {
        root.join(rel)
    })
}

/// What the daemon would answer to `FsList`: the entries of `rel` under `root`, hidden ones
/// included, symlinks followed, in no particular order.
pub fn fixture_list(root: &Path, rel: &str) -> Result<Vec<FsEntry>, String> {
    let dir = fixture_path(root, rel)?;
    let read = std::fs::read_dir(&dir).map_err(|e| format!("Bad: {}: {e}", shown(rel)))?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| e.to_string())?;
        // A dangling link has no metadata: listed as an empty file.
        let meta = std::fs::metadata(entry.path()).ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        out.push(FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: is_dir,
            len: if is_dir {
                0
            } else {
                meta.map_or(0, |m| m.len())
            },
        });
    }
    Ok(out)
}

/// What the daemon would answer to `FsRead`: the bytes of the file `rel` under `root`, at most
/// [`MAX_READ`] of them.
pub fn fixture_read(root: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let file = fixture_path(root, rel)?;
    let len = std::fs::metadata(&file)
        .map_err(|e| format!("NotFound: {}: {e}", shown(rel)))?
        .len();
    if len > MAX_READ {
        return Err(format!(
            "Bad: {} is {len} bytes, the daemon reads at most {MAX_READ}",
            shown(rel)
        ));
    }
    std::fs::read(&file).map_err(|e| format!("Bad: {}: {e}", shown(rel)))
}

/// What the daemon would answer to `FsWrite` (see [`Piece`]): the first piece (`offset` 0) picks
/// the name (a free one, unless `replace`) and starts `<name>.part`, the next ones go on at the
/// end of that `.part` (whose length must be `offset`), and the one that reaches `total` renames
/// it into place. Unlike the daemon it does not check `was`.
pub fn fixture_write(root: &Path, piece: Piece) -> Result<Written, String> {
    use std::io::Write as _;
    let rel = piece.path.as_str();
    if rel.is_empty() {
        return Err("Bad: no file named".to_owned());
    }
    fixture_path(root, rel)?;
    let end = piece.offset + piece.data.len() as u64;
    if end > piece.total {
        return Err(format!(
            "Bad: {rel}: {end} bytes is past the total {}",
            piece.total
        ));
    }
    let bad = |e: std::io::Error| format!("Bad: {rel}: {e}");
    let path = if piece.offset == 0 && !piece.replace {
        let dir = parent(rel);
        let name = free_name(base(rel), |n| {
            let p = if dir.is_empty() {
                n.to_owned()
            } else {
                format!("{dir}/{n}")
            };
            root.join(p).exists()
        });
        if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        }
    } else {
        rel.to_owned()
    };
    let fin = root.join(&path);
    let part = root.join(format!("{path}.part"));
    let mut file = if piece.offset == 0 {
        std::fs::File::create(&part).map_err(bad)?
    } else {
        let have = std::fs::metadata(&part)
            .map_err(|e| format!("Bad: {rel}: no upload under way: {e}"))?
            .len();
        if have != piece.offset {
            return Err(format!(
                "Bad: {rel}: a piece at {} after {have} bytes",
                piece.offset
            ));
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .map_err(bad)?
    };
    file.write_all(&piece.data).map_err(bad)?;
    drop(file);
    let done = end == piece.total;
    if done {
        std::fs::rename(&part, &fin).map_err(bad)?;
    }
    Ok(Written {
        path,
        written: end,
        done,
    })
}

/// What the daemon would answer to `FsZip`: the folder `rel` (`""` for the root) as a zip, at
/// most [`MAX_READ`] bytes of it.
pub fn fixture_zip(root: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let dir = fixture_path(root, rel)?;
    if !dir.is_dir() {
        return Err(format!("Bad: {} is not a folder", shown(rel)));
    }
    let top = if rel.is_empty() {
        root.file_name()
            .map_or("files".to_owned(), |n| n.to_string_lossy().into_owned())
    } else {
        base(rel).to_owned()
    };
    zip_dir(&dir, &top, MAX_READ)
}

fn shown(rel: &str) -> &str {
    if rel.is_empty() {
        "."
    } else {
        rel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory with `sub/` (holding `deep.txt`), `a.txt` and `.hidden`.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("femme-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();
        std::fs::write(dir.join("sub/deep.txt"), b"deep").unwrap();
        dir
    }

    #[test]
    fn the_fixture_lists_with_hidden_files_and_sizes() {
        let root = scratch("list");
        let mut top = fixture_list(&root, "").unwrap();
        top.sort_by(|a, b| a.name.cmp(&b.name));
        let line = |e: &FsEntry| (e.name.clone(), e.dir, e.len);
        assert_eq!(
            top.iter().map(line).collect::<Vec<_>>(),
            [
                (".hidden".to_owned(), false, 0),
                ("a.txt".to_owned(), false, 5),
                ("sub".to_owned(), true, 0),
            ]
        );
        let sub = fixture_list(&root, "sub").unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "deep.txt");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_fixture_reads_files_and_refuses_paths_that_escape() {
        let root = scratch("read");
        assert_eq!(fixture_read(&root, "sub/deep.txt").unwrap(), b"deep");
        for bad in [
            "../x",
            "sub/../../x",
            "/etc/passwd",
            "sub//deep.txt",
            "./a.txt",
        ] {
            let e = fixture_read(&root, bad).unwrap_err();
            assert!(e.starts_with("Bad:"), "{bad}: {e}");
            assert!(fixture_list(&root, bad).is_err(), "{bad}");
        }
        assert!(fixture_read(&root, "missing")
            .unwrap_err()
            .starts_with("NotFound:"));
        assert!(fixture_list(&root, "missing").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn piece(path: &str, offset: u64, total: u64, data: &[u8]) -> Piece {
        Piece {
            path: path.to_owned(),
            offset,
            total,
            data: data.to_vec(),
            ..Piece::default()
        }
    }

    #[test]
    fn the_fixture_writes_in_pieces_and_suffixes_a_taken_name() {
        let root = scratch("write");
        // `a.txt` is there: the upload becomes `a (2).txt`, the next pieces name that.
        let w = fixture_write(&root, piece("a.txt", 0, 6, b"abc")).unwrap();
        assert_eq!(
            w,
            Written {
                path: "a (2).txt".into(),
                written: 3,
                done: false
            }
        );
        assert!(root.join("a (2).txt.part").exists());
        assert!(!root.join("a (2).txt").exists());
        let w = fixture_write(&root, piece(&w.path, 3, 6, b"def")).unwrap();
        assert!(w.done);
        assert_eq!(std::fs::read(root.join("a (2).txt")).unwrap(), b"abcdef");
        assert!(!root.join("a (2).txt.part").exists());
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello");

        // A piece out of order, past the total, or escaping is refused.
        fixture_write(&root, piece("sub/n.bin", 0, 4, b"ab")).unwrap();
        assert!(fixture_write(&root, piece("sub/n.bin", 1, 4, b"cd")).is_err());
        assert!(fixture_write(&root, piece("sub/n.bin", 2, 3, b"cd")).is_err());
        assert!(fixture_write(&root, piece("../x", 0, 1, b"x")).is_err());

        // An empty file is whole at once; `replace` writes over.
        assert!(fixture_write(&root, piece("e", 0, 0, b"")).unwrap().done);
        let over = Piece {
            replace: true,
            ..piece("a.txt", 0, 2, b"hi")
        };
        assert_eq!(fixture_write(&root, over).unwrap().path, "a.txt");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hi");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_fixture_zips_a_folder_and_refuses_a_file() {
        let root = scratch("zipf");
        let zip = fixture_zip(&root, "sub").unwrap();
        assert!(String::from_utf8_lossy(&zip).contains("sub/deep.txt"));
        assert!(fixture_zip(&root, "a.txt").unwrap_err().starts_with("Bad:"));
        assert!(fixture_zip(&root, "..").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
