//! The FEMME daemon's compatibility layer and issue scheduler, plus the small CLI subcommands.
//!
//! - [`compat`]: the phone board and the WhatsApp/Instinct OpenAPI, route for route what
//!   `board/src/main.rs` served, on axum. Talks to the daemon core only through [`core::Core`].
//! - [`scheduler`]: `server/worker.sh` as a pure `plan()` plus the git/gh/claude commands it ran.
//! - [`cli`]: `femmed proxy` (SSH fallback relay) and `femmed hook` (agent status from PTY mode).
//! - [`config`]: `.env` and the environment, with the scripts' defaults.
//!
//! [`daemon::Daemon`] is the [`core::Core`] over the store ([`cli`]'s `serve` builds it and calls
//! [`serve`]); [`session`] runs sessions in holders (PTY mode); [`headless`] drives agents through
//! their pipes, one actor per session; [`takeover`] is the switch between the two; [`worker`] is
//! the scheduler's loop; [`front`] is the desktop's way in (QUIC, and the unix socket behind `femmed proxy`).

pub mod cli;
pub mod compat;
pub mod config;
pub mod core;
#[cfg(any(test, feature = "test-util"))]
pub mod core_fake;
pub mod daemon;
pub mod front;
mod headless;
pub mod holder_client;
pub mod hub;
pub mod notify;
pub mod scheduler;
pub mod session;
pub mod takeover;
#[cfg(test)]
mod testutil;
pub mod worker;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;

use crate::config::Config;
use crate::core::Core;

/// Serves the compat HTTP layer on `cfg.http_bind` until SIGTERM or SIGINT, then drains in-flight
/// requests and returns.
pub async fn serve(core: Arc<dyn Core>, cfg: Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&cfg.http_bind)
        .await
        .with_context(|| format!("cannot bind {}", cfg.http_bind))?;
    tracing::info!(bind = %cfg.http_bind, url = %cfg.board_url, "compat HTTP layer up");
    sd_ready();
    serve_on(listener, core, shutdown_signal()).await
}

/// Tells systemd the daemon is up (`femmed.service` is `Type=notify`) and keeps its watchdog fed
/// every half `WatchdogSec` from a task on the current runtime. Nothing happens outside systemd.
pub fn sd_ready() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    if let Some(period) = sd_notify::watchdog_enabled().filter(|p| !p.is_zero()) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period / 2);
            loop {
                tick.tick().await;
                let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
            }
        });
    }
}

/// [`serve`] on an already bound listener, stopping when `shutdown` completes (tests bind port 0).
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    core: Arc<dyn Core>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app = compat::router(core).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("http server")
}

/// Completes on SIGTERM (systemd stop) or SIGINT (Ctrl-C).
pub async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
}
