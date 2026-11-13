//! Daemon configuration: the repo's `.env` (the file `server/*.sh` source and systemd's
//! `EnvironmentFile=` reads) under the process environment, with the scripts' defaults.
//!
//! Today the board gets these through its systemd unit (`EnvironmentFile=%h/femme/.env`) and the
//! worker sources `.env` itself; `femmed` does both: [`Config::load`] reads the file, then lets
//! real environment variables win (so a unit's `EnvironmentFile=` and `Environment=` still work).

use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Everything the daemon reads from `.env` / the environment. Defaults are the scripts' own
/// (`worker.sh`: `CAP=60`, `PARALLEL=2`; `.env.example`; the board's `127.0.0.1:8787`).
#[derive(Clone)]
pub struct Config {
    /// `$HOME`. Clones live in `<home>/femme-work`; terminals may only open folders under it.
    pub home: PathBuf,
    /// `$FEMME_STATE`, else `<home>/.local/state/femme`: `worker.token`, `deploy.json`, `uploads/`, `report-<name>.md`.
    pub state_dir: PathBuf,
    /// `HOST`: the ssh alias of this server (default `agentbox`).
    pub host: String,
    /// `BOARD_EMAIL`, comma-separated, trimmed and lowercased: the addresses that may sign in.
    pub board_emails: Vec<String>,
    /// `BOARD_URL` without a trailing slash: magic links and `openapi.json` use it; `https://` makes the cookie `Secure`.
    pub board_url: String,
    /// `BOARD_PUBLIC=1`: Caddy serves the board on the internet (bootstrap's business; kept for `/api/meta`-style readers).
    pub board_public: bool,
    /// `RESEND_API_KEY`: empty means sign-in links are logged instead of emailed.
    pub resend_api_key: String,
    /// `MAIL_FROM`, e.g. `FEMME <hello@example.com>`.
    pub mail_from: String,
    /// `NTFY_TOPIC`: empty means no phone pushes.
    pub ntfy_topic: String,
    /// `NTFY_URL`: the ntfy server the pushes go to; empty means ntfy.sh.
    pub ntfy_url: String,
    /// `PARALLEL` (default 2): issues worked at once.
    pub parallel: usize,
    /// `CAP` (default 60): percent of Claude's 5-hour window at which running agents are paused.
    pub cap: u32,
    /// `ACCOUNT_ROTATION=1`: cswap switches accounts at `CAP` (cswap's own services; kept for `/api/meta` readers).
    pub account_rotation: bool,
    /// `FABLE_CAP` (default 96): Fable weekly percent at which an account is held out of rotation.
    pub fable_cap: u32,
    /// `AUTODEPLOY=1`: the server deploys itself when `main` moves.
    pub autodeploy: bool,
    /// `FEMMED=2`: this daemon is the board (the old board's files are imported, 8787). `1`: it runs beside
    /// the herdr board (default bind 8788, nothing imported); `0`: herdr and the board only.
    pub replaces_board: bool,
    /// `CLAUDE_MCP`: space-separated MCP server names to copy to the server.
    pub claude_mcp: Vec<String>,
    /// `MOSH=1`: a phone terminal over mosh.
    pub mosh: bool,
    /// `QUIC_PORT` (default 7788): UDP port of the desktop transport.
    pub quic_port: u16,
    /// `HTTP_BIND` (default `127.0.0.1:8787`; the board's old `FEMME_BOARD_ADDR` is the fallback): the compat HTTP layer.
    pub http_bind: String,
    /// `FEMME_BOARD_API` (a base URL such as `http://127.0.0.1:8787`, no trailing slash): where the
    /// desktop's `BoardApi` calls go, when that is not where the board is: by default `http://127.0.0.1:8787`
    /// beside the herdr board (`FEMMED=1`) and `http://<http_bind>` when this daemon is the board
    /// (`FEMMED=2`). For a box whose board listens elsewhere, and for tests that stand a stub in for it.
    pub board_api: Option<String>,
    /// `PATH` handed to every tool the daemon spawns (`git`, `gh`, `claude`, `cswap`, `aws`); `None`
    /// inherits the daemon's own. Tests point it at a directory of fake tools.
    pub tool_path: Option<String>,
    /// Where magic links are mailed through; tests point it at a local listener.
    pub resend_url: String,
    /// `$XDG_RUNTIME_DIR`, else `/run/user/<uid>`: holder sockets live under `<runtime>/femme/s/`.
    pub runtime_dir: PathBuf,
    /// `FEMME_HOLDER_BIN`: the `femme-holder` binary; default next to `femmed`, else `~/.local/bin/femme-holder`.
    pub holder_bin: Option<PathBuf>,
    /// `FEMME_BIN`: the `femmed` binary the agents' hooks run; default this executable.
    pub femmed_bin: Option<PathBuf>,
    /// `FEMME_HOLDER_DIRECT=1`: start holders as plain child processes instead of transient
    /// systemd user units (development, tests, a box without a user manager).
    pub holder_direct: bool,
    /// The socket the sessions' own `femmed show` / `femmed open` reach, handed to them as
    /// `FEMME_SOCK`; `None` (`serve`) leaves them on [`sock_path`]'s default. Never read from the
    /// environment: only [`Config::local`] sets it.
    pub child_sock: Option<PathBuf>,
}

