//! `BoardApi`: one call of the web app's JSON API, made by the daemon on the desktop's behalf as
//! the owner (the accounts overlay shows what the board shows, and changes it the same way).
//!
//! The call goes to the board's own address ([`base`]) with the owner's bearer, read from
//! `<state>/worker.token` on every call (the file changes when the token is rotated). Only GET,
//! POST and DELETE are made, and only below `/api/accounts`, `/api/keys` and `/api/meta`: a
//! desktop is a trusted device, but this is a way to act as the owner on the board's routes, and
//! it is narrowed to the ones the overlay needs. What the board answers goes back whatever its
//! status: a 404 or a 409 is for the desktop to show, not an error of ours.

use std::sync::OnceLock;
use std::time::Duration;

use femme_proto::rpc::{Err as WireErr, ErrCode, Resp};
use reqwest::{header, redirect, Method, Url};

use super::req::err;
use crate::config::Config;

/// The routes the desktop may call, and everything below them.
const ALLOWED: [&str; 3] = ["/api/accounts", "/api/keys", "/api/meta"];
/// The most a request or an answer body may weigh (the board's own limit on what it takes).
const BODY_MAX: usize = 1 << 20;
/// How long the board has to answer.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Where the board listens when it is the herdr board beside this daemon (`FEMMED=1`).
const HERDR_BOARD: &str = "http://127.0.0.1:8787";

/// The board's address: `FEMME_BOARD_API` when set, this daemon's own `http_bind` when it is the
/// board (`FEMMED=2`), else the herdr board's.
fn base(cfg: &Config) -> String {
    match &cfg.board_api {
        Some(url) => url.clone(),
        None if cfg.replaces_board => format!("http://{}", cfg.http_bind),
        None => HERDR_BOARD.to_string(),
    }
}

/// The one client: no proxy from the environment (the board is on this box, and the bearer must
/// not go through anyone), no redirects (one would take the bearer elsewhere), no idle
/// connections kept (a call is rare, and a kept one would belong to a runtime that may be gone).
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .build()
            .expect("reqwest client")
    })
}

