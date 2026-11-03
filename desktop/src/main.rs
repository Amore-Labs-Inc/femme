//! `femme-desktop [--daemon ADDR:PORT --fingerprint HEX64] [--ssh HOST] [--identity DIR] [--show-spki]`
//! shows a daemon's sessions; without them it runs on synthetic panes:
//! `femme-desktop [--bench SECONDS] [--panes N] [--mode fire,cat] [--size WxH]`.
//! `FEMME_DISPLAY` picks the display. `--bench` needs a build with `--features bench`.

use std::path::PathBuf;

use clap::Parser;
use femme_desktop::app::Config;
use femme_desktop::connect::{self, Flags};
use femme_desktop::terminal::SourceKind;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// FEMME desktop client (prototype). Without `--bench` it opens the app window. `--daemon`,
/// `--fingerprint` and `--ssh` also come from FEMME_DAEMON, FEMME_FINGERPRINT and FEMME_SSH in the
/// environment or in ~/.config/femme/desktop.env (the flags win); with a daemon or an ssh host the
/// window shows its sessions and terminals, without one it runs on synthetic panes.
#[derive(Parser)]
#[command(name = "femme-desktop", version)]
struct Cli {
    /// Run the frame-time benchmark for SECONDS (after a short warm-up), print percentiles, exit 0.
    #[arg(long, value_name = "SECONDS")]
    bench: Option<u64>,
    /// Terminal panes in the drawer.
    #[arg(long, default_value_t = 4)]
    panes: usize,
    /// Synthetic terminal frame rate.
    #[arg(long, default_value_t = 60)]
    hz: u32,
    /// What the panes show, comma separated and repeated over the panes: `fire` (every cell changes
    /// every frame), `shell` (one scrolled line per frame) or `cat` (a screenful of new text per frame).
    #[arg(long, default_value = "fire", value_delimiter = ',', value_parser = parse_mode)]
    mode: Vec<SourceKind>,
    /// The drawer takes the whole window (no columns, no transcript): tall panes.
    #[arg(long)]
    full_height: bool,
    /// Judge the benchmark's p99 frame work against MS milliseconds, whatever the display's refresh.
    #[arg(long, value_name = "MS", value_parser = parse_target_ms)]
    target_ms: Option<f64>,
    /// Seed of the synthetic sources.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Requested window size in logical pixels, WIDTHxHEIGHT (a compositor may override it).
    #[arg(long, default_value = "1440x900", value_parser = parse_size, value_name = "WxH")]
    size: (f32, f32),
    /// The daemon's QUIC address (needs --fingerprint).
    #[arg(long, value_name = "ADDR:PORT")]
    daemon: Option<String>,
    /// SHA-256 of the daemon's certificate, 64 hex digits (`femmed enroll` prints it).
    #[arg(long, value_name = "HEX64")]
    fingerprint: Option<String>,
    /// ssh alias of the daemon's box: the fallback link, and the host the enroll line names.
    #[arg(long, value_name = "HOST")]
    ssh: Option<String>,
    /// Where this device's certificate and key live (created on first use, mode 0700).
    #[arg(long, value_name = "DIR")]
    identity: Option<PathBuf>,
    /// Print this device's SPKI hash and the `femmed enroll` line that enrolls it, then exit.
    #[arg(long)]
    show_spki: bool,
}

fn parse_size(s: &str) -> Result<(f32, f32), String> {
    let bad = || format!("`{s}` is not WIDTHxHEIGHT (for example 1100x700)");
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    match (w.parse::<f32>(), h.parse::<f32>()) {
        (Ok(w), Ok(h)) if w >= 320. && h >= 240. => Ok((w, h)),
        _ => Err(bad()),
    }
}

fn parse_mode(s: &str) -> Result<SourceKind, String> {
    SourceKind::parse(s).ok_or_else(|| format!("unknown mode `{s}` (fire, shell, cat)"))
}

