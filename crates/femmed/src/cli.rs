//! `femmed`'s command line: `serve` (the daemon), `proxy` (the SSH fallback transport), `hook`
//! (agent status from PTY mode), and `enroll` / `devices` (which desktops may connect).
//!
//! - `femmed proxy`: `ssh -T host femmed proxy` turns the SSH channel into a byte pipe to the
//!   daemon's unix socket ([`config::sock_path`]); the client speaks the same frames over it as
//!   over QUIC.
//! - `femmed open`: asks the daemon for a headless Claude session in this directory and prints its id.
//!   A bare `claude` typed in a FEMME shell pane runs it (`server/claude-shim.sh`).
//! - `femmed show <file> [caption]`: run by the agent inside a session, puts the file in the
//!   transcript for the user to see (a screenshot of its work, say); the daemon keeps a copy.
//!   A video goes as it is (the desktop plays it); one over a blob's size as an animated GIF.
//! - `femmed hook <agent> <event> [PAYLOAD]`: what a PTY-mode agent runs to report its status
//!   (Claude `--settings` hooks, Codex `-c notify=[...]`, the pi extension). It wraps the agent's
//!   JSON in an envelope ([`hook_envelope`]) and posts it to the compat layer's loopback-only
//!   `/internal/hook` ([`post_hook`]). It always exits 0: a Claude hook that exits 2 blocks the
//!   tool call and feeds its stderr to the model.
//! - `femmed enroll --name <dev> --spki <sha256 hex>` registers a desktop (the SSH login is the root
//!   of trust) and prints what it pins; `femmed devices` lists them and `femmed devices revoke
//!   <id|name>` refuses one from its next connection on ([`crate::front::admin`]).
//!
//! stdout is `proxy`'s data channel: diagnostics go to stderr.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::builder::NonEmptyStringValueParser;
use clap::{Parser, Subcommand};
use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Mode};
use femme_transport::DeviceIdentity;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use ulid::Ulid;

use crate::config::{self, Config};

/// How long `femmed hook` waits for the agent to write its JSON and close stdin.
const STDIN_WAIT: Duration = Duration::from_secs(2);
/// How long `femmed hook` waits for the daemon to accept the connection.
const CONNECT_WAIT: Duration = Duration::from_secs(2);
/// The whole of `femmed hook`, stdin included: a hook must never hang its agent.
const HOOK_BUDGET: Duration = Duration::from_secs(5);
/// How long a hook keeps trying a daemon that refuses connections: a restart (every deploy) is a
/// second or two, and a status lost in that gap would leave the session stale until the next hook.
const RETRY_WINDOW: Duration = Duration::from_secs(3);
/// How long `femmed open` waits for the daemon to create the session (it starts the agent first).
const ANSWER_WAIT: Duration = Duration::from_secs(20);

/// `femmed`'s command line.
#[derive(Debug, Parser)]
#[command(
    name = "femmed",
    version,
    about = "The FEMME daemon",
    propagate_version = true
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// `femmed`'s subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon.
    Serve,
    /// Relay stdin and stdout to the daemon's local socket (the SSH fallback: `ssh -T host femmed proxy`).
    Proxy,
    /// Start a headless Claude session in the daemon, in `--cwd` (default: here), and print its id.
    Open {
        /// The directory to start in.
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        cwd: Option<String>,
    },
    /// Show the user a file (a screenshot, a chart) in the transcript, from inside a session.
    Show {
        /// The file to show.
        file: std::path::PathBuf,
        /// A line to go with it.
        caption: Option<String>,
    },
    /// Report a PTY-mode agent's status to the daemon (Claude hooks, Codex notify, the pi extension).
    Hook {
        /// The agent reporting.
        #[arg(value_parser = ["claude", "codex", "pi"])]
        agent: String,
        /// The event, as the agent's config names it (`Stop`, `PreToolUse`, `agent-turn-complete`, ...).
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        event: String,
        /// The event's JSON when the agent passes it as the last argument (Codex `notify`); stdin is then not read.
        payload: Option<String>,
    },
    /// Let a desktop connect: record its key and print what it pins (`fingerprint=... quic_port=... host=...`).
    Enroll {
        /// What to call the device in `femmed devices`.
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        name: String,
        /// SHA-256 of the device certificate's public key, 64 hex digits (the desktop shows it on first run).
        #[arg(long)]
        spki: String,
    },
    /// List the enrolled devices, or revoke one.
    Devices {
        /// What to do with them (default: list).
        #[command(subcommand)]
        cmd: Option<DevicesCmd>,
    },
}

