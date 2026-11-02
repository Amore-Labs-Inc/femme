//! `femme-desktop [--bench SECONDS] [--panes N] [--mode fire,cat] [--rows R] [--size WxH]`;
//! `FEMME_DISPLAY` picks the display. `--bench` needs a build with `--features bench`.

use clap::Parser;
use femme_desktop::app::Config;
use femme_desktop::terminal::SourceKind;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// FEMME desktop client (prototype). Without `--bench` it opens the app window.
#[derive(Parser)]
#[command(name = "femme-desktop", version)]
struct Cli {
    /// Run the frame-time benchmark for SECONDS (after a short warm-up), print percentiles, exit 0.
    #[arg(long, value_name = "SECONDS")]
    bench: Option<u64>,
    /// Terminal panes in the drawer.
    #[arg(long, default_value_t = 4)]
    panes: usize,
    /// Transcript rows (0 leaves the transcript out).
    #[arg(long, default_value_t = 50_000)]
    rows: usize,
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
    femme_desktop::run(Config {
        bench: cli.bench,
        panes: cli.panes,
        rows: cli.rows,
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
