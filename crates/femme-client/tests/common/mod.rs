//! What the suite shares: a box (a temp dir with a home, a `PATH` of the few tools a shell needs and
//! a state dir), the binaries it spawns, and a daemon opened in it around a real `femme-holder`.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Once, OnceLock};

use femmed::config::Config;
use femmed::daemon::Daemon;

/// `(femmed, femme-holder)` of the target dir this test was built in, built there first (a no-op when
/// fresh): the suite spawns both, and a stale one once ran a whole audit against an older wire.
pub fn bins() -> &'static (PathBuf, PathBuf) {
    static BINS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BINS.get_or_init(|| {
        // <target>/<profile>/deps/<this test>
        let exe = std::env::current_exe().unwrap();
        let profile = exe.parent().unwrap().parent().unwrap();
        let st = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "-q",
                "-p",
                "femmed",
                "-p",
                "femme-holder",
                "--bins",
                "-j",
                "2",
            ])
            .arg("--target-dir")
            .arg(profile.parent().unwrap())
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo");
        assert!(st.success(), "building femmed and femme-holder");
        (profile.join("femmed"), profile.join("femme-holder"))
    })
}

/// `RUST_LOG=femme_client=debug cargo test -- --nocapture` shows what the client decided.
pub fn logs() {
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

pub struct Box_ {
    // Unix socket paths are limited to 108 bytes: /tmp, not the (long) session scratch dir.
    pub dir: tempfile::TempDir,
}

impl Box_ {
    pub fn new() -> Box_ {
        // Shell sessions run `$SHELL`: pin one that asks a new user nothing in an empty home.
        static SHELL: Once = Once::new();
        SHELL.call_once(|| std::env::set_var("SHELL", "/bin/sh"));
        let dir = tempfile::Builder::new()
            .prefix("femme-c-")
            .tempdir_in("/tmp")
            .unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for tool in ["cat", "git", "sleep"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
        }
        Box_ { dir }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn runtime(&self) -> PathBuf {
        self.dir.path().join("rt")
    }

    /// A daemon in the box: its store, its sessions and their holders. No front yet, no HTTP.
    pub fn daemon(&self) -> Arc<Daemon> {
        let mut cfg = Config::from_vars([("HOME".to_string(), self.home().display().to_string())]);
        cfg.state_dir = self.dir.path().join("state");
        cfg.runtime_dir = self.runtime();
        cfg.tool_path = Some(self.dir.path().join("bin").display().to_string());
        cfg.holder_direct = true;
        cfg.holder_bin = Some(bins().1.clone());
        cfg.femmed_bin = Some(bins().0.clone());
        Daemon::open(cfg).unwrap()
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