/// Whether `route` (a path without its query) is one of [`ALLOWED`] or below one.
fn under_allowed(route: &str) -> bool {
    ALLOWED.iter().any(|p| {
        route == *p
            || route
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether `path` (as the desktop sent it, query included) is one it may call: under an allowed
/// route, and plain. A dot segment (also percent-encoded), an encoded slash, a backslash, a
/// fragment, a space, a control or non-ASCII character is refused: the URL parser would resolve
/// the first into another route, and what the board's router makes of the rest is not ours to guess.
fn path_allowed(path: &str) -> bool {
    let route = path.split_once('?').map_or(path, |(route, _)| route);
    let lower = route.to_ascii_lowercase();
    under_allowed(route)
        && !path
            .bytes()
            .any(|b| b.is_ascii_control() || !b.is_ascii() || matches!(b, b' ' | b'\\' | b'#'))
        && !route.split('/').any(|s| s == "." || s == "..")
        && !["%2e", "%2f", "%5c", "%00"]
            .iter()
            .any(|e| lower.contains(e))
}

/// Makes the call (see the [module docs](self)). `Bad` for a method or path that is not allowed
/// or a body over [`BODY_MAX`]; `Internal` when the board cannot be called (no token, nothing
/// listening, no answer in time, an answer over [`BODY_MAX`]).
pub(crate) async fn call(
    cfg: &Config,
    method: &str,
    path: &str,
    body: String,
) -> Result<Resp, WireErr> {
    call_within(TIMEOUT, cfg, method, path, body).await
}

async fn call_within(
    timeout: Duration,
    cfg: &Config,
    method: &str,
    path: &str,
    body: String,
) -> Result<Resp, WireErr> {
    let bad = |why: &str| Err(err(ErrCode::Bad, why));
    let method = match method.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "DELETE" => Method::DELETE,
        _ => return bad("method must be GET, POST or DELETE"),
    };
    if !path_allowed(path) {
        return bad("path must be under /api/accounts, /api/keys or /api/meta");
    }
    if body.len() > BODY_MAX {
        return bad("a body is at most 1048576 bytes");
    }
    let internal = |why: String| err(ErrCode::Internal, why);
    let base = base(cfg);
    let Ok(url) = Url::parse(&format!("{base}{path}")) else {
        return Err(internal(format!("the board's address {base} is not a URL")));
    };
    // What the parser made of the path is what is sent: it must still be an allowed route.
    if !under_allowed(url.path()) {
        return bad("path must be under /api/accounts, /api/keys or /api/meta");
    }
    let file = cfg.state_path("worker.token");
    let token = match tokio::fs::read_to_string(&file).await {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let why = format!(
                "the board's owner token {} is missing or empty",
                file.display()
            );
            return Err(internal(why));
        }
    };

    let mut request = client()
        .request(method, url)
        .bearer_auth(token)
        .timeout(timeout);
    if !body.is_empty() {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let failed = |e: reqwest::Error| {
        internal(if e.is_timeout() {
            format!("the board did not answer in {} s", timeout.as_secs().max(1))
        } else if e.is_connect() {
            format!("the board is not answering at {base}")
        } else {
            format!("the board call failed: {}", e.without_url())
        })
    };
    let mut response = request.send().await.map_err(failed)?;
    let status = response.status().as_u16();
    let too_big = || internal("the board's answer is over 1048576 bytes".into());
    if response
        .content_length()
        .is_some_and(|n| n > BODY_MAX as u64)
    {
        return Err(too_big());
    }
    let mut answer = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if answer.len() + chunk.len() > BODY_MAX {
            return Err(too_big());
        }
        answer.extend_from_slice(&chunk);
    }
    Ok(Resp::BoardApi {
        status,
        body: String::from_utf8_lossy(&answer).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method as HttpMethod, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::Router;

    use super::*;

    /// What the stub board saw of one request.
    #[derive(Debug, PartialEq)]
    struct Seen {
        method: String,
        target: String,
        bearer: String,
        content_type: String,
        body: String,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// A board on a random port, and what it is asked. `…/missing` is a 404, `…/big` an answer over
    /// the cap, `…/slow` answers after five seconds, `…/moved` redirects; anything else is a 200
    /// that names its target.
    async fn board() -> (String, Log) {
        let log = Log::default();
        let seen = log.clone();
        let app = Router::new().fallback(
            move |method: HttpMethod, uri: Uri, headers: HeaderMap, body: Bytes| {
                let seen = seen.clone();
                async move {
                    let header = |name: &str| {
                        let value = headers.get(name).and_then(|v| v.to_str().ok());
                        value.unwrap_or("").to_string()
                    };
                    let target = uri.path_and_query().map_or("", |p| p.as_str()).to_string();
                    seen.lock().unwrap().push(Seen {
                        method: method.to_string(),
                        target: target.clone(),
                        bearer: header("authorization"),
                        content_type: header("content-type"),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                    match uri.path() {
                        p if p.ends_with("/missing") => {
                            (StatusCode::NOT_FOUND, "no such thing").into_response()
                        }
                        p if p.ends_with("/big") => "x".repeat(BODY_MAX + 1).into_response(),
                        p if p.ends_with("/slow") => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            "late".into_response()
                        }
                        p if p.ends_with("/moved") => {
                            (StatusCode::FOUND, [("location", "/api/meta")]).into_response()
                        }
                        _ => format!("{{\"target\":\"{target}\"}}").into_response(),
                    }
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, log)
    }

    fn config(dir: &Path, board: &str) -> Config {
        let mut cfg = Config::from_vars([("HOME".to_string(), dir.display().to_string())]);
        cfg.state_dir = dir.join("state");
        cfg.board_api = Some(board.to_string());
        std::fs::create_dir_all(&cfg.state_dir).unwrap();
        cfg
    }

    fn token(cfg: &Config, text: &str) {
        std::fs::write(cfg.state_path("worker.token"), text).unwrap();
    }

    fn answer(status: u16, body: &str) -> Result<Resp, WireErr> {
        Ok(Resp::BoardApi {
            status,
            body: body.into(),
        })
    }

    fn code(r: Result<Resp, WireErr>) -> ErrCode {
        r.expect_err("an error").code
    }

    #[tokio::test]
    async fn a_get_a_post_and_a_delete_reach_the_board_with_the_owner_bearer() {
        let (tmp, (url, log)) = (tempfile::tempdir().unwrap(), board().await);
        let cfg = config(tmp.path(), &url);
        token(&cfg, "s3cret\n");
        let get = call(&cfg, "GET", "/api/accounts?full=1", String::new()).await;
        assert_eq!(get, answer(200, r#"{"target":"/api/accounts?full=1"}"#));
        let post = call(&cfg, "post", "/api/keys", r#"{"name":"k"}"#.into()).await;
        assert_eq!(post, answer(200, r#"{"target":"/api/keys"}"#));
        call(&cfg, "DELETE", "/api/keys/k", String::new())
            .await
            .unwrap();
        let seen = |method: &str, target: &str, content_type: &str, body: &str| Seen {
            method: method.into(),
            target: target.into(),
            bearer: "Bearer s3cret".into(),
            content_type: content_type.into(),
            body: body.into(),
        };
        assert_eq!(
            *log.lock().unwrap(),
            [
                seen("GET", "/api/accounts?full=1", "", ""),
                seen("POST", "/api/keys", "application/json", r#"{"name":"k"}"#),
                seen("DELETE", "/api/keys/k", "", ""),
            ]
        );
    }

    #[tokio::test]
    async fn what_the_board_says_is_passed_on_whatever_its_status() {
        let (tmp, (url, log)) = (tempfile::tempdir().unwrap(), board().await);
        let cfg = config(tmp.path(), &url);
        token(&cfg, "t");
        let missing = call(&cfg, "GET", "/api/accounts/missing", String::new()).await;
        assert_eq!(missing, answer(404, "no such thing"));
        // A redirect is not followed: it would take the bearer wherever it points.
        let moved = call(&cfg, "GET", "/api/meta/moved", String::new()).await;
        assert_eq!(moved, answer(302, ""));
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn only_get_post_and_delete_below_the_three_routes_are_called() {
        let (tmp, (url, log)) = (tempfile::tempdir().unwrap(), board().await);
        let cfg = config(tmp.path(), &url);
        token(&cfg, "t");
        for method in ["PUT", "PATCH", "HEAD", "CONNECT", ""] {
            let r = call(&cfg, method, "/api/accounts", String::new()).await;
            assert_eq!(code(r), ErrCode::Bad, "{method:?}");
        }
        for path in [
            "",
            "/",
            "/api",
            "/api/issues",
            "/api/terminals",
            "/api/accountsx",
            "/api/keysx/1",
            "api/accounts",
            "/api/accounts/../issues",
            "/api/accounts/./x",
            "/api/accounts/%2e%2e/issues",
            "/api/accounts/%2E%2E/issues",
            "/api/accounts/..%2fissues",
            "/api/accounts/%5c..%5cissues",
            "/api/meta\\..\\issues",
            "//evil.example/api/accounts",
            "http://evil.example/api/accounts",
            "/api/accounts#x",
            "/api/accounts x",
            "/api/accounts\n",
            "/api/accounts/é",
        ] {
            let r = call(&cfg, "GET", path, String::new()).await;
            assert_eq!(code(r), ErrCode::Bad, "{path:?}");
        }
        assert!(log.lock().unwrap().is_empty(), "nothing got to the board");
        for path in [
            "/api/accounts",
            "/api/accounts/",
            "/api/accounts/3/disable",
            "/api/keys",
            "/api/keys/my-key",
            "/api/meta",
            "/api/meta?x=1",
            "/api/accounts?next=../x&a=%2e",
        ] {
            assert!(path_allowed(path), "{path}");
        }
    }

    #[tokio::test]
    async fn a_body_may_weigh_a_mebibyte_either_way_and_no_more() {
        let (tmp, (url, _)) = (tempfile::tempdir().unwrap(), board().await);
        let cfg = config(tmp.path(), &url);
        token(&cfg, "t");
        let exactly = "y".repeat(BODY_MAX);
        let r = call(&cfg, "POST", "/api/accounts", exactly).await;
        assert!(matches!(r, Ok(Resp::BoardApi { status: 200, .. })), "{r:?}");
        let over = "y".repeat(BODY_MAX + 1);
        let r = call(&cfg, "POST", "/api/accounts", over).await;
        assert_eq!(code(r), ErrCode::Bad);
        let r = call(&cfg, "GET", "/api/accounts/big", String::new()).await;
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrCode::Internal);
        assert!(e.msg.contains("over"), "{}", e.msg);
    }

    #[tokio::test]
    async fn the_token_is_read_on_every_call_and_missing_is_an_internal_error() {
        let (tmp, (url, log)) = (tempfile::tempdir().unwrap(), board().await);
        let cfg = config(tmp.path(), &url);
        let get = || call(&cfg, "GET", "/api/meta", String::new());
        let e = get().await.unwrap_err();
        assert_eq!(e.code, ErrCode::Internal);
        assert!(e.msg.contains("worker.token"), "{}", e.msg);
        token(&cfg, " \n");
        assert_eq!(
            code(get().await),
            ErrCode::Internal,
            "an empty token is none"
        );
        token(&cfg, "one");
        get().await.unwrap();
        token(&cfg, "two");
        get().await.unwrap();
        let bearers: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.bearer.clone())
            .collect();
        assert_eq!(bearers, ["Bearer one", "Bearer two"]);
    }

    #[tokio::test]
    async fn a_board_that_is_down_or_slow_is_an_internal_error_that_says_so() {
        let (tmp, (url, _)) = (tempfile::tempdir().unwrap(), board().await);
        token(&config(tmp.path(), &url), "t");
        // Nothing listens there any more.
        let gone = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let down = call(
            &config(tmp.path(), &gone),
            "GET",
            "/api/meta",
            String::new(),
        )
        .await;
        let e = down.unwrap_err();
        assert_eq!(e.code, ErrCode::Internal);
        assert!(e.msg.contains("not answering"), "{}", e.msg);

        let cfg = config(tmp.path(), &url);
        let slow = Duration::from_millis(300);
        let r = call_within(slow, &cfg, "GET", "/api/meta/slow", String::new()).await;
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrCode::Internal);
        assert!(e.msg.contains("did not answer"), "{}", e.msg);
        let bad_url = call(
            &config(tmp.path(), "not a url"),
            "GET",
            "/api/meta",
            String::new(),
        )
        .await;
        assert_eq!(code(bad_url), ErrCode::Internal);
    }

    #[test]
    fn the_board_is_found_at_the_override_this_daemon_or_the_herdr_board() {
        let cfg = |vars: &[(&str, &str)]| {
            Config::from_vars(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        };
        assert_eq!(base(&cfg(&[])), "http://127.0.0.1:8787");
        assert_eq!(base(&cfg(&[("FEMMED", "1")])), "http://127.0.0.1:8787");
        let own = cfg(&[("FEMMED", "2"), ("HTTP_BIND", "127.0.0.1:9000")]);
        assert_eq!(base(&own), "http://127.0.0.1:9000");
        let set = cfg(&[("FEMMED", "2"), ("FEMME_BOARD_API", "http://127.0.0.1:4/")]);
        assert_eq!(base(&set), "http://127.0.0.1:4");
    }
}
