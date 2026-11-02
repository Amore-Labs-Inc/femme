//! `/api/keys`: keys for other agents (Instinct), made and revoked on the board, shown once. A key
//! may carry a GitHub account (a token pasted, or signed in with gh's device code): its clones
//! push as it. The keys themselves live in the core; the sign-ins in progress live here.

use std::sync::Arc;

use axum::response::Response;
use serde_json::{json, Value};

use super::login::{discard, Login};
use super::util::{
    lock, noreply, now_ms, random_hex, text, tool, valid_name, valid_token, NAME_RULE,
};
use super::{blocking, core_err, err, json_response, ok, App, Req};
use crate::config::Config;
use crate::core::{ApiKey, CoreError, GithubAccount};

/// The keys without their secrets: name, when, the last 4 characters to tell them apart, whose
/// GitHub account their clones push as, and a GitHub sign-in in progress (its code) or how the
/// last one failed.
fn keys_public(app: &App, keys: &[ApiKey]) -> Value {
    let hint = |k: &ApiKey| {
        k.key
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    };
    let account = |k: &ApiKey| match (
        k.github_token.as_deref().unwrap_or(""),
        k.git_name.as_deref().unwrap_or(""),
        k.github_login.as_deref().unwrap_or(""),
    ) {
        ("", ..) => "the box".to_string(),
        (_, "", "") => "its own token".to_string(),
        (_, n, l) => format!("{n} ({l})"),
    };
    let logins = lock(&app.logins);
    let errors = lock(&app.github_errors);
    Value::Array(
        keys.iter()
            .map(|k| {
                let pending =
                    logins.get(&format!("gh:{}", k.name)).and_then(Login::code).map(|code| json!({"code": code, "url": "https://github.com/login/device"}));
                json!({"name": k.name, "created": k.created / 1000, "hint": hint(k), "account": account(k), "github": pending, "error": errors.get(&k.name)})
            })
            .collect(),
    )
}

async fn keys(app: &App) -> Result<Vec<ApiKey>, Response> {
    app.core.api_keys().await.map_err(core_err)
}

/// A GitHub sign-in that ended: its token goes onto the key, with the account's name and the
/// noreply address GitHub gives it for commits; gh's config dir goes when the Login drops. A
/// failed one leaves its reason on the key until the next attempt.
async fn settle_github(app: &Arc<App>) {
    // Take the ended sign-ins out under the lock; asking gh who they are happens outside it.
    let ended: Vec<(String, Login, Result<(), String>)> = {
        let mut logins = lock(&app.logins);
        let ids: Vec<String> = logins
            .keys()
            .filter(|k| k.starts_with("gh:"))
            .cloned()
            .collect();
        let mut ended = Vec::new();
        for id in ids {
            let Some(result) = logins.get_mut(&id).and_then(Login::done) else {
                continue;
            };
            if let Some(login) = logins.remove(&id) {
                ended.push((id, login, result));
            }
        }
        ended
    };
    for (id, login, result) in ended {
        let name = id.trim_start_matches("gh:").to_string();
        let cfg = app.cfg().clone();
        let who = blocking(move || {
            let token = result.and_then(|_| {
                login
                    .token(&cfg)
                    .ok_or_else(|| "gh kept no token".to_string())
            });
            token.and_then(|t| github_identity(&cfg, &t).map(|who| (t, who)))
        })
        .await
        .unwrap_or_else(|_| Err("internal error".into()));
        match who {
            Ok((token, (login, name_, email))) => {
                let account = GithubAccount {
                    token,
                    login,
                    name: name_,
                    email,
                };
                match app.core.api_key_set_github(&name, Some(account)).await {
                    Ok(()) => {
                        lock(&app.github_errors).remove(&name);
                    }
                    // The key was revoked while gh waited: nothing to put the account on.
                    Err(CoreError::NotFound) => {}
                    Err(e) => {
                        lock(&app.github_errors).insert(name, e.to_string());
                    }
                }
            }
            Err(e) => {
                lock(&app.github_errors).insert(name, e);
            }
        }
    }
}