const DEFAULT_RESEND: &str = "https://api.resend.com/emails";

impl Config {
    /// Builds a config from key/value pairs (the layered `.env` + environment, or a test's own).
    /// Unparsable numbers fall back to their default, as `${CAP:-60}` does for an empty one.
    pub fn from_vars<I: IntoIterator<Item = (String, String)>>(vars: I) -> Config {
        let vars: BTreeMap<String, String> = vars.into_iter().collect();
        let get = |k: &str| vars.get(k).map(|v| v.trim()).unwrap_or("");
        let flag = |k: &str| get(k) == "1";
        let home = PathBuf::from(match get("HOME") {
            "" => "/",
            h => h,
        });
        let state_dir = match get("FEMME_STATE") {
            "" => home.join(".local/state/femme"),
            s => PathBuf::from(s),
        };
        let replaces_board = get("FEMMED") == "2";
        let http_bind = [get("HTTP_BIND"), get("FEMME_BOARD_ADDR")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or(if replaces_board {
                "127.0.0.1:8787"
            } else {
                "127.0.0.1:8788"
            });
        Config {
            host: if get("HOST").is_empty() {
                "agentbox".into()
            } else {
                get("HOST").into()
            },
            board_emails: get("BOARD_EMAIL")
                .split(',')
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            board_url: get("BOARD_URL").trim_end_matches('/').into(),
            board_public: flag("BOARD_PUBLIC"),
            resend_api_key: get("RESEND_API_KEY").into(),
            mail_from: get("MAIL_FROM").into(),
            ntfy_topic: get("NTFY_TOPIC").into(),
            ntfy_url: get("NTFY_URL").into(),
            parallel: get("PARALLEL").parse().unwrap_or(2),
            cap: get("CAP").parse().unwrap_or(60),
            account_rotation: flag("ACCOUNT_ROTATION"),
            fable_cap: get("FABLE_CAP").parse().unwrap_or(96),
            autodeploy: flag("AUTODEPLOY"),
            replaces_board,
            claude_mcp: get("CLAUDE_MCP")
                .split_whitespace()
                .map(String::from)
                .collect(),
            mosh: flag("MOSH"),
            quic_port: get("QUIC_PORT").parse().unwrap_or(7788),
            http_bind: http_bind.into(),
            board_api: Some(get("FEMME_BOARD_API").trim_end_matches('/'))
                .filter(|u| !u.is_empty())
                .map(String::from),
            tool_path: None,
            resend_url: DEFAULT_RESEND.into(),
            runtime_dir: match get("XDG_RUNTIME_DIR") {
                "" => runtime_dir(),
                d => PathBuf::from(d),
            },
            holder_bin: (!get("FEMME_HOLDER_BIN").is_empty())
                .then(|| PathBuf::from(get("FEMME_HOLDER_BIN"))),
            femmed_bin: (!get("FEMME_BIN").is_empty()).then(|| PathBuf::from(get("FEMME_BIN"))),
            holder_direct: flag("FEMME_HOLDER_DIRECT"),
            child_sock: None,
            home,
            state_dir,
        }
    }

    /// Reads `$FEMME_ENV`, else `<home>/femme/.env` (where `sync.sh` puts the checkout), then lets the
    /// process environment win. A missing file is fine: everything has a default.
    pub fn load() -> Config {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let path = std::env::var_os("FEMME_ENV")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("femme/.env"));
        Config::load_from(&path)
    }

