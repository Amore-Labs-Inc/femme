//! The compatibility layer: the phone board and the WhatsApp/Instinct OpenAPI, route for route what
//! `board/src/main.rs` served (see `README.md` next to this file for every route, its auth class and
//! the function that serves it), on axum, talking to the daemon only through [`crate::core::Core`].
//!
//! One fallback handler ([`dispatch`]) mirrors the board's `route()`: the JSON content-type rule
//! first (CSRF), then the public routes, then the key-or-owner check, then the owner's routes.
//! Handlers live in the sibling modules, one per area; everything they share is here.

// ponytail: handlers return `Result<T, Response>` so an early exit is one `?`; the 128-byte Err is
// the response about to be sent, and boxing it would only add an allocation per request that fails.
#![allow(clippy::result_large_err)]

mod accounts;
mod agents;
mod issues;
mod keys;
mod login;
mod terminals;
#[cfg(test)]
mod tests;
mod util;

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use femme_proto::state::Agent;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use self::login::Login;
use self::util::{api_route, lock, now, random_hex, same, text};
use crate::config::Config;
use crate::core::{parse_agent, ApiKey, Core, CoreError};

const PAGE: &str = include_str!("../../../../assets/index.html");
const LOGIN: &str = include_str!("../../../../assets/login.html");
const LOGO: &[u8] = include_bytes!("../../../../assets/logo.svg"); // also the favicon
const OPENAPI: &str = include_str!("../../../../assets/openapi.json"); // the API as another agent reads it
/// Largest JSON body a route reads; more is a 413.
const MAX_BODY: usize = 1 << 20;
/// A pasted screenshot or file, before base64.
const MAX_UPLOAD: usize = 20 << 20;
const UPLOAD_DAYS: u64 = 7;
const SESSION_SECS: u64 = 7 * 86400;
const LINK_SECS: u64 = 15 * 60;

/// What the router shares between requests. Locks are `std` mutexes held for a map access only,
/// never across an `.await`.
pub(crate) struct App {
    core: Arc<dyn Core>,
    /// `<state>/worker.token`, if the file exists: the retired worker.sh's bearer, still the owner's for one release.
    worker_token: Option<String>,
    /// Magic-link token -> expiry (unix s). Memory only: 15 minutes is short anyway.
    links: Mutex<HashMap<String, u64>>,
    /// Client IP -> when it last got a link (unix s). Memory only.
    last_mail: Mutex<HashMap<String, u64>>,
    /// Agent name -> its clone's HEAD when its last task was sent.
    heads: Mutex<HashMap<String, String>>,
    /// Agent name -> the lock a `POST /api/agents/{name}` holds while it looks up, opens and sends.
    openers: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `account_status()`, refreshed in the background.
    accounts: Mutex<Value>,
    /// Sign-ins waiting for their code: `claude`, `gh:<key name>`.
    logins: Mutex<HashMap<String, Login>>,
    /// Key name -> why its last GitHub sign-in failed (shown until the next attempt).
    github_errors: Mutex<HashMap<String, String>>,
}

impl App {
    fn cfg(&self) -> &Config {
        self.core.config()
    }
}

/// The compat router. Serve it with `into_make_service_with_connect_info::<SocketAddr>()`: the
/// loopback-only `/internal/hook` and the sign-in throttle need the peer address.
pub fn router(core: Arc<dyn Core>) -> Router {
    build(core, true).0
}

