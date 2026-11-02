//! FEMME desktop client (prototype P4): a gpui app with FEMME-skinned panes, a 50 000-row streaming
//! transcript and a terminal drawer, plus a frame-time benchmark. See `desktop/README.md`.

#![warn(missing_docs)]

pub mod app;
pub mod bench;
pub mod connect;
pub mod display;
pub mod net;
pub mod panels;
pub mod rng;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod ui;

use anyhow::Context as _;

/// Starts the app (or the benchmark when `cfg.bench` is set, in a `bench` build) and blocks until the
/// window closes.
///
/// A two-worker tokio runtime runs the synthetic terminal and transcript sources off the UI thread.
pub fn run(cfg: app::Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.bench.is_none() || cfg!(feature = "bench"),
        "--bench needs a build with the `bench` feature: cargo build --release --features bench"
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("femme-side")
        .enable_time()
        .build()
        .context("start the side tokio runtime")?;
    let handle = rt.handle().clone();
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        theme::apply(cx);
        app::bind_keys(cx);
        app::open(cfg, handle, cx);
    });
    drop(rt);
    Ok(())
}