    /// [`Config::load`] with an explicit file.
    pub fn load_from(path: &Path) -> Config {
        let mut vars = std::fs::read_to_string(path)
            .map(|t| parse_env_file(&t))
            .unwrap_or_default();
        // vars_os, not vars: that panics on one non-UTF-8 variable, and a hook must never panic. Such a variable is not ours.
        vars.extend(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
        );
        Config::from_vars(vars)
    }

    /// `<home>/femme-work`: where issue clones (`<repo>-<id>`) and named agents' clones (`<name>`) live.
    pub fn work_root(&self) -> PathBuf {
        self.home.join("femme-work")
    }

    /// A file in the state dir (`worker.token`, `deploy.json`, `report-<name>.md`, `uploads`).
    pub fn state_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }
}

impl fmt::Debug for Config {
    // No derive: the Resend key must never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("home", &self.home)
            .field("state_dir", &self.state_dir)
            .field("host", &self.host)
            .field("board_url", &self.board_url)
            .field("board_emails", &self.board_emails.len())
            .field(
                "resend_api_key",
                &if self.resend_api_key.is_empty() {
                    "unset"
                } else {
                    "set"
                },
            )
            .field("parallel", &self.parallel)
            .field("cap", &self.cap)
            .field("quic_port", &self.quic_port)
            .field("http_bind", &self.http_bind)
            .finish_non_exhaustive()
    }
}

/// Parses the `.env` format: `KEY=value` lines, `#` (or `;`) comment lines, blank lines; a value may
/// be wrapped in `"…"` or `'…'` (as `MAIL_FROM="FEMME <hello@example.com>"`). No trailing comments
/// (systemd's reader has none), no interpolation; a leading `export ` is tolerated.
pub fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        out.insert(k.to_string(), unquote(v.trim()));
    }
    out
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        // Inside double quotes bash (and systemd) honour \" and \\.
        return v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

/// `$XDG_RUNTIME_DIR`, else `/run/user/<uid>` (a non-interactive `ssh host femmed proxy` may not have it set).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(format!(
            "/run/user/{}",
            std::fs::metadata("/proc/self")
                .map(|m| m.uid())
                .unwrap_or(0)
        )),
    }
}

/// The daemon's local socket, `$XDG_RUNTIME_DIR/femme/femmed.sock`: what `femmed proxy`, `open` and
/// `show` connect to (the SSH fallback transport) and what `femmed serve` binds. `$FEMME_SOCK`, when
/// set, wins: `femmed local` gives its sessions its own socket that way.
pub fn sock_path() -> PathBuf {
    match std::env::var_os("FEMME_SOCK") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => runtime_dir().join("femme/femmed.sock"),
    }
}

/// `femmed local`'s default state dir: `$XDG_STATE_HOME/femme-local`, else
/// `<home>/.local/state/femme-local`. Apart from the server daemon's `femme`, so a laptop's local
/// sessions never share a store with anything else.
pub fn local_state_dir(xdg_state_home: Option<&str>, home: &Path) -> PathBuf {
    match xdg_state_home {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("femme-local"),
        _ => home.join(".local/state/femme-local"),
    }
}

