//! A sign-in that needs a browser, ported as is from `board/src/main.rs`: the command prints a link
//! (or a code), you sign in wherever you are, and what the page then shows goes back to it.
//! - claude: a new cswap account. It signs in under its own config dir, so the box's live login
//!   (and every agent on it) stays untouched; `cswap add` then stores it from there.
//! - gh: a GitHub account for an API key. The other way round: gh shows a one-time code, the
//!   account's owner enters it at github.com/login/device, and gh ends when they have. It runs
//!   in its own config dir, so the box's gh login (and the scheduler's pushes) stay as they are.
//!
//! Everything here blocks (sleeps while the command comes up); callers run it in `spawn_blocking`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;

use super::util::{device_code, last_line, login_url, now, tool};
use crate::config::Config;

pub(crate) struct Login {
    child: Child,
    pub(crate) dir: PathBuf,
    /// Unix seconds.
    pub(crate) started: u64,
}

impl Login {
    pub(crate) fn start(cfg: &Config, kind: &str, dir: PathBuf) -> Result<Login, String> {
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| e.to_string())?;
        let log = fs::File::create(dir.join("out")).map_err(|e| e.to_string())?;
        let mut cmd = tool(cfg, kind);
        match kind {
            // BROWSER: nothing to open here, the link goes to the board.
            "claude" => cmd
                .args(["auth", "login"])
                .env("CLAUDE_CONFIG_DIR", &dir)
                .env("BROWSER", "true"),
            "gh" => cmd
                .args([
                    "auth",
                    "login",
                    "--hostname",
                    "github.com",
                    "--git-protocol",
                    "https",
                    "--web",
                    "--skip-ssh-key",
                ])
                .env("GH_CONFIG_DIR", dir.join("gh")),
            other => return Err(format!("no sign-in for {other}")),
        };
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("{kind}: {e}"))?;
        let mut login = Login {
            child,
            dir,
            started: now(),
        };
        for _ in 0..75 {
            if login.url().is_some() || login.code().is_some() {
                return Ok(login);
            }
            if login.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(last_line(&login.output())
            .unwrap_or("no sign-in link came up")
            .into())
    }

    fn output(&self) -> String {
        fs::read_to_string(self.dir.join("out")).unwrap_or_default()
    }

    pub(crate) fn url(&self) -> Option<String> {
        login_url(&self.output())
    }

    /// gh's one-time code, to be entered at github.com/login/device.
    pub(crate) fn code(&self) -> Option<String> {
        device_code(&self.output())
    }

    /// Whether gh has ended, and how: Ok once the code was entered, Err with gh's last line.
    pub(crate) fn done(&mut self) -> Option<Result<(), String>> {
        let status = self.child.try_wait().ok().flatten()?;
        Some(if status.success() {
            Ok(())
        } else {
            Err(last_line(&self.output())
                .unwrap_or("GitHub sign-in failed")
                .to_string())
        })
    }

    /// The token gh ended up with, from its own config dir.
    pub(crate) fn token(&self, cfg: &Config) -> Option<String> {
        let out = tool(cfg, "gh")
            .args(["auth", "token"])
            .env("GH_CONFIG_DIR", self.dir.join("gh"))
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|t| !t.is_empty())
    }

    /// Hands over the code the sign-in page showed and waits for the command to finish.
    pub(crate) fn finish(&mut self, code: &str) -> Result<(), String> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or("the login has ended; start again")?;
        writeln!(stdin, "{code}").map_err(|e| e.to_string())?;
        for _ in 0..150 {
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                if status.success() {
                    return Ok(());
                }
                // The error lands on the prompt's line, as the prompt ends without a newline.
                let out = self.output();
                let why = last_line(&out).unwrap_or("login failed");
                let prompts = [
                    "Paste code here if prompted > ",
                    "Enter the authorization code displayed in your browser: ",
                ];
                return Err(prompts
                    .iter()
                    .fold(why, |w, p| w.trim_start_matches(p))
                    .into());
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err("the login did not finish; start again".into())
    }
}

/// Ends a sign-in on a blocking thread and waits for that: its `Drop` kills and waits for a process
/// and removes a directory, too slow for an async worker, and the next attempt of the same kind
/// reuses the directory, so it must not start before this one is gone. Never call it with a lock held.
pub(crate) async fn discard(login: Option<Login>) {
    if let Some(login) = login {
        let _ = tokio::task::spawn_blocking(move || drop(login)).await;
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