fn parse_target_ms(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(ms) if ms.is_finite() && ms > 0.0 => Ok(ms),
        _ => Err(format!("`{s}` is not a positive number of milliseconds")),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let flags = Flags {
        daemon: cli.daemon,
        fingerprint: cli.fingerprint,
        ssh: cli.ssh,
    };
    if cli.show_spki {
        println!("{}", connect::show(flags, cli.identity)?);
        return Ok(());
    }
    // A benchmark measures the synthetic panes: a daemon that only the environment or the env file
    // names is not its business (`run` refuses `--bench` with a `--daemon` or `--ssh` flag).
    let connect = if cli.bench.is_some() && flags.daemon.is_none() && flags.ssh.is_none() {
        None
    } else {
        connect::load(flags, cli.identity)?
    };
    femme_desktop::run(Config {
        connect,
        bench: cli.bench,
        panes: cli.panes,
        hz: cli.hz,
        modes: cli.mode,
        full_height: cli.full_height,
        target_ms: cli.target_ms,
        seed: cli.seed,
        window: cli.size,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_size, parse_target_ms, Cli};
    use clap::Parser as _;
    use femme_desktop::terminal::SourceKind::{Cat, Fire, Shell};

    #[test]
    fn mode_list_repeats_over_the_panes() {
        let cli = Cli::try_parse_from(["femme-desktop", "--mode", "fire,fire,cat", "--panes", "5"])
            .unwrap();
        assert_eq!(cli.mode, [Fire, Fire, Cat]);
        let cfg = femme_desktop::app::Config {
            panes: cli.panes,
            modes: cli.mode,
            ..Default::default()
        };
        assert_eq!(
            [0, 1, 2, 3, 4].map(|i| cfg.mode(i)),
            [Fire, Fire, Cat, Fire, Fire]
        );
        assert_eq!(cfg.mode_label(), "fire,fire,cat,fire,fire");
        assert_eq!(Cli::try_parse_from(["femme-desktop"]).unwrap().mode, [Fire]);
        assert!(Cli::try_parse_from(["femme-desktop", "--mode", "fire,doom"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["femme-desktop", "--mode", "shell"])
                .unwrap()
                .mode,
            [Shell]
        );
    }

    #[test]
    fn daemon_flags_are_optional() {
        let cli = Cli::try_parse_from([
            "femme-desktop",
            "--daemon",
            "127.0.0.1:7788",
            "--fingerprint",
            "ab",
            "--ssh",
            "vps",
            "--identity",
            "/tmp/device",
            "--show-spki",
        ])
        .unwrap();
        assert_eq!(cli.daemon.as_deref(), Some("127.0.0.1:7788"));
        assert_eq!(cli.fingerprint.as_deref(), Some("ab"));
        assert_eq!(cli.ssh.as_deref(), Some("vps"));
        assert_eq!(cli.identity, Some("/tmp/device".into()));
        assert!(cli.show_spki);
        let bare = Cli::try_parse_from(["femme-desktop"]).unwrap();
        assert!(bare.daemon.is_none() && bare.identity.is_none() && !bare.show_spki);
    }

    #[test]
    fn target_ms_is_a_positive_number() {
        assert_eq!(parse_target_ms("10"), Ok(10.0));
        assert_eq!(parse_target_ms("6.5"), Ok(6.5));
        assert!(parse_target_ms("0").is_err());
        assert!(parse_target_ms("-3").is_err());
        assert!(parse_target_ms("fast").is_err());
        assert!(parse_target_ms("NaN").is_err());
    }

    #[test]
    fn parses_window_sizes() {
        assert_eq!(parse_size("1100x700"), Ok((1100., 700.)));
        assert_eq!(parse_size("1440X900"), Ok((1440., 900.)));
        assert!(parse_size("1100").is_err());
        assert!(parse_size("axb").is_err());
        assert!(parse_size("10x10").is_err());
    }
}