/// What `femmed devices` does.
#[derive(Debug, Subcommand)]
pub enum DevicesCmd {
    /// One line per device (the default).
    List,
    /// Refuse a device's connections from now on (open ones end on their own).
    Revoke {
        /// The device's id, or its name when no other device has it.
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        device: String,
    },
}

/// `femmed serve` until the daemon's wiring exists: this crate cannot build a real
/// [`crate::core::Core`]. The wiring replaces this with building its `Core` and calling
/// [`crate::serve`]. Exits 2.
/// `femmed serve`: logs to the journal (under systemd) or stderr, opens the store, imports the old
/// board's files once when `FEMMED=2`, and serves until SIGTERM. Exits 1 with the reason otherwise.
async fn serve_daemon() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    match tracing_journald::layer() {
        Ok(journald) if std::env::var_os("JOURNAL_STREAM").is_some() => {
            registry.with(journald).init()
        }
        _ => registry
            .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
            .init(),
    }
    let cfg = Config::load();
    let daemon = crate::daemon::Daemon::open(cfg.clone())?;
    if cfg.replaces_board {
        let report = daemon.import_board()?;
        tracing::info!(?report, "old board files");
    }
    daemon.reconcile().await;
    // Before the worker starts anything: a port that is taken must stop the daemon, not half-start it.
    // Held to the end of `serve`; dropping it closes the listeners and removes the socket.
    let _front = crate::front::serve_front(daemon.clone()).await?;
    tokio::spawn(crate::worker::run(std::sync::Arc::downgrade(&daemon)));
    tokio::spawn(crate::headless::seed_commands(std::sync::Arc::downgrade(
        &daemon,
    )));
    tokio::spawn(crate::headless::herdr::run(std::sync::Arc::downgrade(
        &daemon,
    )));
    tokio::spawn(crate::daemon::Daemon::retain_daily(
        std::sync::Arc::downgrade(&daemon),
    ));
    crate::serve(daemon, cfg).await
}

/// The SSH fallback transport: connects to the daemon's socket `sock`, copies `input` to it and
/// it to `output`, byte for byte. EOF on `input` only closes our sending side (the daemon's answer
/// to `printf req | ssh host femmed proxy` still arrives); the copy ends when the daemon hangs up
/// or on an I/O error, then `output` is flushed. A connect error names the socket and keeps its
/// kind (`NotFound`: no daemon; `ConnectionRefused`: a stale socket file).
pub async fn proxy<R, W>(sock: &Path, mut input: R, mut output: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut stream = UnixStream::connect(sock).await.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot connect to {}: {e}", sock.display()),
        )
    })?;
    let (mut rd, mut wr) = stream.split();
    let ended = {
        let mut from_daemon = std::pin::pin!(tokio::io::copy(&mut rd, &mut output));
        let to_daemon = std::pin::pin!(async {
            tokio::io::copy(&mut input, &mut wr).await?;
            wr.shutdown().await // our EOF reaches the daemon; its answer still comes
        });
        tokio::select! {
            r = &mut from_daemon => r.map(drop),
            r = to_daemon => match r {
                Ok(()) => (&mut from_daemon).await.map(drop),
                Err(e) => Err(e),
            },
        }
    };
    let flushed = output.flush().await;
    ended.and(flushed)
}

/// The JSON `femmed hook` posts: the agent's own JSON object (its `session_id`,
/// `hook_event_name`, `transcript_path`, ... untouched) with `agent`, `event` and `sid` set,
/// overwriting the agent's fields of those names. A payload that is not a JSON object becomes
/// `{"text": <payload, trailing whitespace trimmed>}`; an empty one gives just our three fields.
pub fn hook_envelope(agent: &str, event: &str, sid: &str, payload: &str) -> Value {
    let mut base = match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => map,
        _ if payload.trim().is_empty() => Map::new(),
        _ => Map::from_iter([("text".to_owned(), Value::from(payload.trim_end()))]),
    };
    for (key, value) in [("agent", agent), ("event", event), ("sid", sid)] {
        base.insert(key.to_owned(), Value::from(value));
    }
    Value::Object(base)
}

/// POSTs `body` to `http://<addr>/internal/hook` and returns the response's status code. Only
/// `Host`, `Content-Type`, `Content-Length` and `Connection: close` are sent: the route is
/// loopback-only and refuses anything that looks proxied (`X-Forwarded-For`, `Forwarded`,
/// `X-Real-IP`). Connecting gives up after 2 s; the caller bounds the rest.
pub async fn post_hook(addr: &str, body: &Value) -> io::Result<u16> {
    // ponytail: hand-written HTTP/1.1 for one loopback POST per tool call (no client setup to pay
    // for); use hyper's client if this ever needs TLS, redirects or a response body.
    let connect = tokio::time::timeout(CONNECT_WAIT, TcpStream::connect(addr));
    let mut stream = connect
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
    let body = body.to_string();
    let request = format!(
        "POST /internal/hook HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    // The status line is all we need; the cap keeps a peer that is not HTTP from feeding us forever.
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(256)
        .read_until(b'\n', &mut line)
        .await?;
    std::str::from_utf8(&line)
        .ok()
        .and_then(|l| l.strip_prefix("HTTP/1."))
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|code| code.trim().parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not an HTTP response: {:?}", String::from_utf8_lossy(&line)),
            )
        })
}

