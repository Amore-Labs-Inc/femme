//! `femme-holder`: the per-session process behind `femmed` (see the crate README).

mod agent;
mod conn;
mod pane;
mod proc;
mod ring;
mod server;
mod shared;

use std::path::PathBuf;

use clap::Parser;
use femme_proto::Sid;

/// One process per session: owns the session's PTY panes (with their terminal emulator) and its
/// headless agent's pipes, so `femmed` can restart without the agents noticing.
#[derive(Parser, Debug)]
#[command(version)]
pub struct Args {
    /// Session id (ULID).
    #[arg(long)]
    pub sid: Sid,
    /// Directory holding `s/<sid>.sock` [default: $XDG_RUNTIME_DIR/femme].
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
    /// Directory holding `spool/` and `snap/` [default: $XDG_STATE_HOME/femme or ~/.local/state/femme].
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// Do not talk to systemd (no READY=1, no watchdog): for running outside a service.
    #[arg(long)]
    pub no_sd_notify: bool,
    /// Every Nth frame carries a Timing record (0 = never).
    #[arg(long, default_value_t = 100)]
    pub timing_every: u64,
    /// Snapshot / idle-check interval in milliseconds.
    #[arg(long, default_value_t = 30_000, hide = true)]
    pub tick_ms: u64,
    /// Exit after this many seconds with nothing alive and nobody connected.
    #[arg(long, default_value_t = 600, hide = true)]
    pub idle_exit_secs: u64,
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // A log line that cannot be written (stderr is a file on a full disk) must not panic the task
    // that logged it: by default tracing reports the failure with `eprintln!`, which panics, and
    // that would end the spool pump in exactly the situation it is built to ride out.
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).log_internal_errors(false);
    // journald stamps its own times.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        b.without_time().init();
    } else {
        b.init();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    init_logging();
    // The emulator is not required to be `Send`, so every task runs on this thread's LocalSet.
    // ponytail: one thread for every pane of the session; a flood on one pane delays the others by
    // one 8 KiB read at a time. Shard panes over threads if that ever shows up in the latency numbers.
    let local = tokio::task::LocalSet::new();
    let code = match local.run_until(server::run(args)).await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    };
    // Straight out: do not wait for blocking-pool threads or detached tasks.
    std::process::exit(code);
}