/// `femmed local`'s only way in: `<state>/femmed.sock`. The desktop computes the same path.
pub fn local_sock_path(state: &Path) -> PathBuf {
    state.join("femmed.sock")
}

impl Config {
    /// This config for `femmed local` on `state`: its own store there, and nothing that belongs to
    /// the server daemon (no board import, no phone pushes, no board API elsewhere).
    pub fn local(mut self, state: PathBuf) -> Config {
        self.child_sock = Some(local_sock_path(&state));
        self.state_dir = state;
        self.replaces_board = false;
        self.ntfy_topic.clear();
        self.board_api = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_file_format() {
        let text = "# comment\n\nHOST=agentbox\nBOARD_EMAIL=a@x.test, B@x.test\nMAIL_FROM=\"FEMME <hello@example.com>\"\nQ='single # not a comment'\nexport EXPORTED=1\n; also a comment\nEMPTY=\nbad line\n=novalue\nWITH SPACE=1\nESC=\"a \\\"b\\\" c\"\n";
        let m = parse_env_file(text);
        assert_eq!(m["HOST"], "agentbox");
        assert_eq!(m["BOARD_EMAIL"], "a@x.test, B@x.test");
        assert_eq!(m["MAIL_FROM"], "FEMME <hello@example.com>");
        assert_eq!(m["Q"], "single # not a comment");
        assert_eq!(m["EXPORTED"], "1");
        assert_eq!(m["EMPTY"], "");
        assert_eq!(m["ESC"], "a \"b\" c");
        assert!(
            !m.contains_key("bad line") && !m.contains_key("WITH SPACE") && !m.contains_key("")
        );
        assert_eq!(m.len(), 7);
    }

    #[test]
    fn defaults_are_the_scripts() {
        let c = Config::from_vars(vars(&[("HOME", "/home/me")]));
        assert_eq!(
            (c.parallel, c.cap, c.fable_cap, c.quic_port),
            (2, 60, 96, 7788)
        );
        // Beside the herdr board (FEMMED unset or 1) femmed keeps off its port; as the board it takes it.
        assert_eq!(c.http_bind, "127.0.0.1:8788");
        assert!(!c.replaces_board);
        let c = Config::from_vars(vars(&[("FEMMED", "1")]));
        assert_eq!(
            (c.http_bind.as_str(), c.replaces_board),
            ("127.0.0.1:8788", false)
        );
        let c = Config::from_vars(vars(&[("FEMMED", "2")]));
        assert_eq!(
            (c.http_bind.as_str(), c.replaces_board),
            ("127.0.0.1:8787", true)
        );
        let c = Config::from_vars(vars(&[("FEMMED", "2"), ("HTTP_BIND", "127.0.0.1:9000")]));
        assert_eq!(c.http_bind, "127.0.0.1:9000");
        let c = Config::from_vars(vars(&[("HOME", "/home/me")]));
        assert_eq!(c.host, "agentbox");
        assert_eq!(c.state_dir, PathBuf::from("/home/me/.local/state/femme"));
        assert_eq!(c.work_root(), PathBuf::from("/home/me/femme-work"));
        assert!(!c.autodeploy && !c.account_rotation && !c.mosh && !c.board_public);
        assert!(c.board_emails.is_empty() && c.claude_mcp.is_empty() && c.tool_path.is_none());
        assert_eq!(c.board_api, None);
        // An empty value (`PARALLEL=` in .env) is the default, like ${PARALLEL:-2}.
        let c = Config::from_vars(vars(&[("PARALLEL", ""), ("CAP", "x")]));
        assert_eq!((c.parallel, c.cap), (2, 60));
    }

    #[test]
    fn values_are_read_and_cleaned() {
        let c = Config::from_vars(vars(&[
            ("HOME", "/h"),
            ("FEMME_STATE", "/s"),
            ("BOARD_EMAIL", " A@X.test, b@x.test ,,"),
            ("BOARD_URL", "https://femme.example.com/"),
            ("PARALLEL", "4"),
            ("CAP", "80"),
            ("AUTODEPLOY", "1"),
            ("ACCOUNT_ROTATION", "1"),
            ("MOSH", "0"),
            ("CLAUDE_MCP", "context7  posthog"),
            ("QUIC_PORT", "9000"),
            ("FEMME_BOARD_ADDR", "127.0.0.1:9999"),
            ("HOST", "box"),
        ]));
        assert_eq!(c.board_emails, ["a@x.test", "b@x.test"]);
        assert_eq!(c.board_url, "https://femme.example.com");
        assert_eq!((c.parallel, c.cap, c.quic_port), (4, 80, 9000));
        assert!(c.autodeploy && c.account_rotation && !c.mosh);
        assert_eq!(c.claude_mcp, ["context7", "posthog"]);
        assert_eq!(c.state_dir, PathBuf::from("/s"));
        assert_eq!(c.http_bind, "127.0.0.1:9999"); // the board's old variable still works
        assert_eq!(c.host, "box");
        let c = Config::from_vars(vars(&[
            ("FEMME_BOARD_ADDR", "127.0.0.1:9999"),
            ("HTTP_BIND", "0.0.0.0:1"),
        ]));
        assert_eq!(c.http_bind, "0.0.0.0:1"); // HTTP_BIND wins
    }

    #[test]
    fn the_board_api_override_is_a_base_url() {
        let api = |v: &str| Config::from_vars(vars(&[("FEMME_BOARD_API", v)])).board_api;
        assert_eq!(
            api("http://127.0.0.1:9999/"),
            Some("http://127.0.0.1:9999".into())
        );
        assert_eq!(
            api(" http://127.0.0.1:9999 "),
            Some("http://127.0.0.1:9999".into())
        );
        assert_eq!(api(""), None);
        assert_eq!(api("/"), None);
    }

    #[test]
    fn debug_hides_the_mail_key() {
        let c = Config::from_vars(vars(&[("RESEND_API_KEY", "re_secret")]));
        assert!(!format!("{c:?}").contains("re_secret"));
    }

    #[test]
    fn file_then_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "CAP=70\nFEMME_TEST_ONLY_IN_FILE=1\nHOST=fromfile\nPARALLEL=3\n",
        )
        .unwrap();
        let c = Config::load_from(&path);
        assert_eq!((c.cap, c.parallel), (70, 3));
        // A missing file is fine.
        let c = Config::load_from(&dir.path().join("missing"));
        assert_eq!(c.cap, 60);
    }

    #[test]
    fn socket_path_is_under_the_runtime_dir() {
        assert!(sock_path().ends_with("femme/femmed.sock"));
    }

    #[test]
    fn local_state_follows_xdg_then_home() {
        let home = Path::new("/home/me");
        assert_eq!(
            local_state_dir(Some("/x/state"), home),
            PathBuf::from("/x/state/femme-local")
        );
        for unset in [None, Some("")] {
            assert_eq!(
                local_state_dir(unset, home),
                PathBuf::from("/home/me/.local/state/femme-local")
            );
        }
        assert_eq!(
            local_sock_path(Path::new("/s")),
            PathBuf::from("/s/femmed.sock")
        );
    }

    #[test]
    fn local_config_is_its_own_store_and_no_board() {
        let c = Config::from_vars(vars(&[
            ("HOME", "/home/me"),
            ("FEMMED", "2"),
            ("NTFY_TOPIC", "t"),
            ("FEMME_BOARD_API", "http://x"),
        ]))
        .local(PathBuf::from("/s"));
        assert_eq!(c.state_dir, PathBuf::from("/s"));
        assert_eq!(c.child_sock, Some(PathBuf::from("/s/femmed.sock")));
        assert!(!c.replaces_board && c.ntfy_topic.is_empty() && c.board_api.is_none());
    }
}