/// `femmed hook` without the process around it. Without a `sid` (unset or empty `FEMME_SID`: a
/// human's shell, the old herdr setup) it does nothing. Otherwise the payload is `payload`, else
/// `stdin` read until EOF for at most 2 s (then whatever was read so far), and the envelope goes to
/// the daemon at `cfg.http_bind`, all within 5 s, trying again for [`RETRY_WINDOW`] while the
/// daemon refuses connections (a restart). Never fails: a failure is one
/// `femmed hook: ...` line on stderr; success prints nothing.
pub async fn run_hook(
    agent: &str,
    event: &str,
    payload: Option<&str>,
    cfg: &Config,
    sid: Option<&str>,
    mut stdin: impl AsyncRead + Unpin,
) {
    let Some(sid) = sid.filter(|s| !s.is_empty()) else {
        return;
    };
    let sent = tokio::time::timeout(HOOK_BUDGET, async {
        let text = match payload {
            Some(p) => p.to_owned(),
            None => {
                let mut buf = Vec::new();
                // On timeout `buf` keeps what was read: an agent that never closes stdin still reports.
                let _ = tokio::time::timeout(STDIN_WAIT, stdin.read_to_end(&mut buf)).await;
                String::from_utf8_lossy(&buf).into_owned()
            }
        };
        let (body, t0) = (
            hook_envelope(agent, event, sid, &text),
            tokio::time::Instant::now(),
        );
        loop {
            match post_hook(&cfg.http_bind, &body).await {
                Err(e)
                    if e.kind() == io::ErrorKind::ConnectionRefused
                        && t0.elapsed() < RETRY_WINDOW =>
                {
                    tokio::time::sleep(Duration::from_millis(200)).await
                }
                sent => break sent,
            }
        }
    })
    .await;
    let url = format!("http://{}/internal/hook", cfg.http_bind);
    match sent {
        Ok(Ok(status)) if (200..300).contains(&status) => {}
        Ok(Ok(status)) => eprintln!("femmed hook: {url} answered {status}"),
        Ok(Err(e)) => eprintln!("femmed hook: cannot post to {url}: {e}"),
        Err(_) => eprintln!("femmed hook: no answer from {url} within {HOOK_BUDGET:?}"),
    }
}

/// Runs a parsed command line on the real stdin, stdout and environment (`FEMME_SID`,
/// [`Config::load`], [`config::sock_path`]). tokio reads stdin on a blocking thread that cannot be
/// cancelled, so the caller must not wait for the runtime's blocking threads once this returns
/// (`main` uses `Runtime::shutdown_background`), or `proxy` and `hook` hang on a stdin that is
/// still open.
pub async fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Serve => match serve_daemon().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("femmed serve: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Proxy => match proxy(
            &config::sock_path(),
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("femmed proxy: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Open { cwd } => finish("open", open(cwd).await),
        Command::Show { file, caption } => finish("show", show(&file, caption).await),
        Command::Hook {
            agent,
            event,
            payload,
        } => {
            let sid = std::env::var("FEMME_SID").ok();
            // Agents pipe their JSON. A terminal on stdin is a person (or a misconfigured hook):
            // reading it would eat their keystrokes for 2 s.
            let stdin: Box<dyn AsyncRead + Unpin> = if io::stdin().is_terminal() {
                Box::new(tokio::io::empty())
            } else {
                Box::new(tokio::io::stdin())
            };
            run_hook(
                &agent,
                &event,
                payload.as_deref(),
                &Config::load(),
                sid.as_deref(),
                stdin,
            )
            .await;
            ExitCode::SUCCESS
        }
        Command::Enroll { name, spki } => finish(
            "enroll",
            crate::front::admin::enroll(&Config::load(), &name, &spki),
        ),
        Command::Devices {
            cmd: None | Some(DevicesCmd::List),
        } => finish("devices", crate::front::admin::devices(&Config::load())),
        Command::Devices {
            cmd: Some(DevicesCmd::Revoke { device }),
        } => finish(
            "devices",
            crate::front::admin::revoke(&Config::load(), &device),
        ),
    }
}