/// [`router`] plus its state. `background`: start the task that keeps the accounts window fresh
/// (it shells out to cswap and aws, which tests never want).
pub(crate) fn build(core: Arc<dyn Core>, background: bool) -> (Router, Arc<App>) {
    let cfg = core.config();
    let worker_token = fs::read_to_string(cfg.state_path("worker.token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if cfg.board_emails.is_empty() || cfg.board_url.is_empty() {
        tracing::warn!("BOARD_EMAIL and BOARD_URL must be set: nobody can sign in to the board");
    }
    if cfg.resend_api_key.is_empty() {
        tracing::warn!("RESEND_API_KEY is not set: sign-in links are logged instead of emailed");
    }
    let app = Arc::new(App {
        core,
        worker_token,
        links: Mutex::default(),
        last_mail: Mutex::default(),
        heads: Mutex::default(),
        openers: Mutex::default(),
        accounts: Mutex::new(Value::Null),
        logins: Mutex::default(),
        github_errors: Mutex::default(),
    });
    if background {
        accounts::spawn_refresh(&app);
    }
    (
        Router::new().fallback(dispatch).with_state(app.clone()),
        app,
    )
}

/// One request: its head, the peer and the body (taken once by whichever handler reads it).
pub(crate) struct Req {
    method: Method,
    headers: HeaderMap,
    peer: Option<SocketAddr>,
    body: Option<Body>,
}

/// Why a body did not become JSON.
pub(crate) enum BodyErr {
    TooLarge,
    Invalid,
}

impl Req {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// The bearer token of the `Authorization` header, or "".
    fn bearer(&self) -> &str {
        self.header("authorization")
            .strip_prefix("Bearer ")
            .unwrap_or("")
    }

    fn session_cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|c| c.trim().strip_prefix("femme_session=").map(String::from))
            .filter(|t| !t.is_empty())
    }

    /// The client as Caddy saw it: the last X-Forwarded-For entry (the one Caddy appends; anything
    /// before it is the client's own word), else the socket's peer (a local caller).
    fn client_ip(&self) -> String {
        match self
            .header("x-forwarded-for")
            .rsplit(',')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(ip) => ip.to_string(),
            None => self.peer.map(|a| a.ip().to_string()).unwrap_or_default(),
        }
    }

    /// A call from this machine itself: a loopback peer that no reverse proxy forwarded (Caddy
    /// connects from 127.0.0.1 too, but always adds one of these headers).
    fn is_local(&self) -> bool {
        self.peer
            .is_some_and(|p| p.ip().to_canonical().is_loopback())
            && ["x-forwarded-for", "forwarded", "x-real-ip"]
                .iter()
                .all(|h| !self.headers.contains_key(*h))
    }

    /// Reads the body as JSON, at most `limit` bytes.
    // ponytail: a body error that is not the size limit (client hung up) also reads as TooLarge; nobody is left to see the 413.
    async fn json(&mut self, limit: usize) -> Result<Value, BodyErr> {
        let body = self.body.take().ok_or(BodyErr::Invalid)?;
        let bytes = axum::body::to_bytes(body, limit)
            .await
            .map_err(|_| BodyErr::TooLarge)?;
        serde_json::from_slice(&bytes).map_err(|_| BodyErr::Invalid)
    }

    /// [`Req::json`] with the standard answers: 413 over [`MAX_BODY`], 400 for anything that is not JSON.
    async fn json_or_400(&mut self) -> Result<Value, Response> {
        self.json(MAX_BODY).await.map_err(|e| match e {
            BodyErr::TooLarge => err(413, "request body over 1 MiB"),
            BodyErr::Invalid => err(400, "invalid JSON"),
        })
    }
}

async fn dispatch(State(app): State<Arc<App>>, http: axum::extract::Request) -> Response {
    let (head, body) = http.into_parts();
    let path = head.uri.path().to_string();
    let peer = head
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let mut req = Req {
        method: head.method,
        headers: head.headers,
        peer,
        body: Some(body),
    };
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    route(&app, &mut req, &parts).await
}