/// Who a GitHub token belongs to: (login, name or login, the noreply address GitHub gives them,
/// which attributes commits to them without exposing an address).
pub(super) fn github_identity(
    cfg: &Config,
    token: &str,
) -> Result<(String, String, String), String> {
    let out = tool(cfg, "gh")
        .args(["api", "user"])
        .env("GH_TOKEN", token)
        .output()
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let login = text(&v, "login").to_string();
    if !out.status.success() || login.is_empty() {
        return Err("GitHub did not accept that token".into());
    }
    let name = v["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&login)
        .to_string();
    Ok((
        login.clone(),
        name,
        noreply(v["id"].as_u64().unwrap_or(0), &login),
    ))
}

pub(super) async fn list(app: &Arc<App>) -> Response {
    settle_github(app).await;
    match keys(app).await {
        Ok(k) => ok(keys_public(app, &k)),
        Err(r) => r,
    }
}

pub(super) async fn create(app: &Arc<App>, req: &mut Req) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = text(&input, "name").trim().to_string();
    if !valid_name(&name) {
        return err(400, NAME_RULE);
    }
    const TAKEN: &str = "a key with that name exists: revoke it first";
    match keys(app).await {
        Ok(k) if k.iter().any(|k| k.name == name) => return err(409, TAKEN),
        Ok(_) => {}
        Err(r) => return r,
    }
    let mut key = ApiKey {
        name: name.clone(),
        key: random_hex(),
        created: now_ms(),
        ..Default::default()
    };
    let token = text(&input, "github_token").trim().to_string();
    if !token.is_empty() {
        if !valid_token(&token) {
            return err(400, "github_token: a GitHub token is letters, digits and _");
        }
        let cfg = app.cfg().clone();
        let t = token.clone();
        match blocking(move || github_identity(&cfg, &t)).await {
            Ok(Ok((login, git_name, git_email))) => {
                key.github_token = Some(token);
                key.github_login = Some(login);
                key.git_name = Some(git_name);
                key.git_email = Some(git_email);
            }
            Ok(Err(e)) => return err(400, &e),
            Err(r) => return r,
        }
    }
    match app.core.api_key_create(key.clone()).await {
        Ok(()) => {}
        Err(CoreError::Conflict(_)) => return err(409, TAKEN),
        Err(e) => return core_err(e),
    }
    match keys(app).await {
        Ok(k) => json_response(
            201,
            json!({"name": name, "key": key.key, "keys": keys_public(app, &k)}),
        ),
        Err(r) => r,
    }
}

pub(super) async fn delete(app: &Arc<App>, name: &str) -> Response {
    let old = lock(&app.logins).remove(&format!("gh:{name}"));
    discard(old).await;
    lock(&app.github_errors).remove(name);
    if let Err(e) = app.core.api_key_delete(name).await {
        return core_err(e);
    }
    match keys(app).await {
        Ok(k) => ok(keys_public(app, &k)),
        Err(r) => r,
    }
}

/// Starts a GitHub sign-in for the key: its one-time code shows in the key's list entry.
pub(super) async fn github_start(app: &Arc<App>, name: &str) -> Response {
    match keys(app).await {
        Ok(k) if k.iter().any(|k| k.name == name) => {}
        Ok(_) => return err(404, "not found"),
        Err(r) => return r,
    }
    lock(&app.github_errors).remove(name);
    let id = format!("gh:{name}");
    let old = lock(&app.logins).remove(&id); // ends an earlier attempt first
    discard(old).await;
    let (cfg, dir) = (
        app.cfg().clone(),
        app.cfg().state_path(&format!("login-{id}")),
    );
    match blocking(move || Login::start(&cfg, "gh", dir)).await {
        Ok(Ok(login)) => {
            lock(&app.logins).insert(id, login);
            match keys(app).await {
                Ok(k) => ok(keys_public(app, &k)),
                Err(r) => r,
            }
        }
        Ok(Err(e)) => err(502, &format!("gh: {e}")),
        Err(r) => r,
    }
}

pub(super) async fn github_cancel(app: &Arc<App>, name: &str) -> Response {
    let old = lock(&app.logins).remove(&format!("gh:{name}"));
    discard(old).await;
    match keys(app).await {
        Ok(k) => ok(keys_public(app, &k)),
        Err(r) => r,
    }
}