/// `femmed open`: asks the daemon, over the socket `femmed proxy` connects to, for a headless Claude
/// session in `cwd` (default: here) and returns its id. The desktop lists the session like any other.
async fn open(cwd: Option<String>) -> anyhow::Result<String> {
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    // The client would wait out ANSWER_WAIT for a daemon that is not there; the shim falls back at once.
    let sock = config::sock_path();
    anyhow::ensure!(sock.exists(), "no daemon at {}", sock.display());
    let mut proxy = tokio::process::Command::new(std::env::current_exe()?);
    proxy.arg("proxy");
    let mut cfg =
        femme_client::Config::new(DeviceIdentity::generate("femmed open"), Ulid::generate());
    cfg.ssh_command = Some(proxy);
    let client = femme_client::Client::start(cfg);
    let req = Req::SessionCreate {
        cwd,
        agent: Agent::Claude,
        mode: Mode::Headless,
        label: None,
        name: None,
        prompt: None,
        model: None,
    };
    match client.request_timeout(req, ANSWER_WAIT).await? {
        Resp::Session(sid) => Ok(sid.to_string()),
        other => anyhow::bail!("unexpected answer {other:?}"),
    }
}

/// `femmed show <file> [caption]`, run by the agent inside a session (`FEMME_SID` names it): uploads
/// the file as a blob and asks the daemon to show it, over the socket like `open`. The daemon
/// keeps a copy beside the session's uploads, so the file may go afterwards.
async fn show(file: &Path, caption: Option<String>) -> anyhow::Result<String> {
    let env_sid: Option<femme_proto::Sid> =
        std::env::var("FEMME_SID").ok().and_then(|s| s.parse().ok());
    // a video goes as it is, for the desktop's player; one too big for a blob goes as an
    // animated GIF instead (gpui animates a GIF from memory, not a WebP)
    let too_big =
        std::fs::metadata(file).is_ok_and(|m| m.len() as usize > femme_proto::rpc::MAX_BLOB_PUT);
    let gif = if is_video(file) && too_big {
        Some(to_gif(file).await?)
    } else {
        None
    };
    let file = gif.as_deref().unwrap_or(file);
    let read = std::fs::read(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()));
    if let Some(dir) = gif.as_deref().and_then(Path::parent) {
        let _ = std::fs::remove_dir_all(dir);
    }
    let data = read?;
    anyhow::ensure!(
        data.len() <= femme_proto::rpc::MAX_BLOB_PUT,
        "{} is {} bytes, the most is {}",
        file.display(),
        data.len(),
        femme_proto::rpc::MAX_BLOB_PUT
    );
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let sock = config::sock_path();
    anyhow::ensure!(sock.exists(), "no daemon at {}", sock.display());
    let mut proxy = tokio::process::Command::new(std::env::current_exe()?);
    proxy.arg("proxy");
    let mut cfg =
        femme_client::Config::new(DeviceIdentity::generate("femmed show"), Ulid::generate());
    cfg.ssh_command = Some(proxy);
    let client = femme_client::Client::start(cfg);
    // The chat is the one whose Claude this process runs under; a `FEMME_SID` is only for a
    // process with no Claude above it (2026-11-09: an agent that guessed the id from the newest
    // uploads folder showed its pictures in another chat).
    let own = own_conversation(&home_dir()?);
    let sid = match &own {
        Some(conv) => match client
            .request_timeout(
                Req::SessionOf {
                    agent_sid: conv.clone(),
                },
                ANSWER_WAIT,
            )
            .await
        {
            Ok(Resp::Session(sid)) => Some(sid),
            _ => None,
        },
        None => None,
    };
    let sid = match (sid, env_sid) {
        (Some(sid), Some(env)) if sid != env => {
            eprintln!("femmed show: FEMME_SID={env} ignored, this process runs in chat {sid}");
            sid
        }
        (Some(sid), _) => sid,
        (None, Some(env)) => env,
        (None, None) => anyhow::bail!(
            "not inside a FEMME session (no Claude above this process in a FEMME chat, no FEMME_SID)"
        ),
    };
    let put = Req::BlobPut {
        name: name.clone(),
        data,
    };
    let blob = match client.request_timeout(put, ANSWER_WAIT).await? {
        Resp::BlobHash(hash) => hash,
        other => anyhow::bail!("unexpected answer {other:?}"),
    };
    let req = Req::Show {
        sid,
        name: name.clone(),
        blob,
        caption: caption.unwrap_or_default(),
    };
    match client.request_timeout(req, ANSWER_WAIT).await? {
        Resp::Ok => Ok(format!("shown {name}")),
        other => anyhow::bail!("unexpected answer {other:?}"),
    }
}