async fn route(app: &Arc<App>, req: &mut Req, parts: &[&str]) -> Response {
    let method = req.method.clone();
    let m = method.as_str();

    // Bodies must be JSON: a cross-site form can't send that without a CORS preflight,
    // which this server never answers, so this doubles as CSRF protection.
    if matches!(m, "POST" | "PATCH") && !req.header("content-type").starts_with("application/json")
    {
        return err(415, "expected application/json");
    }

    // Public routes: the sign-in flow, and the daemon's own hook door (loopback only).
    match (m, parts) {
        ("GET", ["login"]) | ("GET", ["magic", _]) => return html(LOGIN),
        ("GET", ["logo.svg"]) => return typed(LOGO.to_vec(), "image/svg+xml"),
        ("POST", ["api", "login"]) => return request_link(app, req).await,
        ("POST", ["api", "magic"]) => return use_link(app, req).await,
        ("POST", ["internal", "hook"]) => return hook(app, req).await,
        _ => {}
    }

    // Another agent's key opens the routes in api_route and nothing else.
    let caller = find_key(app, req.bearer()).await;
    if caller.is_some() {
        if !api_route(m, parts) {
            return err(403, "this key only reaches /api/ping, /api/openapi.json, /api/agents/{name} with its /push and /discard, /api/terminals, and /api/terminals/{pane} with its /prompt and /input");
        }
    } else if !signed_in(app, req.bearer(), req.session_cookie()).await {
        return match parts {
            ["api", ..] => err(401, "sign in first"),
            _ => redirect("/login"),
        };
    }

    match (m, parts) {
        ("GET", [""]) => html(PAGE),
        ("GET", ["api", "ping"]) => ok(json!({"ok": true})),
        ("GET", ["api", "openapi.json"]) => typed(
            OPENAPI
                .replace("{BOARD_URL}", &app.cfg().board_url)
                .into_bytes(),
            "application/json",
        ),
        // One standing Claude per name, kept between calls so the caller's tasks share a session
        // instead of each opening a terminal.
        ("GET", ["api", "agents", name]) => agents::get(app, name).await,
        ("POST", ["api", "agents", name]) => agents::post(app, req, name, caller.as_ref()).await,
        // The owner's answer to a task's report: the changes stay uncommitted until one of these comes.
        ("POST", ["api", "agents", name, verdict @ ("push" | "discard")]) => {
            agents::verdict(app, req, name, verdict).await
        }
        ("DELETE", ["api", "agents", name]) => agents::delete(app, name).await,
        // Keys for other agents: made and revoked here, shown once. A key may carry a GitHub account.
        ("GET", ["api", "keys"]) => keys::list(app).await,
        ("POST", ["api", "keys"]) => keys::create(app, req).await,
        ("DELETE", ["api", "keys", name]) => keys::delete(app, name).await,
        ("POST", ["api", "keys", name, "github"]) => keys::github_start(app, name).await,
        ("DELETE", ["api", "keys", name, "github"]) => keys::github_cancel(app, name).await,
        ("POST", ["api", "logout"]) => logout(app, req).await,
        ("GET", ["api", "issues"]) => issues::list(app).await,
        ("POST", ["api", "issues"]) => issues::create(app, req).await,
        ("PATCH", ["api", "issues", id]) => issues::patch(app, req, id).await,
        ("DELETE", ["api", "issues", id]) => issues::delete(app, id).await,
        ("GET", ["api", "meta"]) => meta(app).await,
        // deploy.sh's last result, shown until closed.
        ("DELETE", ["api", "deploy"]) => {
            let _ = fs::remove_file(app.cfg().state_path("deploy.json"));
            ok(json!({"ok": true}))
        }
        // Terminals: list, open, close, read, type into. Issue chat uses the same routes through the issue's pane.
        ("GET", ["api", "terminals"]) => terminals::list(app).await,
        ("POST", ["api", "terminals"]) => terminals::create(app, req).await,
        ("DELETE", ["api", "terminals", ws]) => terminals::close(app, ws).await,
        // The browser terminal: the pane's screen with its colours, and raw keystrokes back.
        ("GET", ["api", "terminals", pane, "screen"]) => terminals::screen(app, pane, true).await,
        // For another agent's key (the owner has the page): a terminal's status and screen as text,
        // and a whole message to the agent in it; raw keys are /input.
        ("GET", ["api", "terminals", pane]) => terminals::screen(app, pane, false).await,
        ("POST", ["api", "terminals", pane, "prompt"]) => terminals::prompt(app, req, pane).await,
        ("POST", ["api", "terminals", pane, "input"]) => terminals::input(app, req, pane).await,
        // A pasted or dropped file lands in <state>/uploads; the page then types its path into the terminal.
        ("POST", ["api", "terminals", "upload"]) => terminals::upload(app, req).await,
        // Accounts: Claude's (cswap) with the usage of each, and the box's AWS CLI.
        ("GET", ["api", "accounts"]) => accounts::list(app).await,
        ("POST", ["api", "accounts", "switch"]) => accounts::switch(app, req).await,
        ("POST", ["api", "accounts", kind @ "claude", "login"]) => accounts::login(app, kind).await,
        ("POST", ["api", "accounts", kind @ "claude", "code"]) => {
            accounts::code(app, req, kind).await
        }
        ("DELETE", ["api", "accounts", kind, "login"]) => accounts::cancel(app, kind).await,
        _ => err(404, "not found"),
    }
}

