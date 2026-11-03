//! Where the panel gets directory listings and file contents: the daemon, or, as a debug aid,
//! a local directory (see [`Source::new`]).

use std::path::{Path, PathBuf};

use femme_proto::rpc::{FsEntry, Req, Resp};
use femme_proto::Sid;
use gpui::{AsyncApp, Entity};

use super::tree::clean;
use crate::net::Net;

/// The most the daemon reads for one `FsRead`: a frame holds under 16 MiB and the answer's
/// envelope takes a few bytes of that, so the daemon refuses (`Bad`) anything over a frame less 4
/// KiB. The panel does not offer to open a bigger file.
pub const MAX_READ: u64 = (femme_proto::codec::MAX_FRAME - 4096) as u64;

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
async fn ask(net: &Entity<Net>, req: Req, cx: &mut AsyncApp) -> Result<Resp, String> {
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
}