fn home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(Into::into)
        .ok_or_else(|| anyhow::anyhow!("no HOME"))
}

/// This process and its ancestors, nearest first, from `/proc/<pid>/stat`.
fn ancestors() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::process::id();
    while pid > 1 && out.len() < 64 {
        out.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        // "pid (comm) state ppid ...": the comm may hold spaces, so cut after its ')'
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            break;
        };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|p| p.parse().ok()) else {
            break;
        };
        pid = ppid;
    }
    out
}

/// The conversation of the nearest Claude Code above this process: Claude writes
/// `~/.claude/sessions/<pid>.json` with its `sessionId` (the mirror reads the same files).
fn own_conversation(home: &Path) -> Option<String> {
    let dir = home.join(".claude/sessions");
    ancestors().into_iter().find_map(|pid| {
        let bytes = std::fs::read(dir.join(format!("{pid}.json"))).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        v["sessionId"].as_str().map(str::to_owned)
    })
}

/// A video `femmed show` turns into an animated GIF, by its extension.
fn is_video(file: &Path) -> bool {
    file.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v"
        )
    })
}

/// The most frames a shown video keeps: ~180 MB decoded in the desktop at 640x360.
const FRAMES: f64 = 200.;

/// `file` as a silent, looping animated GIF named `<stem>.gif` in a temporary folder, small
/// enough to go up as one blob: each try is smaller until one fits. The desktop holds every
/// frame decoded (640x360 RGBA is ~0.9 MB), so a clip is at most [`FRAMES`] frames: a longer one
/// plays at a lower rate. The caller removes its folder; a failed try removes it here.
// ponytail: the first minute only, no sound; a real player when someone needs audio or length
async fn to_gif(file: &Path) -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("femmed-show-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let made = gif_in(file, &dir).await;
    if made.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    made
}

async fn gif_in(file: &Path, dir: &Path) -> anyhow::Result<std::path::PathBuf> {
    let stem = file.file_stem().unwrap_or_default().to_string_lossy();
    let out = dir.join(format!("{stem}.gif"));
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("a video needs ffmpeg: {e}"))?;
    let seconds: f64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .unwrap_or(60.);
    let fps = (FRAMES / seconds.clamp(1., 60.)).clamp(2., 15.);
    for width in [640, 480, 360] {
        let ran = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-t", "60", "-i"])
            .arg(file)
            .args([
                "-vf",
                &format!(
                    "fps={fps:.2},scale='min({width},iw)':-2:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle"
                ),
                "-an",
                "-loop",
                "0",
            ])
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("a video needs ffmpeg: {e}"))?;
        anyhow::ensure!(
            ran.status.success(),
            "ffmpeg could not convert {}: {}",
            file.display(),
            String::from_utf8_lossy(&ran.stderr).trim()
        );
        if std::fs::metadata(&out)?.len() as usize <= femme_proto::rpc::MAX_BLOB_PUT {
            return Ok(out);
        }
    }
    anyhow::bail!(
        "{} is too long to show even small; cut it shorter",
        file.display()
    )
}