/// The API key the request's bearer is, if any. Compared in constant time against every key.
async fn find_key(app: &App, bearer: &str) -> Option<ApiKey> {
    if bearer.is_empty() {
        return None;
    }
    match app.core.api_keys().await {
        Ok(keys) => keys
            .into_iter()
            .find(|k| same(k.key.as_bytes(), bearer.as_bytes())),
        Err(e) => {
            tracing::warn!("cannot read the API keys: {e}");
            None
        }
    }
}

/// The owner: the worker token's bearer, or a live session cookie.
async fn signed_in(app: &App, bearer: &str, cookie: Option<String>) -> bool {
    if let Some(token) = &app.worker_token {
        if same(bearer.as_bytes(), token.as_bytes()) {
            return true;
        }
    }
    match cookie {
        Some(cookie) => app.core.web_session_valid(digest(&cookie)).await,
        None => false,
    }
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Sign-in step 1: a magic link to an allowed address, whatever the asker typed.
async fn request_link(app: &Arc<App>, req: &mut Req) -> Response {
    let input = req.json(MAX_BODY).await.unwrap_or_default();
    let asked = text(&input, "email").trim().to_lowercase();
    // Same answer either way, so the page never reveals which address is allowed.
    // One link a minute per client (Caddy's X-Forwarded-For), five in all: a stranger posting
    // an owner's address neither keeps the owner from a link nor floods the inbox.
    let ip = req.client_ip();
    let send = {
        let mut last = lock(&app.last_mail);
        last.retain(|_, t| *t + 60 > now());
        let send =
            app.cfg().board_emails.contains(&asked) && !last.contains_key(&ip) && last.len() < 5;
        if send {
            last.insert(ip, now());
        }
        send
    };
    if send {
        let token = random_hex();
        {
            let mut links = lock(&app.links);
            links.retain(|_, exp| *exp > now());
            links.insert(token.clone(), now() + LINK_SECS);
        }
        let link = format!("{}/magic/{token}", app.cfg().board_url);
        let cfg = app.cfg().clone();
        // Off this request: the answer takes the same time whichever address was asked, and
        // Resend's round trip holds up nobody.
        tokio::spawn(async move {
            if let Err(e) = send_link(&cfg, &asked, &link).await {
                tracing::warn!("sign-in mail failed: {e}");
            }
        });
    }
    ok(json!({"ok": true}))
}

async fn send_link(cfg: &Config, to: &str, link: &str) -> Result<(), String> {
    if cfg.resend_api_key.is_empty() {
        tracing::warn!("sign-in link: {link}");
        return Ok(());
    }
    let body = json!({
        "from": cfg.mail_from,
        "to": [to],
        "subject": "Sign in to FEMME",
        "text": format!("Sign in to your FEMME board (valid 15 minutes, once):\n\n{link}\n"),
        "html": format!(r#"<p>Sign in to your FEMME board (valid 15 minutes, once):</p><p><a href="{link}">Sign in to FEMME</a></p>"#),
    });
    // ponytail: a client per mail, which is at most five a minute; keep one in App if that changes.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&cfg.resend_url)
        .bearer_auth(&cfg.resend_api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Sign-in step 2: the link's token for a 7-day cookie.
async fn use_link(app: &Arc<App>, req: &mut Req) -> Response {
    let input = req.json(MAX_BODY).await.unwrap_or_default();
    let exp = lock(&app.links).remove(text(&input, "token"));
    match exp {
        Some(exp) if exp > now() => {
            let session = random_hex();
            match app
                .core
                .web_session_create(digest(&session), (now() + SESSION_SECS) as i64 * 1000)
                .await
            {
                Ok(()) => with_cookie(ok(json!({"ok": true})), app, &session, SESSION_SECS),
                Err(e) => err(500, &format!("save failed: {e}")),
            }
        }
        _ => err(
            400,
            "this link has expired or was already used; ask for a new one",
        ),
    }
}

async fn logout(app: &Arc<App>, req: &mut Req) -> Response {
    if let Some(cookie) = req.session_cookie() {
        let _ = app.core.web_session_delete(digest(&cookie)).await;
    }
    with_cookie(ok(json!({"ok": true})), app, "", 0)
}

/// `POST /internal/hook`: what `femmed hook <agent> <event>` posts from inside an agent's own
/// process. Loopback only, and never through the reverse proxy: it has no sign-in of its own.
async fn hook(app: &Arc<App>, req: &mut Req) -> Response {
    if !req.is_local() {
        return err(403, "this route is for the box itself");
    }
    let body = match req.json(MAX_UPLOAD).await {
        Ok(v) => v,
        Err(BodyErr::TooLarge) => return err(413, "request body too large"),
        Err(BodyErr::Invalid) => return err(400, "invalid JSON"),
    };
    let Some(agent) = body["agent"]
        .as_str()
        .and_then(parse_agent)
        .filter(|a| *a != Agent::Shell)
    else {
        return err(400, "agent: claude, codex or pi");
    };
    if text(&body, "event").is_empty() || text(&body, "sid").is_empty() {
        return err(400, "event and sid are required");
    }
    match app.core.hook(agent, body).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}

async fn meta(app: &Arc<App>) -> Response {
    let mut meta = match app.core.meta().await {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    };
    meta["deploy"] = fs::read_to_string(app.cfg().state_path("deploy.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    ok(meta)
}

/// Runs blocking work (git, gh, cswap, files) off the async threads. The error is the response for a panicked task.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!("blocking task failed: {e}");
        err(500, "internal error")
    })
}

// ---- responses ---------------------------------------------------------------------------------

fn respond(code: u16, ctype: &'static str, body: Vec<u8>) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    r
}

fn typed(body: Vec<u8>, ctype: &'static str) -> Response {
    respond(200, ctype, body)
}

fn json_response(code: u16, v: Value) -> Response {
    respond(code, "application/json", v.to_string().into_bytes())
}

fn ok(v: Value) -> Response {
    json_response(200, v)
}

fn err(code: u16, msg: &str) -> Response {
    json_response(code, json!({"error": msg}))
}

/// The one place a [`CoreError`] becomes an HTTP answer.
fn core_err(e: CoreError) -> Response {
    err(e.status().as_u16(), &e.to_string())
}

fn html(page: &str) -> Response {
    typed(page.as_bytes().to_vec(), "text/html; charset=utf-8")
}

fn redirect(to: &str) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::FOUND;
    r.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(to).expect("a fixed path"),
    );
    r
}

fn with_cookie(mut resp: Response, app: &App, value: &str, max_age: u64) -> Response {
    // Secure only on https, so the board still works over plain http on localhost.
    let secure = if app.cfg().board_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let c =
        format!("femme_session={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}");
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&c).expect("hex and fixed text"),
    );
    resp
}
