//! What the session suites share: a box (a temp dir with a home, a `PATH` of fake tools and a
//! state dir), a daemon started in it around a real `femme-holder`, and the waits.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use femme_proto::state::{Agent, Mode};
use femme_proto::Sid;
use femmed::config::Config;
use femmed::core::{Core, CreateSession, PaneRef, SessionInfo};
use femmed::daemon::Daemon;

pub fn holder_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let femmed = PathBuf::from(env!("CARGO_BIN_EXE_femmed"));
        let dir = femmed.parent().unwrap();
        let bin = dir.join("femme-holder");
        // Always built (a no-op when fresh): the suites spawn this binary, and a stale one once
        // ran a whole audit against an older wire format.
        {
            let st = std::process::Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-q",
                    "-p",
                    "femme-holder",
                    "--bin",
                    "femme-holder",
                    "--target-dir",
                ])
                .arg(dir.parent().unwrap())
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("cargo");
            assert!(st.success(), "building femme-holder");
        }
        assert!(bin.is_file(), "{} missing", bin.display());
        bin
    })
    .clone()
}

/// A `claude` for the suites: headless (`-p`) it speaks the stream-json protocol, otherwise it is
/// a TUI that reports through the hooks. The prompts it understands are listed in the script.
const FAKE_CLAUDE: &str = include_str!("../../../femme-agents/fixtures/claude/fake-claude.py");

/// A `gh` that clones by `git init` and lists the PR in `<box>/pr-url`, when there is one.
pub const FAKE_GH: &str = r#"#!/bin/bash
case "$1 $2" in
  "repo clone") git init -q -b main "$4" && git -C "$4" -c user.name=t -c user.email=t@x commit -q --allow-empty -m init ;;
  "pr list") f="${0%/*}/../pr-url"; [ -f "$f" ] && printf '%s\n' "$(<"$f")" ;;
esac
"#;

pub struct Box_ {
    // Unix socket paths are limited to 108 bytes: /tmp, not the (long) session scratch dir.
    pub dir: tempfile::TempDir,
}

impl Box_ {
    pub fn new() -> Box_ {
        let dir = tempfile::Builder::new()
            .prefix("femmed-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let script = FAKE_CLAUDE.replace("__FEMMED__", env!("CARGO_BIN_EXE_femmed"));
        for (name, body) in [("claude", script.as_str()), ("gh", FAKE_GH)] {
            std::fs::write(bin.join(name), body).unwrap();
            std::fs::set_permissions(
                bin.join(name),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
        }
        for tool in ["git", "sleep"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
        }
        Box_ { dir }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn config(&self, http_bind: String) -> Config {
        let mut cfg = Config::from_vars([("HOME".to_string(), self.home().display().to_string())]);
        cfg.state_dir = self.dir.path().join("state");
        cfg.runtime_dir = self.dir.path().join("rt");
        cfg.http_bind = http_bind;
        cfg.tool_path = Some(self.dir.path().join("bin").display().to_string());
        cfg.holder_direct = true;
        cfg.holder_bin = Some(holder_bin());
        cfg.femmed_bin = Some(PathBuf::from(env!("CARGO_BIN_EXE_femmed")));
        cfg
    }

    /// A daemon serving its hook route on `port` (a free one when `None`); the sender stops it.
    pub async fn start(
        &self,
        port: Option<u16>,
    ) -> (Arc<Daemon>, tokio::sync::oneshot::Sender<()>) {
        self.start_with(port, |_| {}).await
    }

    /// [`Box_::start`] with `tweak` applied to the box's config first.
    pub async fn start_with(
        &self,
        port: Option<u16>,
        tweak: impl FnOnce(&mut Config),
    ) -> (Arc<Daemon>, tokio::sync::oneshot::Sender<()>) {
        // The port of a daemon just stopped frees a moment after its stop signal.
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await {
                Ok(l) => break l,
                Err(e) if port.is_some() && e.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("bind: {e}"),
            }
        };
        let mut cfg = self.config(listener.local_addr().unwrap().to_string());
        tweak(&mut cfg);
        let daemon = Daemon::open(cfg).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let core: Arc<dyn Core> = daemon.clone();
        tokio::spawn(femmed::serve_on(listener, core, async move {
            let _ = stopped.await;
        }));
        (daemon, stop)
    }

    pub fn socket(&self, sid: Sid) -> PathBuf {
        self.dir
            .path()
            .join("rt/femme/s")
            .join(format!("{sid}.sock"))
    }

    /// The pid the fake `claude` last wrote (each launch overwrites it).
    pub fn claude_pid(&self) -> String {
        std::fs::read_to_string(self.home().join("claude.pid"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

/// A test that fails mid-way leaves its holders up (they outlive daemons by design); they would
/// hold the test's stderr open and so hang the pipeline running it.
impl Drop for Box_ {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill")
            .args([
                "-f",
                &format!(
                    "[f]emme-holder --sid .* --runtime-dir {}/rt",
                    self.dir.path().display()
                ),
            ])
            .status();
    }
}

/// The port a daemon of [`Box_::start`] listens on.
pub fn port_of(d: &Daemon) -> u16 {
    d.cfg.http_bind.rsplit(':').next().unwrap().parse().unwrap()
}

/// A Claude in a terminal: its TUI in pane 1.
pub fn claude(home: &Path) -> CreateSession {
    CreateSession {
        cwd: home.display().to_string(),
        agent: Agent::Claude,
        mode: Mode::Tui,
        label: Some("t".into()),
        name: None,
        owner: "board".into(),
    }
}

/// A headless Claude: a driver in the daemon, no terminal.
pub fn headless(home: &Path) -> CreateSession {
    CreateSession {
        mode: Mode::Headless,
        ..claude(home)
    }
}

pub fn pane_of(s: &SessionInfo) -> PaneRef {
    PaneRef {
        machine: s.machine.clone(),
        handle: s.handle.clone(),
        no: 1,
    }
}

pub async fn wait_for(
    d: &Daemon,
    sid: Sid,
    what: &str,
    ok: impl Fn(Option<&SessionInfo>) -> bool,
) -> Option<SessionInfo> {
    let t0 = Instant::now();
    loop {
        let s = d
            .terminals()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.sid == sid);
        if ok(s.as_ref()) {
            return s;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "timeout waiting for {what}; last: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn wait_screen(d: &Daemon, pane: &PaneRef, needle: &str) -> String {
    let t0 = Instant::now();
    loop {
        let s = d.screen(pane, false).await.unwrap().text;
        if s.contains(needle) {
            return s;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "timeout waiting for {needle:?} on screen:\n{s}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