/// Prints an admin command's line, or why it failed.
fn finish(what: &str, out: anyhow::Result<String>) -> ExitCode {
    match out {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("femmed {what}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use clap::CommandFactory;
    use serde_json::json;
    use tokio::io::{duplex, ReadBuf};
    use tokio::net::{TcpListener, UnixListener};
    use tokio::time::{timeout, Instant};

    use super::*;

    const OK: &str = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";

    fn cfg(addr: &str) -> Config {
        Config::from_vars([("HTTP_BIND".to_owned(), addr.to_owned())])
    }

    async fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        (listener, addr)
    }

    /// Accepts one connection, reads one whole request (head, then `Content-Length` bytes of
    /// body), writes `answer` verbatim and hangs up. Returns (head, body).
    async fn serve_one(listener: TcpListener, answer: &'static str) -> (String, Vec<u8>) {
        let (conn, _) = listener.accept().await.unwrap();
        let mut conn = BufReader::new(conn);
        let mut head = String::new();
        while !head.ends_with("\r\n\r\n") {
            assert_ne!(
                conn.read_line(&mut head).await.unwrap(),
                0,
                "EOF inside the head: {head:?}"
            );
        }
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("Content-Length")
            .parse()
            .unwrap();
        let mut body = vec![0; len];
        conn.read_exact(&mut body).await.unwrap();
        conn.get_mut().write_all(answer.as_bytes()).await.unwrap();
        (head, body)
    }

    /// A stdin that must never be read.
    struct Untouchable;

    impl AsyncRead for Untouchable {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            panic!("stdin must not be read")
        }
    }

    // ---- command line ----------------------------------------------------------------------------

    #[test]
    fn command_line() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "femmed",
            "hook",
            "codex",
            "agent-turn-complete",
            r#"{"type":"agent-turn-complete"}"#,
        ])
        .unwrap();
        let Command::Hook {
            agent,
            event,
            payload,
        } = cli.command
        else {
            panic!("{cli:?}")
        };
        assert_eq!(
            (agent.as_str(), event.as_str(), payload.as_deref()),
            (
                "codex",
                "agent-turn-complete",
                Some(r#"{"type":"agent-turn-complete"}"#)
            )
        );
        let cli = Cli::try_parse_from(["femmed", "hook", "claude", "Stop"]).unwrap();
        assert!(matches!(cli.command, Command::Hook { payload: None, .. }));
        assert!(matches!(
            Cli::try_parse_from(["femmed", "proxy"]).unwrap().command,
            Command::Proxy
        ));
        assert!(matches!(
            Cli::try_parse_from(["femmed", "serve"]).unwrap().command,
            Command::Serve
        ));
        let spki = "ab".repeat(32);
        let Command::Enroll { name, spki: got } =
            Cli::try_parse_from(["femmed", "enroll", "--name", "laptop", "--spki", &spki])
                .unwrap()
                .command
        else {
            panic!("enroll")
        };
        assert_eq!((name.as_str(), got.as_str()), ("laptop", spki.as_str()));
        assert!(matches!(
            Cli::try_parse_from(["femmed", "devices"]).unwrap().command,
            Command::Devices { cmd: None }
        ));
        assert!(matches!(
            Cli::try_parse_from(["femmed", "devices", "list"])
                .unwrap()
                .command,
            Command::Devices {
                cmd: Some(DevicesCmd::List)
            }
        ));
        let Command::Devices {
            cmd: Some(DevicesCmd::Revoke { device }),
        } = Cli::try_parse_from(["femmed", "devices", "revoke", "laptop"])
            .unwrap()
            .command
        else {
            panic!("revoke")
        };
        assert_eq!(device, "laptop");
        for bad in [
            &["femmed", "hook", "nosuchagent", "x"][..],
            &["femmed", "hook", "pi", ""],
            &["femmed", "hook", "claude"],
            &["femmed", "enroll", "--name", "x"],
            &["femmed", "enroll", "--name", "", "--spki", "ab"],
            &["femmed", "devices", "revoke"],
            &["femmed"],
        ] {
            assert!(Cli::try_parse_from(bad).is_err(), "{bad:?}");
        }
    }

    // ---- hook_envelope ---------------------------------------------------------------------------

    #[test]
    fn envelope_keeps_the_agents_object_and_ours_win() {
        let payload = r#"{"session_id":"abc","hook_event_name":"Stop","transcript_path":"/t.jsonl","agent":"spoofed","event":"x","sid":"other"}"#;
        assert_eq!(
            hook_envelope("claude", "Stop", "s1", payload),
            json!({"session_id": "abc", "hook_event_name": "Stop", "transcript_path": "/t.jsonl", "agent": "claude", "event": "Stop", "sid": "s1"})
        );
    }

    #[test]
    fn envelope_wraps_anything_else_as_text() {
        for (payload, text) in [
            ("[1,2]\n", "[1,2]"),
            ("\"x\"", "\"x\""),
            ("7", "7"),
            ("turn done \r\n", "turn done"),
            ("{\"cut\":", "{\"cut\":"),
        ] {
            assert_eq!(
                hook_envelope("codex", "e", "s", payload),
                json!({"text": text, "agent": "codex", "event": "e", "sid": "s"}),
                "{payload:?}"
            );
        }
    }

    #[test]
    fn envelope_of_nothing_is_just_ours() {
        for payload in ["", " \n\t"] {
            assert_eq!(
                hook_envelope("pi", "agent_end", "s9", payload),
                json!({"agent": "pi", "event": "agent_end", "sid": "s9"})
            );
        }
    }

    // ---- post_hook -------------------------------------------------------------------------------

    #[tokio::test]
    async fn post_hook_sends_a_bare_request_and_returns_the_status() {
        for (answer, code) in [
            ("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", 200),
            (
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\n\r\nforbidden",
                403,
            ),
        ] {
            let (listener, addr) = listener().await;
            let server = tokio::spawn(serve_one(listener, answer));
            // Non-ASCII, so a character count would be a wrong Content-Length.
            let body = json!({"agent": "claude", "event": "Stop", "sid": "s1", "text": "é ü 😀"});
            assert_eq!(post_hook(&addr, &body).await.unwrap(), code);
            let (head, got) = server.await.unwrap();
            let content_length = format!("Content-Length: {}", body.to_string().len());
            let host = format!("Host: {addr}");
            assert_eq!(
                head.lines().collect::<Vec<_>>(),
                [
                    "POST /internal/hook HTTP/1.1",
                    &host,
                    "Content-Type: application/json",
                    &content_length,
                    "Connection: close",
                    ""
                ]
            );
            assert_eq!(serde_json::from_slice::<Value>(&got).unwrap(), body);
        }
    }

    #[tokio::test]
    async fn post_hook_refuses_what_is_not_http() {
        for answer in ["SSH-2.0-OpenSSH_9.6\r\n", ""] {
            let (listener, addr) = listener().await;
            tokio::spawn(serve_one(listener, answer));
            let err = post_hook(&addr, &json!({})).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        }
    }

    // ---- run_hook --------------------------------------------------------------------------------

    #[tokio::test]
    async fn hook_without_a_sid_sends_nothing_and_reads_nothing() {
        let (listener, addr) = listener().await;
        for sid in [None, Some("")] {
            run_hook("claude", "Stop", None, &cfg(&addr), sid, Untouchable).await;
            run_hook(
                "codex",
                "agent-turn-complete",
                Some("{}"),
                &cfg(&addr),
                sid,
                Untouchable,
            )
            .await;
        }
        // run_hook has returned: a connection it made would already wait in the backlog.
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "run_hook connected without a sid"
        );
    }

    #[tokio::test]
    async fn hook_payload_argument_is_posted_and_stdin_is_not_read() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(serve_one(listener, OK));
        let payload = r#"{"type":"agent-turn-complete","turn-id":"t1"}"#;
        run_hook(
            "codex",
            "agent-turn-complete",
            Some(payload),
            &cfg(&addr),
            Some("s1"),
            Untouchable,
        )
        .await;
        let (head, body) = server.await.unwrap();
        assert!(
            head.starts_with("POST /internal/hook HTTP/1.1\r\n"),
            "{head:?}"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"type": "agent-turn-complete", "turn-id": "t1", "agent": "codex", "event": "agent-turn-complete", "sid": "s1"})
        );
    }

    #[tokio::test]
    async fn hook_reads_stdin_to_eof() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(serve_one(listener, OK));
        let (mut agent, stdin) = duplex(1024);
        agent
            .write_all(b"{\"session_id\":\"abc\"}\n")
            .await
            .unwrap();
        drop(agent);
        let start = Instant::now();
        run_hook("claude", "Stop", None, &cfg(&addr), Some("s1"), stdin).await;
        assert!(
            start.elapsed() < STDIN_WAIT,
            "waited for the stdin timeout although stdin was closed: {:?}",
            start.elapsed()
        );
        let (_, body) = server.await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"session_id": "abc", "agent": "claude", "event": "Stop", "sid": "s1"})
        );
    }

    #[tokio::test]
    async fn hook_sends_what_it_read_when_stdin_never_closes() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(serve_one(listener, OK));
        let (mut agent, stdin) = duplex(1024);
        agent.write_all(br#"{"session_id":"abc"}"#).await.unwrap(); // and keeps stdin open
        let start = Instant::now();
        run_hook("claude", "Stop", None, &cfg(&addr), Some("s1"), stdin).await;
        let took = start.elapsed();
        assert!(
            took >= STDIN_WAIT - Duration::from_millis(50) && took < HOOK_BUDGET,
            "{took:?}"
        );
        let (_, body) = server.await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"session_id": "abc", "agent": "claude", "event": "Stop", "sid": "s1"})
        );
        drop(agent);
    }

    #[tokio::test(start_paused = true)]
    async fn hook_with_the_daemon_down_keeps_trying_for_the_retry_window_then_returns() {
        let dead = listener().await.1; // bound, then dropped: nothing listens there
        let cfg = cfg(&dead);
        let start = Instant::now();
        run_hook("pi", "agent_end", Some("{}"), &cfg, Some("s1"), Untouchable).await;
        let took = start.elapsed();
        assert!(
            took >= RETRY_WINDOW && took < RETRY_WINDOW + Duration::from_secs(1),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn hook_reaches_a_daemon_that_comes_up_during_the_retry_window() {
        let (listener, addr) = listener().await;
        drop(listener); // down now; the hook starts, then the daemon binds the same port
        let cfg = cfg(&addr);
        let hook = tokio::spawn(async move {
            run_hook("claude", "Stop", Some("{}"), &cfg, Some("s1"), Untouchable).await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut head = vec![0; 64];
        let n = conn.read(&mut head).await.unwrap();
        assert!(head[..n].starts_with(b"POST /internal/hook HTTP/1.1\r\n"));
        conn.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(5), hook)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn hook_gives_up_on_a_daemon_that_never_answers() {
        // The kernel completes the handshake and buffers the request; nobody ever answers. The
        // paused clock jumps to the next timer as soon as the runtime idles.
        let (listener, addr) = listener().await;
        let start = Instant::now();
        run_hook(
            "claude",
            "Stop",
            Some("{}"),
            &cfg(&addr),
            Some("s1"),
            Untouchable,
        )
        .await;
        assert!(
            start.elapsed() <= HOOK_BUDGET + Duration::from_millis(10),
            "{:?}",
            start.elapsed()
        );
        drop(listener);
    }

    // ---- proxy -----------------------------------------------------------------------------------

    /// A daemon stand-in on `<dir>/femmed.sock` that echoes every connection back.
    fn echo_daemon(dir: &Path) -> PathBuf {
        let sock = dir.join("femmed.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = conn.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        sock
    }

    #[tokio::test]
    async fn proxy_round_trips_bytes_until_input_ends() {
        let dir = tempfile::tempdir().unwrap();
        let sock = echo_daemon(dir.path());
        let (mut client_in, proxy_in) = duplex(64 * 1024);
        let (proxy_out, mut client_out) = duplex(64 * 1024);
        let relay = tokio::spawn(async move { proxy(&sock, proxy_in, proxy_out).await });
        let mut sent = b"\x00\xff\xfe not utf-8: \xc3\x28 and no newline".to_vec();
        sent.extend((0..1u32 << 20).map(|i| (i % 251) as u8)); // and 1 MiB more
        let writer = {
            let sent = sent.clone();
            tokio::spawn(async move {
                client_in.write_all(&sent).await.unwrap();
                client_in
            })
        };
        let mut got = vec![0; sent.len()];
        client_out.read_exact(&mut got).await.unwrap();
        assert!(got == sent, "the bytes came back changed");
        drop(writer.await.unwrap()); // EOF on the proxy's input
        timeout(Duration::from_secs(5), relay)
            .await
            .expect("proxy must return on input EOF")
            .unwrap()
            .unwrap();
        let mut rest = Vec::new();
        client_out.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn proxy_delivers_the_answer_after_input_eof() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("femmed.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            conn.read_to_end(&mut req).await.unwrap(); // our EOF reached the daemon
            assert_eq!(req, b"req");
            conn.write_all(b"answer").await.unwrap();
        });
        let (mut client_in, proxy_in) = duplex(1024);
        let (proxy_out, mut client_out) = duplex(1024);
        client_in.write_all(b"req").await.unwrap();
        drop(client_in); // `printf req | ssh host femmed proxy`
        timeout(Duration::from_secs(5), proxy(&sock, proxy_in, proxy_out))
            .await
            .expect("proxy must return when the daemon hangs up")
            .unwrap();
        let mut got = Vec::new();
        client_out.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"answer");
    }

    #[tokio::test]
    async fn proxy_returns_when_the_daemon_hangs_up() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("femmed.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            conn.write_all(b"bye\xff").await.unwrap();
        }); // `conn` dropped: the daemon hangs up
        let (_client_in, proxy_in) = duplex(1024); // the client keeps its side open
        let (proxy_out, mut client_out) = duplex(1024);
        timeout(Duration::from_secs(5), proxy(&sock, proxy_in, proxy_out))
            .await
            .expect("proxy must return when the daemon hangs up")
            .unwrap();
        let mut got = Vec::new();
        client_out.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"bye\xff");
    }

    #[tokio::test]
    async fn proxy_without_a_daemon_names_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("femmed.sock");
        let err = proxy(&sock, tokio::io::empty(), tokio::io::sink())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("cannot connect to {}: ", sock.display())),
            "{err}"
        );
        // A stale socket file (the daemon died without unlinking it).
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        let err = proxy(&sock, tokio::io::empty(), tokio::io::sink())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
    }

    #[test]
    fn the_ancestors_start_here_and_climb_to_the_parent() {
        let a = ancestors();
        assert_eq!(a[0], std::process::id());
        assert_eq!(a[1], std::os::unix::process::parent_id());
    }

    #[test]
    fn the_conversation_is_the_nearest_claude_above() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/sessions")).unwrap();
        assert_eq!(own_conversation(dir.path()), None);
        let me = std::process::id();
        let f = dir.path().join(format!(".claude/sessions/{me}.json"));
        std::fs::write(&f, r#"{"pid":1,"sessionId":"conv-1","kind":"interactive"}"#).unwrap();
        assert_eq!(own_conversation(dir.path()).as_deref(), Some("conv-1"));
    }
}
