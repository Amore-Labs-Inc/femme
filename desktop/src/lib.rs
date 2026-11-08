//! FEMME desktop client: a gpui app that shows a daemon's sessions (transcript and composer, or a
//! terminal, with files and terminals in a right-hand panel and a drawing board that sends a diagram
//! to Claude), plus a frame-time benchmark on synthetic panes. See `desktop/README.md` and
//! `docs/desktop-ui.md`.

#![warn(missing_docs)]

pub mod accounts;
pub mod app;
pub mod bench;
pub mod board;
pub mod composer;
pub mod connect;
pub mod display;
pub mod files;
pub mod frame;
pub mod lightbox;
pub mod menu;
pub mod motion;
pub mod net;
pub mod projects;
pub mod rng;
pub mod session;
pub mod sidebar;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod ui;
pub mod video;
pub mod viewer;

use anyhow::Context as _;

/// Starts the app (or the benchmark when `cfg.bench` is set, in a `bench` build) and blocks until the
/// window closes.
///
/// A two-worker tokio runtime runs the benchmark's synthetic terminals, and the daemon's client when
/// `cfg.connect` names one, off the UI thread. Without a daemon the window shows its frame with
/// "no daemon configured" and nothing in it.
pub fn run(cfg: app::Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.bench.is_none() || cfg!(feature = "bench"),
        "--bench needs a build with the `bench` feature: cargo build --release --features bench"
    );
    anyhow::ensure!(
        cfg.bench.is_none() || cfg.connect.is_none(),
        "--bench measures the synthetic panes: leave --daemon and --ssh out"
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("femme-side")
        // The client needs sockets (QUIC) and child processes (ssh) as well as timers.
        .enable_all()
        .build()
        .context("start the side tokio runtime")?;
    let handle = rt.handle().clone();
    // The client dials from here on, while the window opens.
    let started = cfg
        .connect
        .as_ref()
        .map(|c| {
            let _in_rt = handle.enter();
            net::start(c)
        })
        .transpose()?;
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        theme::apply(cx);
        app::bind_keys(cx);
        app::open(cfg, handle, started, cx);
    });
    drop(rt);
    Ok(())
}
