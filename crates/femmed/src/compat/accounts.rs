//! `/api/accounts`: Claude's accounts on the box (cswap's) with the usage of each, and who the
//! box's AWS CLI runs as. Switch the box to another Claude account, or sign in: start a login and
//! open its link, then hand over the code the sign-in page shows.
//!
//! cswap and aws can take a few seconds (network), so they never run inside a page load: a task
//! refreshes [`App::accounts`] every 30 seconds and the routes read that.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use serde_json::{json, Value};

use super::login::{discard, Login};
use super::util::{last_line, lock, now, sign_in_code, text, tool};
use super::{blocking, err, ok, App, Req, LINK_SECS};
use crate::config::Config;

/// Runs cswap without a shell. `config` points it at another Claude config dir (a new login).
pub(super) fn cswap(cfg: &Config, args: &[&str], config: Option<&Path>) -> Result<String, String> {
    let mut cmd = tool(cfg, "cswap");
    cmd.args(args).env("NO_COLOR", "1").stdin(Stdio::null());
    if let Some(dir) = config {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let out = cmd.output().map_err(|e| format!("cswap: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    // With --json, errors come on stdout as {"error": {"message": ...}}.
    let json_error = serde_json::from_str::<Value>(&stdout)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from));
    Err(json_error.unwrap_or_else(|| {
        format!("{stdout}{}", String::from_utf8_lossy(&out.stderr))
            .trim()
            .to_string()
    }))
}

pub(super) fn cswap_list(cfg: &Config) -> Value {
    cswap(cfg, &["list", "--json"], None)
        .and_then(|o| serde_json::from_str(&o).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| json!({"error": e}))
}

/// The accounts window: cswap's list, and who the AWS CLI runs as (the instance role).
pub(super) fn account_status(cfg: &Config) -> Value {
    let aws = tool(cfg, "aws")
        .args([
            "sts",
            "get-caller-identity",
            "--output",
            "json",
            "--cli-connect-timeout",
            "5",
            "--cli-read-timeout",
            "5",
        ])
        .env("AWS_PAGER", "")
        .stdin(Stdio::null())
        .output();
    let aws = match aws {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_default(),
        Ok(o) => {
            json!({"error": last_line(&String::from_utf8_lossy(&o.stderr)).unwrap_or("no AWS access")})
        }
        Err(e) => json!({"error": format!("aws: {e}")}),
    };
    json!({"cswap": cswap_list(cfg), "aws": aws})
}

/// The cached status plus the sign-in links of the logins in progress.
pub(super) fn accounts_json(app: &App) -> Value {
    let mut v = lock(&app.accounts).clone();
    v["login"] = lock(&app.logins)
        .iter()
        .filter_map(|(k, l)| Some((k.clone(), Value::from(l.url()?))))
        .collect();
    v
}

/// Reads cswap and aws now and stores the result.
pub(super) async fn refresh(app: &Arc<App>) {
    let cfg = app.cfg().clone();
    if let Ok(status) = blocking(move || account_status(&cfg)).await {
        *lock(&app.accounts) = status;
    }
}

/// Keeps the status fresh every 30 seconds until the router is dropped. No runtime, no task.
pub(super) fn spawn_refresh(app: &Arc<App>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(app);
    rt.spawn(async move {
        loop {
            let Some(app) = weak.upgrade() else { return };
            refresh(&app).await;
            drop(app);
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

pub(super) async fn list(app: &Arc<App>) -> Response {
    // Their codes would have expired.
    let expired: Vec<Login> = {
        let mut logins = lock(&app.logins);
        let stale: Vec<String> = logins
            .iter()
            .filter(|(_, l)| now() > l.started + LINK_SECS)
            .map(|(k, _)| k.clone())
            .collect();
        stale.iter().filter_map(|k| logins.remove(k)).collect()
    };
    for l in expired {
        discard(Some(l)).await;
    }
    ok(accounts_json(app))
}

pub(super) async fn switch(app: &Arc<App>, req: &mut Req) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let n = text(&input, "account").to_string();
    if n.is_empty() || n.len() > 3 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return err(400, "account must be a number");
    }
    let cfg = app.cfg().clone();
    let switched =
        blocking(move || cswap(&cfg, &["switch", &n, "--json"], None).map(|_| cswap_list(&cfg)))
            .await;
    match switched {
        Ok(Ok(list)) => {
            lock(&app.accounts)["cswap"] = list;
            ok(accounts_json(app))
        }
        Ok(Err(e)) => err(502, &e),
        Err(r) => r,
    }
}

/// Starts a sign-in (`kind` is `claude`): its link shows in the accounts window.
pub(super) async fn login(app: &Arc<App>, kind: &str) -> Response {
    let old = lock(&app.logins).remove(kind); // ends an earlier attempt first
    discard(old).await;
    let (cfg, dir, k) = (
        app.cfg().clone(),
        app.cfg().state_path(&format!("{kind}-login")),
        kind.to_string(),
    );
    match blocking(move || Login::start(&cfg, &k, dir)).await {
        Ok(Ok(login)) => {
            lock(&app.logins).insert(kind.to_string(), login);
            ok(accounts_json(app))
        }
        Ok(Err(e)) => err(502, &e),
        Err(r) => r,
    }
}

/// Hands the code the sign-in page showed to the waiting login, then stores the account with `cswap add`.
pub(super) async fn code(app: &Arc<App>, req: &mut Req, kind: &str) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(code) = sign_in_code(text(&input, "code")) else {
        return err(400, "paste the code exactly as the sign-in page shows it");
    };
    let waiting = lock(&app.logins).remove(kind); // consumed on the blocking thread below
    let Some(mut login) = waiting else {
        return err(409, "that sign-in ended; start again");
    };
    let cfg = app.cfg().clone();
    let done = blocking(move || {
        let mut done = login.finish(&code);
        if done.is_ok() {
            done = cswap(&cfg, &["add"], Some(&login.dir)).map(drop);
        }
        drop(login);
        done
    })
    .await;
    refresh(app).await;
    match done {
        Ok(Ok(())) => ok(accounts_json(app)),
        Ok(Err(e)) => err(502, &e),
        Err(r) => r,
    }
}

pub(super) async fn cancel(app: &Arc<App>, kind: &str) -> Response {
    let old = lock(&app.logins).remove(kind);
    discard(old).await;
    ok(accounts_json(app))
}
