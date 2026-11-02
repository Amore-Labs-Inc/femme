//! The compat layer's tests: the board's unit tests (moved with the code they cover) and the
//! router driven end to end with `tower::ServiceExt::oneshot` against [`FakeCore`].
//! External tools (`gh`, `git` for clones with a token, `cswap`, `aws`, `claude`) are fake
//! scripts in a temp dir that `Config::tool_path` puts first on PATH; real `git` does the git work.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use std::time::SystemTime;

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use femme_proto::state::{Attention, Issue, IssueStatus, SessionStatus};
use serde_json::json;
use tower::ServiceExt;

use super::util::*;
use super::*;
use crate::core::{on_machine, valid_id};
use crate::core_fake::FakeCore;
use crate::testutil::install_tool;

// ---- the board's unit tests --------------------------------------------------------------------

#[test]
fn repo_validation_blocks_shell_and_path_tricks() {
    assert!(valid_repo("example-org/femme"));
    assert!(valid_repo("a-b/c_d.e"));
    for bad in [
        "",
        "x",
        "a/b/c",
        "../x",
        "a/..",
        "a/.git",
        "a b/c",
        "a/$(rm)",
        "a/b;c",
        "-o/x",
        "a/--upload-pack",
    ] {
        assert!(!valid_repo(bad), "{bad} should be rejected");
    }
}

// Was herdr_ids_are_strict: the id rules now live in core.rs and the machine tag is just a name there
// (which machines exist is the core's call, not a socket file's).
#[test]
fn ids_are_strict() {
    assert!(
        valid_id("w9", false)
            && valid_id("wC", false)
            && valid_id("w9:p1", true)
            && valid_id("wA:p12", true)
    );
    for bad in [
        "", "w", "9", "w9;ls", "w9 p1", "w9:p1", "../w9", "-w9", "w9:x1",
    ] {
        assert!(!valid_id(bad, false), "{bad} is not a workspace");
    }
    for bad in ["w9", "w9:", ":p1", "w9:p1;rm", "w9:p1:p2", "w9:p", "x9:p1"] {
        assert!(!valid_id(bad, true), "{bad} is not a pane");
    }
    assert_eq!(on_machine("w9:p1", true), Some((None, "w9:p1")));
    assert_eq!(
        on_machine("laptop~w9:p1", true),
        Some((Some("laptop"), "w9:p1"))
    );
    assert_eq!(on_machine("laptop~w9", false), Some((Some("laptop"), "w9")));
    for bad in [
        "~w9:p1",
        "../x~w9:p1",
        "a/b~w9:p1",
        "a b~w9:p1",
        "a~b~w9:p1",
        "laptop~w9",
        "laptop~",
    ] {
        assert_eq!(on_machine(bad, true), None, "{bad} is not a linked pane");
    }
}

#[test]
fn api_key_reaches_its_agents_only() {
    for (m, p) in [
        ("GET", "api/ping"),
        ("GET", "api/openapi.json"),
        ("GET", "api/agents/instinct"),
        ("POST", "api/agents/instinct"),
        ("DELETE", "api/agents/instinct"),
        ("POST", "api/agents/instinct/push"),
        ("POST", "api/agents/instinct/discard"),
        ("POST", "api/terminals/w2:p1/input"),
        ("GET", "api/terminals"),
        ("GET", "api/terminals/w2:p1"),
        ("GET", "api/terminals/laptop~w2:p1"),
        ("POST", "api/terminals/w2:p1/prompt"),
    ] {
        assert!(
            api_route(m, &p.split('/').collect::<Vec<_>>()),
            "{m} {p} should be open to the key"
        );
    }
    for (m, p) in [
        ("GET", ""),
        ("GET", "api/issues"),
        ("POST", "api/issues"),
        ("GET", "api/accounts"),
        ("POST", "api/accounts/switch"),
        ("POST", "api/accounts/claude/login"),
        ("POST", "api/terminals"),
        ("DELETE", "api/terminals/w2"),
        ("POST", "api/terminals/w2:p1"),
        ("GET", "api/terminals/w2:p1/screen"),
        ("POST", "api/terminals/upload"),
        ("GET", "api/meta"),
        ("DELETE", "api/deploy"),
        ("POST", "api/logout"),
        ("GET", "reports/x.png"),
        ("PATCH", "api/agents/instinct"),
        ("GET", "api/keys"),
        ("POST", "api/keys"),
        ("DELETE", "api/keys/instinct"),
        ("POST", "internal/hook"),
    ] {
        assert!(
            !api_route(m, &p.split('/').collect::<Vec<_>>()),
            "{m} {p} should be closed to the key"
        );
    }
}

#[test]
fn key_github_fields_are_plain() {
    assert!(valid_token("github_pat_EXAMPLE_not_a_real_token_0000"));
    assert!(valid_token("ghp_EXAMPLEnotarealtoken0000"));
    for bad in [
        "",
        "short",
        "ghp_abc def ghijklmnopqrstuvwxyz",
        "ghp_abc;defghijklmnopqrstuvwxyz",
        &"x".repeat(256),
    ] {
        assert!(!valid_token(bad), "{bad:?} should be rejected");
    }
    // Assembled, so no credential-looking literal sits in the source.
    assert_eq!(
        clone_url("example-org/example-repo", "T"),
        [
            "https://x-access-token:",
            "T",
            "@github.com/example-org/example-repo.git"
        ]
        .concat()
    );
    assert_eq!(
        clone_url("example-org/example-repo", ""),
        "https://github.com/example-org/example-repo.git"
    );
    assert_eq!(
        noreply(12345678, "octocat"),
        "12345678+octocat@users.noreply.github.com"
    );
    let out = "! Failed to copy one-time code to clipboard\n  No clipboard utilities available.\n! First copy your one-time code: 24E9-601B\nOpen this URL to continue in your web browser: https://github.com/login/device\n";
    assert_eq!(device_code(out).as_deref(), Some("24E9-601B"));
    assert_eq!(device_code("- Logging into github.com\n"), None);
    // The with_github half of this test is keys_crud_and_github_accounts below: the account now goes through the core.
}

#[test]
fn outcome_reads_the_clone() {
    let cfg = Config::from_vars([]);
    let dir = std::env::temp_dir().join(format!("femme-outcome-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| agents::git(&cfg, &dir, args);
    assert_eq!(
        agents::outcome(&cfg, &dir, None, "working", "x")["message"],
        Value::Null
    );
    assert_eq!(
        agents::outcome(&cfg, &dir, None, "idle", "x")["message"],
        "finished: read the screen"
    );
    // A clone whose HEAD sits on origin/main: pushed, unless it is the HEAD the task started from.
    let sh = |c: &str| {
        assert!(
            StdCommand::new("sh")
                .arg("-c")
                .arg(c)
                .current_dir(&dir)
                .output()
                .unwrap()
                .status
                .success(),
            "{c}"
        )
    };
    sh("git init -q -b main && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m x \
        && git remote add origin https://github.com/example-org/example-repo.git && git update-ref refs/remotes/origin/main HEAD");
    let head = git(&["rev-parse", "HEAD"]).unwrap();
    let v = agents::outcome(&cfg, &dir, Some("0000"), "idle", "x");
    assert_eq!(v["message"], "changes created and pushed to example-repo");
    assert_eq!(v["pushed"], true);
    assert_eq!(v["commit"], head);
    assert_eq!(
        agents::outcome(&cfg, &dir, Some(&head), "done", "x")["message"],
        "finished, nothing new to push: read the screen"
    );
    sh("echo x > f");
    assert_eq!(
        agents::outcome(&cfg, &dir, Some("0000"), "idle", "x")["message"],
        "changes ready, not pushed to main: read the report, then POST /api/agents/x/push or /discard"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn names_and_branches_are_plain() {
    for good in ["instinct", "onur-2", "a"] {
        assert!(valid_name(good), "{good}");
    }
    for bad in [
        "",
        "-x",
        "Instinct",
        "a b",
        "a/b",
        "a;b",
        "..",
        &"x".repeat(33),
    ] {
        assert!(!valid_name(bad), "{bad} should be rejected");
    }
    for good in ["main", "release/2.1", "feat_x-1", "v1.0"] {
        assert!(valid_branch(good), "{good}");
    }
    for bad in [
        "",
        "-f",
        "--force",
        "a..b",
        "a b",
        "a;b",
        "$(x)",
        "a/.git/../b",
        &"x".repeat(100),
    ] {
        assert!(!valid_branch(bad), "{bad} should be rejected");
    }
}

#[test]
fn openapi_is_json_with_every_key_route() {
    let spec: Value =
        serde_json::from_str(&OPENAPI.replace("{BOARD_URL}", "https://x.test")).unwrap();
    assert_eq!(spec["servers"][0]["url"], "https://x.test");
    let paths = spec["paths"].as_object().unwrap();
    for (m, p) in [
        ("get", "/api/ping"),
        ("get", "/api/openapi.json"),
        ("get", "/api/agents/{name}"),
        ("post", "/api/agents/{name}"),
        ("delete", "/api/agents/{name}"),
        ("post", "/api/terminals/{pane_id}/input"),
    ] {
        assert!(paths[p][m].is_object(), "{m} {p} is not described");
    }
}

#[test]
fn home_dir_stays_inside_home() {
    let home = std::env::temp_dir().join(format!("femme-home-{}", std::process::id()));
    fs::create_dir_all(home.join("proj")).unwrap();
    assert!(home_dir(&home, "").is_some());
    assert!(home_dir(&home, home.join("proj").to_str().unwrap()).is_some());
    assert!(home_dir(&home, home.join("proj/../..").to_str().unwrap()).is_none());
    assert!(home_dir(&home, "/etc").is_none());
    assert!(home_dir(&home, home.join("missing").to_str().unwrap()).is_none());
    fs::remove_dir_all(&home).unwrap();
}

#[test]
fn login_url_finds_the_link_plain_or_hyperlinked() {
    let url = "https://claude.com/cai/oauth/authorize?code=true&client_id=x&state=y";
    let plain = format!("Opening browser to sign in…\nIf the browser didn't open, visit: {url}\nPaste code here if prompted > ");
    assert_eq!(login_url(&plain).as_deref(), Some(url));
    let osc8 = format!("visit: \x1b]8;;{url}\x07{url}\x1b]8;;\x07\n");
    assert_eq!(login_url(&osc8).as_deref(), Some(url));
    let aws = "https://eu-west-2.signin.aws.amazon.com/v1/authorize?response_type=code&client_id=x";
    assert_eq!(
        login_url(&format!(
            "Please visit the following URL:\n\n{aws}\n\nEnter the authorization code"
        ))
        .as_deref(),
        Some(aws)
    );
    assert_eq!(login_url("visit: https://example.com/elsewhere"), None);
    assert_eq!(login_url("Paste code here if prompted > "), None);
}

#[test]
fn sign_in_codes_long_or_wrapped() {
    let aws = "Y29kZT1leUo2YVhBaU9pSkVSVVlp".repeat(100); // 2800 chars, like AWS's
    assert_eq!(sign_in_code(&aws).as_deref(), Some(aws.as_str()));
    assert_eq!(sign_in_code(" abc#def\n").as_deref(), Some("abc#def"));
    assert_eq!(sign_in_code("abc\ndef").as_deref(), Some("abcdef"));
    assert_eq!(sign_in_code("ab\x1bc"), None);
    assert_eq!(sign_in_code("  "), None);
    assert_eq!(sign_in_code(&"a".repeat(16_001)), None);
}

#[test]
fn upload_names_stay_plain() {
    assert_eq!(
        clean_name("Screenshot 2026-10-20 at 10.01.png"),
        "Screenshot2026-10-20at10.01.png"
    );
    assert_eq!(clean_name("../../.ssh/id rsa"), "sshidrsa"); // no slashes, no leading dot
    assert_eq!(clean_name(""), "paste");
    assert_eq!(clean_name(&format!("{}.png", "a".repeat(100))).len(), 60);
    assert!(clean_name(&format!("{}.png", "a".repeat(100))).ends_with(".png"));
}

// The board mutated a JSON issue; apply now checks the same input and returns the store's patch.
#[test]
fn apply_whitelists_fields_and_statuses() {
    assert!(apply(&json!({"status": "pr", "pr_url": "https://x"})).is_ok());
    assert!(apply(&json!({"status": "hacked"})).is_err());
    assert!(apply(&json!({"id": 9})).is_err());
    assert!(apply(&json!({"title": 5})).is_err());
    assert!(apply(&json!({"resume_at": 1700000000})).is_ok());
    assert!(apply(&json!([1])).is_err());
    assert!(apply(&json!({"status": "triage"})).is_err());
    assert!(apply(&json!({"repo": "../x"})).is_err());
    assert!(apply(&json!({"resume_at": "soon"})).is_err());
}

#[test]
fn tokens_compare_strictly() {
    assert!(
        same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab") && !same(b"", b"a")
    );
}

#[test]
fn client_ip_is_the_proxys_word() {
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let req = |xff: Option<&str>| {
        let mut headers = HeaderMap::new();
        if let Some(v) = xff {
            headers.insert("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        Req {
            method: Method::GET,
            headers,
            peer: Some(peer),
            body: None,
        }
    };
    assert_eq!(req(Some("1.2.3.4")).client_ip(), "1.2.3.4");
    assert_eq!(req(Some("9.9.9.9, 1.2.3.4")).client_ip(), "1.2.3.4"); // the client's own header comes first
    assert_eq!(req(Some(" , ")).client_ip(), "127.0.0.1");
    assert_eq!(req(None).client_ip(), "127.0.0.1");
}

// ---- the router --------------------------------------------------------------------------------

struct Harness {
    _dir: tempfile::TempDir,
    router: Router,
    app: Arc<App>,
    core: Arc<FakeCore>,
    home: PathBuf,
    state: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

fn setup() -> Harness {
    setup_with(|_| {})
}

fn setup_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(home.join("femme-work")).unwrap();
    let mut cfg = Config::from_vars([
        ("HOME".to_string(), home.display().to_string()),
        (
            "BOARD_EMAIL".to_string(),
            "owner@x.test, other@x.test".to_string(),
        ),
        (
            "BOARD_URL".to_string(),
            "https://femme.example.com".to_string(),
        ),
    ]);
    cfg.tool_path = Some(format!("{}:/usr/bin:/bin", bin.display()));
    tweak(&mut cfg);
    let state = cfg.state_dir.clone();
    fs::create_dir_all(&state).unwrap();
    let core = Arc::new(FakeCore::new(cfg));
    let (router, app) = build(core.clone(), false);
    Harness {
        log: dir.path().join("tools.log"),
        _dir: dir,
        router,
        app,
        core,
        home,
        state,
        bin,
    }
}

impl Harness {
    /// A fake tool: logs its name and arguments, then runs `script` (shell).
    fn tool(&self, name: &str, script: &str) {
        install_tool(
            &self.bin,
            name,
            &format!("echo \"{name} $*\" >> '{}'\n{script}\n", self.log.display()),
        );
    }

    fn tool_log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// A signed-in owner's Cookie header, made through the core like a magic link would.
    async fn cookie(&self) -> String {
        let value = random_hex();
        self.core
            .web_session_create(digest(&value), now_ms() + 3_600_000)
            .await
            .unwrap();
        format!("femme_session={value}")
    }

    /// An API key made on the board; its bearer.
    async fn key(&self, name: &str) -> String {
        let key = ApiKey {
            name: name.into(),
            key: random_hex(),
            created: now_ms(),
            ..Default::default()
        };
        self.core.api_key_create(key.clone()).await.unwrap();
        key.key
    }

    fn call(&self, method: &str, path: &str) -> Call {
        Call {
            router: self.router.clone(),
            method: method.into(),
            path: path.into(),
            headers: Vec::new(),
            body: None,
            peer: None,
        }
    }

    fn get(&self, path: &str) -> Call {
        self.call("GET", path)
    }

    fn delete(&self, path: &str) -> Call {
        self.call("DELETE", path)
    }

    fn post(&self, path: &str, body: &str) -> Call {
        self.call("POST", path)
            .header("content-type", "application/json")
            .body(body)
    }

    fn patch(&self, path: &str, body: &str) -> Call {
        self.call("PATCH", path)
            .header("content-type", "application/json")
            .body(body)
    }

    /// A git repo at `<home>/femme-work/<name>` with one commit and `origin/main` at it (a clone that pushed).
    fn clone_dir(&self, name: &str) -> PathBuf {
        let dir = self.home.join("femme-work").join(name);
        fs::create_dir_all(&dir).unwrap();
        sh(
            &dir,
            "git init -q -b main && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m x \
            && git remote add origin https://github.com/example-org/example-repo.git && git update-ref refs/remotes/origin/main HEAD",
        );
        dir
    }
}

fn sh(dir: &Path, script: &str) {
    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Call {
    router: Router,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    peer: Option<SocketAddr>,
}

struct Res {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Call {
    fn header(mut self, k: &str, v: &str) -> Call {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        self.headers.push((k.into(), v.into()));
        self
    }

    fn body(mut self, b: &str) -> Call {
        self.body = Some(b.as_bytes().to_vec());
        self
    }

    fn cookie(self, c: &str) -> Call {
        self.header("cookie", c)
    }

    fn bearer(self, key: &str) -> Call {
        self.header("authorization", &format!("Bearer {key}"))
    }

    fn peer(mut self, addr: &str) -> Call {
        self.peer = Some(addr.parse().unwrap());
        self
    }

    async fn send(self) -> Res {
        let mut req = Request::builder()
            .method(self.method.as_str())
            .uri(&self.path);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let mut req = req.body(Body::from(self.body.unwrap_or_default())).unwrap();
        if let Some(p) = self.peer {
            req.extensions_mut().insert(ConnectInfo(p));
        }
        let resp = self.router.oneshot(req).await.unwrap();
        let (head, body) = resp.into_parts();
        Res {
            status: head.status.as_u16(),
            headers: head.headers,
            body: axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        }
    }
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    fn error(&self) -> String {
        self.json()["error"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

// ---- sign-in, CSRF, limits ---------------------------------------------------------------------

#[tokio::test]
async fn login_flow() {
    let h = setup();
    // Step 1: the same answer for any address; only an allowed one gets a link.
    let r = h
        .post("/api/login", r#"{"email":" Owner@X.test "}"#)
        .send()
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let token = lock(&h.app.links)
        .keys()
        .next()
        .cloned()
        .expect("a link was made");
    assert_eq!(token.len(), 64);
    // The link opens the sign-in page, which posts the token.
    assert_eq!(h.get(&format!("/magic/{token}")).send().await.text(), LOGIN);
    let r = h
        .post("/api/magic", &json!({"token": token}).to_string())
        .send()
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let set = r.header("set-cookie").to_string();
    for part in [
        "femme_session=",
        "HttpOnly",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=604800",
        "Secure",
    ] {
        assert!(set.contains(part), "{part} in {set}");
    }
    let cookie = set.split(';').next().unwrap().to_string();
    // The cookie is the owner.
    assert_eq!(
        h.get("/api/ping").cookie(&cookie).send().await.json(),
        json!({"ok": true})
    );
    assert_eq!(h.get("/api/ping").send().await.status, 401);
    // The link was single use.
    let again = h
        .post("/api/magic", &json!({"token": token}).to_string())
        .send()
        .await;
    assert_eq!(
        (again.status, again.error().as_str()),
        (
            400,
            "this link has expired or was already used; ask for a new one"
        )
    );
    // Logging out ends the session and clears the cookie.
    let out = h.post("/api/logout", "{}").cookie(&cookie).send().await;
    assert_eq!(out.status, 200);
    assert!(
        out.header("set-cookie").starts_with("femme_session=;")
            && out.header("set-cookie").contains("Max-Age=0")
    );
    assert_eq!(h.get("/api/ping").cookie(&cookie).send().await.status, 401);
}

#[tokio::test]
async fn the_cookie_is_secure_only_on_https() {
    let h = setup_with(|c| c.board_url = "http://localhost:8787".into());
    lock(&h.app.links).insert("t".into(), now() + 60);
    let r = h.post("/api/magic", r#"{"token":"t"}"#).send().await;
    assert!(
        !r.header("set-cookie").contains("Secure"),
        "{}",
        r.header("set-cookie")
    );
}

#[tokio::test]
async fn links_expire_and_strangers_get_none() {
    let h = setup();
    lock(&h.app.links).insert("old".into(), now() - 1);
    let r = h.post("/api/magic", r#"{"token":"old"}"#).send().await;
    assert_eq!(r.status, 400);
    for body in [r#"{"token":"nope"}"#, "{}", "not json", r#"{"token":5}"#] {
        assert_eq!(
            h.post("/api/magic", body).send().await.status,
            400,
            "{body}"
        );
    }
    // A stranger's address, a missing one, a broken body: the same 200, and no link.
    lock(&h.app.links).clear();
    for body in [r#"{"email":"stranger@x.test"}"#, "{}", "not json"] {
        let r = h.post("/api/login", body).send().await;
        assert_eq!((r.status, r.json()), (200, json!({"ok": true})), "{body}");
    }
    assert!(lock(&h.app.links).is_empty());
}

#[tokio::test]
async fn links_are_throttled_per_client_and_in_all() {
    let h = setup();
    let ask = |ip: &'static str| {
        h.post("/api/login", r#"{"email":"owner@x.test"}"#)
            .header("x-forwarded-for", ip)
            .send()
    };
    ask("1.1.1.1").await;
    assert_eq!(lock(&h.app.links).len(), 1);
    ask("1.1.1.1").await; // a minute has not passed
    assert_eq!(lock(&h.app.links).len(), 1);
    // The client's own X-Forwarded-For entry comes first; Caddy's, the last, is the one that counts.
    ask("9.9.9.9, 1.1.1.1").await;
    assert_eq!(lock(&h.app.links).len(), 1);
    for ip in ["2.2.2.2", "3.3.3.3", "4.4.4.4", "5.5.5.5", "6.6.6.6"] {
        ask(ip).await;
    }
    assert_eq!(lock(&h.app.links).len(), 5, "five in all");
    // A minute later the first client may ask again.
    lock(&h.app.last_mail).clear();
    ask("1.1.1.1").await;
    assert_eq!(lock(&h.app.links).len(), 6);
}

/// Reads one HTTP request off a connection: (head, body). Answers 200 with an empty JSON object.
async fn read_request(listener: &tokio::net::TcpListener) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut conn, _) = listener.accept().await.unwrap();
    let mut buf = Vec::new();
    let (head, body) = loop {
        let mut chunk = [0u8; 4096];
        let n = conn.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed mid-request");
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        if buf.len() >= end + 4 + len {
            break (
                head,
                String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string(),
            );
        }
    };
    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
    (head, body)
}

#[tokio::test]
async fn the_link_goes_out_through_resend() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/emails", listener.local_addr().unwrap());
    let h = setup_with(|c| {
        c.resend_url = url;
        c.resend_api_key = "re_test_key".into();
        c.mail_from = "FEMME <hello@x.test>".into();
    });
    let r = h
        .post("/api/login", r#"{"email":"owner@x.test"}"#)
        .send()
        .await;
    assert_eq!(r.status, 200);
    let (head, body) = tokio::time::timeout(Duration::from_secs(10), read_request(&listener))
        .await
        .expect("the mail request came");
    assert!(head.starts_with("POST /emails HTTP/1.1"), "{head}");
    let head = head.to_lowercase();
    assert!(
        head.contains("authorization: bearer re_test_key")
            && head.contains("content-type: application/json"),
        "{head}"
    );
    let mail: Value = serde_json::from_str(&body).unwrap();
    let token = lock(&h.app.links).keys().next().cloned().unwrap();
    let link = format!("https://femme.example.com/magic/{token}");
    assert_eq!(mail["from"], "FEMME <hello@x.test>");
    assert_eq!(mail["to"], json!(["owner@x.test"]));
    assert_eq!(mail["subject"], "Sign in to FEMME");
    assert!(
        mail["text"].as_str().unwrap().contains(&link)
            && mail["html"]
                .as_str()
                .unwrap()
                .contains(&format!("href=\"{link}\""))
    );
}

#[tokio::test]
async fn bodies_must_be_json_even_before_sign_in() {
    let h = setup();
    let cookie = h.cookie().await;
    for ctype in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "",
    ] {
        for (method, path) in [
            ("POST", "/api/issues"),
            ("PATCH", "/api/issues/1"),
            ("POST", "/api/login"),
            ("POST", "/api/logout"),
        ] {
            for signed_in in [true, false] {
                let mut c = h.call(method, path).body("{}");
                if !ctype.is_empty() {
                    c = c.header("content-type", ctype);
                }
                if signed_in {
                    c = c.cookie(&cookie);
                }
                let r = c.send().await;
                assert_eq!(
                    (r.status, r.error().as_str()),
                    (415, "expected application/json"),
                    "{method} {path} {ctype:?} signed_in={signed_in}"
                );
            }
        }
    }
    // A charset after the type is fine; DELETE carries no body and needs no type.
    let r = h
        .post("/api/issues", r#"{"repo":"a/b","title":"t"}"#)
        .header("content-type", "application/json; charset=utf-8")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(
        h.delete("/api/issues/1")
            .cookie(&cookie)
            .send()
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn bodies_have_size_limits() {
    let h = setup();
    let cookie = h.cookie().await;
    let big = format!(r#"{{"repo":"a/b","title":"{}"}}"#, "x".repeat(MAX_BODY));
    let r = h.post("/api/issues", &big).cookie(&cookie).send().await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (413, "request body over 1 MiB")
    );
    let almost = format!(
        r#"{{"repo":"a/b","title":"{}"}}"#,
        "x".repeat(MAX_BODY - 100)
    );
    assert_eq!(
        h.post("/api/issues", &almost)
            .cookie(&cookie)
            .send()
            .await
            .status,
        201
    );
    // Uploads may be a base64 20 MB file; more is refused.
    let huge = format!(
        r#"{{"name":"a.bin","data":"{}"}}"#,
        "A".repeat(MAX_UPLOAD / 3 * 4 + 8192)
    );
    let r = h
        .post("/api/terminals/upload", &huge)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (413, "files up to 20 MB"));
    // Unreadable JSON is a 400 everywhere.
    for path in [
        "/api/issues",
        "/api/keys",
        "/api/terminals",
        "/api/terminals/upload",
    ] {
        let r = h.post(path, "{not json").cookie(&cookie).send().await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (400, "invalid JSON"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn pages_and_the_sign_in_gate() {
    let h = setup();
    // Public: the sign-in page (also behind a magic link) and the logo.
    assert_eq!(h.get("/login").send().await.text(), LOGIN);
    let r = h.get("/login").send().await;
    assert_eq!(r.header("content-type"), "text/html; charset=utf-8");
    assert_eq!(h.get("/magic/anything").send().await.text(), LOGIN);
    let logo = h.get("/logo.svg").send().await;
    assert_eq!(
        (logo.header("content-type"), logo.body.as_slice()),
        ("image/svg+xml", LOGO)
    );
    // Everything else: pages go to the sign-in page, the API says 401.
    let r = h.get("/").send().await;
    assert_eq!((r.status, r.header("location")), (302, "/login"));
    assert_eq!(h.get("/anything").send().await.status, 302);
    for (method, path) in [
        ("GET", "/api/issues"),
        ("GET", "/api/nothing"),
        ("DELETE", "/api/keys/x"),
        ("GET", "/api/terminals"),
    ] {
        let r = h.call(method, path).send().await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (401, "sign in first"),
            "{method} {path}"
        );
    }
    // A wrong or empty cookie is not the owner.
    assert_eq!(
        h.get("/api/ping")
            .cookie("femme_session=nope")
            .send()
            .await
            .status,
        401
    );
    assert_eq!(
        h.get("/api/ping")
            .cookie("femme_session=")
            .send()
            .await
            .status,
        401
    );
    // Signed in: the page, and 404s for what is not there.
    let cookie = h.cookie().await;
    let r = h.get("/").cookie(&cookie).send().await;
    assert_eq!(
        (r.status, r.header("content-type"), r.text()),
        (200, "text/html; charset=utf-8", PAGE.to_string())
    );
    let r = h.get("/api/nothing").cookie(&cookie).send().await;
    assert_eq!((r.status, r.error().as_str()), (404, "not found"));
    assert_eq!(h.get("/nothing").cookie(&cookie).send().await.status, 404);
    // The cookie can sit among others.
    let both = format!("a=b; {cookie}; c=d");
    assert_eq!(h.get("/api/ping").cookie(&both).send().await.status, 200);
    // Trailing slashes do not matter, as before.
    assert_eq!(h.get("/api/ping/").cookie(&cookie).send().await.status, 200);
}

#[tokio::test]
async fn an_api_key_reaches_its_routes_and_nothing_else() {
    let h = setup();
    let key = h.key("instinct").await;
    assert_eq!(
        h.get("/api/ping").bearer(&key).send().await.json(),
        json!({"ok": true})
    );
    assert_eq!(
        h.get("/api/ping")
            .bearer("0".repeat(64).as_str())
            .send()
            .await
            .status,
        401
    );
    assert_eq!(
        h.get("/api/ping")
            .header("authorization", &key)
            .send()
            .await
            .status,
        401
    ); // no "Bearer "
    let walls = "this key only reaches /api/ping, /api/openapi.json, /api/agents/{name} with its /push and /discard, /api/terminals, and /api/terminals/{pane} with its /prompt and /input";
    for (method, path) in [
        ("GET", "/"),
        ("GET", "/api/issues"),
        ("POST", "/api/issues"),
        ("GET", "/api/keys"),
        ("POST", "/api/keys"),
        ("DELETE", "/api/keys/instinct"),
        ("GET", "/api/accounts"),
        ("GET", "/api/meta"),
        ("POST", "/api/terminals"),
        ("POST", "/api/terminals/upload"),
        ("DELETE", "/api/terminals/w1"),
        ("GET", "/api/terminals/w1:p1/screen"),
        ("PATCH", "/api/agents/instinct"),
        ("POST", "/api/logout"),
    ] {
        let mut c = h.call(method, path).bearer(&key);
        if method != "GET" && method != "DELETE" {
            c = c.header("content-type", "application/json").body("{}");
        }
        let r = c.send().await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (403, walls),
            "{method} {path}"
        );
    }
    // The owner is not walled off.
    let cookie = h.cookie().await;
    assert_eq!(h.get("/api/keys").cookie(&cookie).send().await.status, 200);
    // A revoked key is no key.
    h.core.api_key_delete("instinct").await.unwrap();
    assert_eq!(h.get("/api/ping").bearer(&key).send().await.status, 401);
}

#[tokio::test]
async fn the_worker_token_is_the_owner_for_now() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    fs::create_dir_all(home.join(".local/state/femme")).unwrap();
    fs::write(
        home.join(".local/state/femme/worker.token"),
        "w0rk3r-token\n",
    )
    .unwrap();
    let h = setup_with(|c| {
        *c = Config::from_vars([
            ("HOME".to_string(), home.display().to_string()),
            ("BOARD_URL".to_string(), "http://x".to_string()),
        ])
    });
    assert_eq!(
        h.get("/api/ping")
            .bearer("w0rk3r-token")
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        h.get("/api/issues")
            .bearer("w0rk3r-token")
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        h.get("/api/ping")
            .bearer("w0rk3r-tokeN")
            .send()
            .await
            .status,
        401
    );
    // No file, no worker token: an empty bearer is nobody.
    let h = setup();
    assert!(h.app.worker_token.is_none());
    assert_eq!(
        h.get("/api/ping")
            .header("authorization", "Bearer ")
            .send()
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn the_hook_door_is_for_the_box_itself() {
    let h = setup();
    let body = json!({"agent": "claude", "event": "Stop", "sid": "01HX", "session_id": "abc"});
    let hook = |b: &Value| h.post("/internal/hook", &b.to_string());
    // From the box: delivered to the core, whole.
    let r = hook(&body).peer("127.0.0.1:5555").send().await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(
        h.core.hooks(),
        vec![(femme_proto::state::Agent::Claude, body.clone())]
    );
    assert_eq!(hook(&body).peer("[::1]:5555").send().await.status, 200);
    assert_eq!(
        hook(&body)
            .peer("[::ffff:127.0.0.1]:5555")
            .send()
            .await
            .status,
        200
    );
    let delivered = h.core.hooks().len();
    // Not from the box, or through a proxy on it, or with no peer at all: refused, nothing delivered.
    for r in [
        hook(&body).peer("10.0.0.5:5555").send().await,
        hook(&body).send().await,
        hook(&body)
            .peer("127.0.0.1:5555")
            .header("x-forwarded-for", "1.2.3.4")
            .send()
            .await,
        hook(&body)
            .peer("127.0.0.1:5555")
            .header("x-real-ip", "1.2.3.4")
            .send()
            .await,
        hook(&body)
            .peer("127.0.0.1:5555")
            .header("forwarded", "for=1.2.3.4")
            .send()
            .await,
    ] {
        assert_eq!(
            (r.status, r.error().as_str()),
            (403, "this route is for the box itself")
        );
    }
    assert_eq!(h.core.hooks().len(), delivered);
    // Checked: the agent, the event, the sid.
    for (bad, msg) in [
        (
            json!({"agent": "shell", "event": "x", "sid": "s"}),
            "agent: claude, codex or pi",
        ),
        (
            json!({"agent": "gpt", "event": "x", "sid": "s"}),
            "agent: claude, codex or pi",
        ),
        (
            json!({"event": "x", "sid": "s"}),
            "agent: claude, codex or pi",
        ),
        (
            json!({"agent": "pi", "sid": "s"}),
            "event and sid are required",
        ),
        (
            json!({"agent": "pi", "event": "x", "sid": ""}),
            "event and sid are required",
        ),
    ] {
        let r = hook(&bad).peer("127.0.0.1:5555").send().await;
        assert_eq!((r.status, r.error()), (400, msg.to_string()), "{bad}");
    }
    let r = h
        .post("/internal/hook", "nope")
        .peer("127.0.0.1:5555")
        .send()
        .await;
    assert_eq!(r.status, 400);
    // The JSON rule applies here too, and a core failure is the usual mapped error.
    let r = h
        .call("POST", "/internal/hook")
        .header("content-type", "text/plain")
        .body("{}")
        .peer("127.0.0.1:5555")
        .send()
        .await;
    assert_eq!(r.status, 415);
    h.core.inject_error("hook", CoreError::NotFound);
    assert_eq!(hook(&body).peer("127.0.0.1:5555").send().await.status, 404);
    // GET is not a hook: it falls through to the sign-in gate like any page.
    assert_eq!(
        h.get("/internal/hook")
            .peer("127.0.0.1:5555")
            .send()
            .await
            .status,
        302
    );
}

// ---- terminals ---------------------------------------------------------------------------------

/// Three sessions as the board saw them in herdr: the box's Claude, Instinct's own (a machine of
/// its own, ids tagged), and a plain shell.
fn terminal_fixture(h: &Harness) -> (femme_proto::Sid, femme_proto::Sid, femme_proto::Sid) {
    let mut claude = FakeCore::session("1");
    claude.label = Some("FEMME EDIT".into());
    claude.cwd = "/home/me/femme-work/FEMME-1".into();
    claude.panes[0].title = "Board: sign-in throttle".into();
    let mut instinct = FakeCore::session("2");
    instinct.machine = Some("instinct".into());
    instinct.name = Some("instinct".into());
    instinct.owner = "key:instinct".into();
    instinct.status = SessionStatus::Working;
    let mut shell = FakeCore::session("3");
    shell.agent = femme_proto::state::Agent::Shell;
    shell.panes[0].kind = femme_proto::state::PaneKind::Shell;
    shell.cwd = "/home/me".into();
    let ids = (claude.sid, instinct.sid, shell.sid);
    for s in [claude, instinct, shell] {
        h.core.add_session(s);
    }
    ids
}

#[tokio::test]
async fn terminals_list_ids_machines_and_folders() {
    let h = setup();
    terminal_fixture(&h);
    fs::create_dir_all(h.home.join("femme-work/FEMME-1")).unwrap();
    fs::create_dir_all(h.home.join("Documents/proj/sub")).unwrap();
    fs::create_dir_all(h.home.join("Documents/.hidden")).unwrap();
    let key = h.key("instinct").await;
    let r = h.get("/api/terminals").bearer(&key).send().await;
    assert_eq!(r.status, 200);
    let v = r.json();
    let ws = v["workspaces"].as_array().unwrap();
    assert_eq!(ws.len(), 3);
    assert_eq!(
        ws[0],
        json!({"workspace_id": "w1", "machine": null, "label": "FEMME EDIT", "status": "idle",
               "panes": [{"pane_id": "w1:p1", "agent": "claude", "status": "idle", "cwd": "/home/me/femme-work/FEMME-1", "title": "Board: sign-in throttle"}]})
    );
    // Instinct's own sessions carry the machine and the tag in every id.
    assert_eq!(ws[1]["workspace_id"], "instinct~w2");
    assert_eq!(ws[1]["machine"], "instinct");
    assert_eq!(ws[1]["label"], "instinct");
    assert_eq!(ws[1]["status"], "working");
    assert_eq!(ws[1]["panes"][0]["pane_id"], "instinct~w2:p1");
    // A shell has no agent and no status.
    assert_eq!(
        (
            &ws[2]["panes"][0]["agent"],
            &ws[2]["panes"][0]["status"],
            &ws[2]["status"]
        ),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    assert_eq!(ws[2]["label"], "me");
    // Folders for the page's composer: home, then what is under Documents (one level down) and femme-work.
    let home = h.home.display().to_string();
    let folders: Vec<&str> = v["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    for want in [
        home.clone(),
        format!("{home}/Documents/.hidden"),
        format!("{home}/Documents/proj"),
        format!("{home}/Documents/proj/sub"),
        format!("{home}/femme-work/FEMME-1"),
    ] {
        // Hidden folders under Documents/<x> are skipped, but the first level is listed as it always was.
        assert!(folders.contains(&want.as_str()), "{want} in {folders:?}");
    }
    assert!(!folders.iter().any(|f| f.contains("/.hidden/")));
    assert_eq!(folders.iter().min(), folders.first(), "sorted");
    // The core failing is a 502, as herdr failing was.
    h.core
        .inject_error("terminals", CoreError::Internal("holder gone".into()));
    let r = h.get("/api/terminals").bearer(&key).send().await;
    assert_eq!((r.status, r.error().as_str()), (502, "holder gone"));
}

#[tokio::test]
async fn a_terminal_reads_as_text_and_as_ansi() {
    let h = setup();
    terminal_fixture(&h);
    h.core
        .set_screen("w1:p1", "plain words", "\u{1b}[1mcoloured words\u{1b}[0m");
    h.core
        .set_screen("instinct~w2:p1", "instinct plain", "instinct ansi");
    let key = h.key("instinct").await;
    // Another agent's key reads text (and may read Instinct's own and the box's panes).
    for (pane, text, status) in [
        ("w1:p1", "plain words", json!("idle")),
        ("instinct~w2:p1", "instinct plain", json!("working")),
        ("w3:p1", "screen of w3:p1", Value::Null),
    ] {
        let r = h
            .get(&format!("/api/terminals/{pane}"))
            .bearer(&key)
            .send()
            .await;
        assert_eq!(
            (r.status, r.json()),
            (200, json!({"status": status, "screen": text})),
            "{pane}"
        );
    }
    // The page's terminal reads colours (owner only).
    let cookie = h.cookie().await;
    let r = h
        .get("/api/terminals/w1:p1/screen")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            200,
            json!({"status": "idle", "screen": "\u{1b}[1mcoloured words\u{1b}[0m"})
        )
    );
    let r = h
        .get("/api/terminals/instinct~w2:p1/screen")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.json()["screen"], "instinct ansi");
    assert_eq!(
        h.get("/api/terminals/w1:p1/screen")
            .bearer(&key)
            .send()
            .await
            .status,
        403
    );
    // Ids that are not panes, and panes that are not there, are 404 and never reach the core.
    for bad in [
        "w9:p1",
        "w1:p9",
        "w1",
        "w9;ls:p1",
        "x1:p1",
        "instinct~w1:p1",
        "laptop~w2:p1",
        "w1:p1:p2",
    ] {
        let r = h
            .get(&format!("/api/terminals/{bad}"))
            .bearer(&key)
            .send()
            .await;
        assert_eq!((r.status, r.error().as_str()), (404, "not found"), "{bad}");
    }
}

#[tokio::test]
async fn prompts_and_keys_reach_the_pane() {
    let h = setup();
    let (claude, _, shell) = terminal_fixture(&h);
    let key = h.key("instinct").await;
    // A whole message; NUL never goes through.
    let r = h
        .post(
            "/api/terminals/w1:p1/prompt",
            &json!({"text": "do\u{0}it"}).to_string(),
        )
        .bearer(&key)
        .send()
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(
        h.core.prompts(),
        vec![("w1:p1".to_string(), "doit".to_string())]
    );
    // The agent is working now, which the status shows; blocked on a question it cannot take another.
    assert_eq!(
        h.get("/api/terminals/w1:p1")
            .bearer(&key)
            .send()
            .await
            .json()["status"],
        "working"
    );
    h.core
        .set_status(claude, SessionStatus::Blocked, Attention::Blocked);
    let r = h
        .post("/api/terminals/w1:p1/prompt", r#"{"text":"again"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (409, "agent_not_ready"));
    // ... but keys can answer it.
    let r = h
        .post(
            "/api/terminals/w1:p1/input",
            &json!({"data": "2\r"}).to_string(),
        )
        .bearer(&key)
        .send()
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(
        h.core.inputs(),
        vec![("w1:p1".to_string(), b"2\r".to_vec())]
    );
    h.post(
        "/api/terminals/w1:p1/input",
        &json!({"data": "a\u{0}b\u{1b}"}).to_string(),
    )
    .bearer(&key)
    .send()
    .await;
    assert_eq!(h.core.inputs()[1].1, b"ab\x1b");
    // A shell has no agent to prompt: 409 as well; it does take keys.
    let r = h
        .post("/api/terminals/w3:p1/prompt", r#"{"text":"ls"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(
        h.post("/api/terminals/w3:p1/input", r#"{"data":"ls\r"}"#)
            .bearer(&key)
            .send()
            .await
            .status,
        200
    );
    let _ = shell;
    // Limits and shapes.
    let long = "x".repeat(100_001);
    for (path, body, msg) in [
        ("prompt", json!({"text": ""}), "text: 1 to 100000 bytes"),
        ("prompt", json!({}), "text: 1 to 100000 bytes"),
        ("prompt", json!({"text": long}), "text: 1 to 100000 bytes"),
        (
            "prompt",
            json!({"text": "\u{0}"}),
            "text: 1 to 100000 bytes",
        ),
        ("input", json!({"data": ""}), "send 1 to 100000 bytes"),
        ("input", json!({"data": long}), "send 1 to 100000 bytes"),
    ] {
        let r = h
            .post(&format!("/api/terminals/w1:p1/{path}"), &body.to_string())
            .bearer(&key)
            .send()
            .await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (400, msg),
            "{path} {body:.40}"
        );
    }
    for path in ["prompt", "input"] {
        let r = h
            .post(
                &format!("/api/terminals/w9:p1/{path}"),
                r#"{"text":"a","data":"a"}"#,
            )
            .bearer(&key)
            .send()
            .await;
        assert_eq!(r.status, 404, "{path} on a pane that is not there");
        let r = h
            .post(&format!("/api/terminals/w1:p1/{path}"), "nope")
            .bearer(&key)
            .send()
            .await;
        assert_eq!((r.status, r.error().as_str()), (400, "invalid JSON"));
    }
    assert_eq!(
        h.core.prompts().len(),
        1,
        "nothing refused reached the agent"
    );
}

#[tokio::test]
async fn terminals_open_and_close() {
    let h = setup();
    let (claude, instinct, _) = terminal_fixture(&h);
    let cookie = h.cookie().await;
    fs::create_dir_all(h.home.join("femme-work/proj")).unwrap();
    let cwd = fs::canonicalize(h.home.join("femme-work/proj"))
        .unwrap()
        .display()
        .to_string();
    let open = |body: Value| {
        h.post("/api/terminals", &body.to_string())
            .cookie(&cookie)
            .send()
    };
    let r = open(json!({"cwd": cwd, "label": "  my label ", "agent": "claude"})).await;
    assert_eq!(r.status, 200);
    let made = h.core.created().pop().unwrap();
    assert_eq!(r.json(), json!({"pane_id": "w11:p1"}));
    assert_eq!(
        (made.cwd.as_str(), made.agent, made.mode),
        (
            cwd.as_str(),
            femme_proto::state::Agent::Claude,
            femme_proto::state::Mode::Tui
        )
    );
    assert_eq!(
        (made.label.as_deref(), made.name, made.owner.as_str()),
        (Some("my label"), None, "board")
    );
    // Anything but claude, codex, pi is a plain shell; no label, no folder: home.
    let r = open(json!({"agent": "bogus"})).await;
    assert_eq!(r.status, 200);
    let made = h.core.created().pop().unwrap();
    assert_eq!(
        (made.agent, made.label, made.cwd),
        (
            femme_proto::state::Agent::Shell,
            None,
            fs::canonicalize(&h.home).unwrap().display().to_string()
        )
    );
    assert_eq!(open(json!({"agent": "pi"})).await.status, 200);
    assert_eq!(
        h.core.created().pop().unwrap().agent,
        femme_proto::state::Agent::Pi
    );
    let long = "l".repeat(100);
    open(json!({"label": long})).await;
    assert_eq!(h.core.created().pop().unwrap().label.unwrap().len(), 60);
    // Folders outside home never open; a failure to open is the core's message with its status.
    for bad in ["/etc", "../..", "/home/nobody/x"] {
        let r = open(json!({"cwd": bad})).await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (400, "folder must exist inside your home directory"),
            "{bad}"
        );
    }
    h.core.inject_error(
        "create_session",
        CoreError::Internal("Claude did not start: no trust".into()),
    );
    let r = open(json!({"agent": "claude"})).await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (502, "Claude did not start: no trust")
    );
    // Close by workspace id, tagged ones included; anything else is 404.
    assert_eq!(
        h.delete("/api/terminals/w1")
            .cookie(&cookie)
            .send()
            .await
            .json(),
        json!({"ok": true})
    );
    assert_eq!(
        h.delete("/api/terminals/instinct~w2")
            .cookie(&cookie)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(h.core.closed(), vec![claude, instinct]);
    for gone in ["w1", "w9", "w1:p1", "instinct~w1", "w2", "nope", "w1;x"] {
        let r = h
            .delete(&format!("/api/terminals/{gone}"))
            .cookie(&cookie)
            .send()
            .await;
        assert_eq!((r.status, r.error().as_str()), (404, "not found"), "{gone}");
    }
    assert_eq!(
        h.delete("/api/terminals/w3")
            .bearer(&h.key("k").await)
            .send()
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn uploads_land_in_the_state_dir_and_old_ones_go() {
    let h = setup();
    let cookie = h.cookie().await;
    let uploads = h.state.join("uploads");
    fs::create_dir_all(&uploads).unwrap();
    let age = |name: &str, days: u64| {
        let f = fs::File::create(uploads.join(name)).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(days * 86400 + 3600))
            .unwrap();
    };
    age("old.png", 7);
    age("older.png", 30);
    age("recent.png", 6);
    let body = json!({"name": "Screenshot 2026-10-20 at 10.01.png", "data": BASE64.encode(b"\x89PNG fake")}).to_string();
    let r = h
        .post("/api/terminals/upload", &body)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200);
    let path = PathBuf::from(r.json()["path"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), uploads);
    let file = path.file_name().unwrap().to_str().unwrap();
    assert!(
        file.len() > 9
            && file.as_bytes()[8] == b'-'
            && file[..8].bytes().all(|b| b.is_ascii_hexdigit())
            && file.ends_with("-Screenshot2026-10-20at10.01.png"),
        "{file}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"\x89PNG fake");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Seven days and older are pruned on the way; newer ones stay.
    assert!(!uploads.join("old.png").exists() && !uploads.join("older.png").exists());
    assert!(uploads.join("recent.png").exists());
    // The directory is made on first use, private.
    fs::remove_dir_all(&uploads).unwrap();
    let r = h
        .post("/api/terminals/upload", r#"{"name":"../../x","data":""}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        fs::metadata(&uploads).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(r.json()["path"].as_str().unwrap().ends_with("-x"));
    // Not base64: refused. Keys cannot upload.
    let r = h
        .post("/api/terminals/upload", r#"{"name":"a","data":"!!!"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (400, "data must be base64"));
    assert_eq!(
        h.post("/api/terminals/upload", &body)
            .bearer(&h.key("k").await)
            .send()
            .await
            .status,
        403
    );
}

// ---- issues ------------------------------------------------------------------------------------

fn issue(id: u64, status: IssueStatus) -> Issue {
    Issue {
        id,
        rev: 1,
        repo: "example-org/femme".into(),
        title: format!("issue {id}"),
        body: String::new(),
        status,
        pr_url: None,
        note: None,
        session: None,
        resume_at: None,
        created: 1_700_000_000_000,
        updated: 1_700_000_500_000,
    }
}

#[tokio::test]
async fn issues_are_created_listed_patched_and_deleted() {
    let h = setup();
    let cookie = h.cookie().await;
    let r = h
        .post(
            "/api/issues",
            r#"{"repo":"example-org/femme","title":"  Fix the form ","body":" details "}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 201);
    let made = r.json();
    assert_eq!(
        (made["id"].as_u64(), made["status"].as_str()),
        (Some(1), Some("open"))
    );
    assert_eq!(
        (
            made["repo"].as_str(),
            made["title"].as_str(),
            made["body"].as_str()
        ),
        (
            Some("example-org/femme"),
            Some("Fix the form"),
            Some("details")
        )
    );
    // Seconds, like the board always sent them.
    assert!(
        made["created"].as_u64().unwrap() < 100_000_000_000
            && made["created"].as_u64().unwrap() > 1_700_000_000
    );
    assert!(
        made.get("note").is_none() && made.get("pr_url").is_none() && made.get("pane").is_none()
    );
    h.post(
        "/api/issues",
        r#"{"repo":"example-org/femme","title":"second"}"#,
    )
    .cookie(&cookie)
    .send()
    .await;
    let list = h.get("/api/issues").cookie(&cookie).send().await.json();
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    // Patch: the page's own PATCHes (status and note), and the worker's.
    let r = h
        .patch(
            "/api/issues/1",
            r#"{"status":"waiting","note":"Claude is asking for your approval: open Chat"}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        (r.json()["status"].clone(), r.json()["note"].clone()),
        (
            json!("waiting"),
            json!("Claude is asking for your approval: open Chat")
        )
    );
    let r = h
        .patch(
            "/api/issues/1",
            r#"{"status":"pr","pr_url":"https://github.com/example-org/femme/pull/3","note":""}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    let v = r.json();
    assert_eq!(
        (v["status"].as_str(), v["pr_url"].as_str(), v.get("note")),
        (
            Some("pr"),
            Some("https://github.com/example-org/femme/pull/3"),
            None
        ),
        "an empty note clears it"
    );
    // pane is accepted from old clients and ignored (the daemon links sessions itself); resume_at
    // is kept, in seconds on the wire.
    let r = h
        .patch(
            "/api/issues/2",
            r#"{"pane":"w1:p1","resume_at":1700000000,"title":"renamed"}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()["title"].clone()),
        (200, json!("renamed"))
    );
    assert!(r.json().get("pane").is_none());
    assert_eq!(r.json()["resume_at"], json!(1700000000));
    // Delete.
    assert_eq!(
        h.delete("/api/issues/2")
            .cookie(&cookie)
            .send()
            .await
            .json(),
        json!({"ok": true})
    );
    assert_eq!(
        h.get("/api/issues")
            .cookie(&cookie)
            .send()
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let r = h.delete("/api/issues/2").cookie(&cookie).send().await;
    assert_eq!((r.status, r.error().as_str()), (404, "not found"));
}

#[tokio::test]
async fn issue_input_is_checked_before_the_core_sees_it() {
    let h = setup();
    let cookie = h.cookie().await;
    h.core.add_issue(issue(1, IssueStatus::Open));
    for (body, msg) in [
        (r#"{"repo":"a/b"}"#, "repo and title are required"),
        (r#"{"title":"t"}"#, "repo and title are required"),
        (
            r#"{"repo":"a/b","title":""}"#,
            "repo and title are required",
        ),
        (
            r#"{"repo":"../x","title":"t"}"#,
            "repo must look like owner/name",
        ),
        (
            r#"{"repo":"a/b","title":"t","status":"hacked"}"#,
            "status must be one of open, working, waiting, paused, pr, done, failed",
        ),
        (r#"{"repo":"a/b","title":"t","id":9}"#, "unknown field id"),
        (r#"{"repo":"a/b","title":5}"#, "title must be a string"),
        (r#"[1]"#, "expected a JSON object"),
    ] {
        let r = h.post("/api/issues", body).cookie(&cookie).send().await;
        assert_eq!((r.status, r.error().as_str()), (400, msg), "{body}");
    }
    for (body, msg) in [
        (
            r#"{"status":"triage"}"#,
            "status must be one of open, working, waiting, paused, pr, done, failed",
        ),
        (r#"{"repo":"x"}"#, "repo must look like owner/name"),
        (r#"{"resume_at":"soon"}"#, "resume_at must be unix seconds"),
        (r#"{"id":5}"#, "unknown field id"),
    ] {
        let r = h.patch("/api/issues/1", body).cookie(&cookie).send().await;
        assert_eq!((r.status, r.error().as_str()), (400, msg), "{body}");
    }
    // The board looked the issue up before reading the body: a missing one is a 404 even with bad
    // input; ids are the number's own spelling.
    assert_eq!(
        h.patch("/api/issues/99", r#"{"status":"nope"}"#)
            .cookie(&cookie)
            .send()
            .await
            .status,
        404
    );
    for id in ["99", "01", "+1", "x", "1.0", "-1"] {
        assert_eq!(
            h.patch(&format!("/api/issues/{id}"), r#"{"note":"n"}"#)
                .cookie(&cookie)
                .send()
                .await
                .status,
            404,
            "{id}"
        );
        assert_eq!(
            h.delete(&format!("/api/issues/{id}"))
                .cookie(&cookie)
                .send()
                .await
                .status,
            404,
            "{id}"
        );
    }
    assert_eq!(
        h.patch("/api/issues/1", "nope")
            .cookie(&cookie)
            .send()
            .await
            .error(),
        "invalid JSON"
    );
    // Nothing above changed the issue.
    assert_eq!(
        h.core.issues().await.unwrap()[0],
        issue(1, IssueStatus::Open)
    );
}

#[tokio::test]
async fn issue_conflicts_and_failures_are_the_cores_status() {
    let h = setup();
    let cookie = h.cookie().await;
    h.core.add_issue(issue(1, IssueStatus::Open));
    h.core.inject_error(
        "issue_patch",
        CoreError::Conflict("status changed under you: working".into()),
    );
    let r = h
        .patch("/api/issues/1", r#"{"status":"done"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (409, "status changed under you: working")
    );
    h.core
        .inject_error("issue_create", CoreError::Internal("store failed".into()));
    let r = h
        .post("/api/issues", r#"{"repo":"a/b","title":"t"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (502, "store failed"));
    h.core
        .inject_error("issues", CoreError::Internal("store failed".into()));
    assert_eq!(
        h.get("/api/issues").cookie(&cookie).send().await.status,
        502
    );
    h.core
        .inject_error("issue_delete", CoreError::Conflict("busy".into()));
    assert_eq!(
        h.delete("/api/issues/1")
            .cookie(&cookie)
            .send()
            .await
            .status,
        409
    );
    // A client that sets more than repo, title and body at creation gets it, as a patch after.
    let r = h
        .post(
            "/api/issues",
            r#"{"repo":"a/b","title":"t","status":"paused","note":"later","status":"paused"}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (
            r.status,
            r.json()["status"].clone(),
            r.json()["note"].clone()
        ),
        (201, json!("paused"), json!("later"))
    );
}

#[tokio::test]
async fn an_issue_knows_the_pane_working_on_it() {
    let h = setup();
    let cookie = h.cookie().await;
    let mut s = FakeCore::session("5");
    s.label = Some("#7 something".into());
    let sid = s.sid;
    h.core.add_session(s);
    let mut working = issue(7, IssueStatus::Working);
    working.session = Some(sid);
    working.resume_at = Some(1_700_000_900_000);
    working.note = Some("n".into());
    h.core.add_issue(working);
    h.core.add_issue(issue(8, IssueStatus::Open));
    let list = h.get("/api/issues").cookie(&cookie).send().await.json();
    assert_eq!(list[0]["pane"], "w5:p1");
    assert_eq!(
        (
            list[0]["created"].clone(),
            list[0]["updated"].clone(),
            list[0]["resume_at"].clone()
        ),
        (
            json!(1_700_000_000),
            json!(1_700_000_500),
            json!(1_700_000_900)
        )
    );
    assert_eq!(list[0]["note"], "n");
    assert!(list[1].get("pane").is_none());
    // A session that is gone leaves no pane (the page then shows no Chat button).
    h.core.close_session(sid).await.unwrap();
    assert!(h.get("/api/issues").cookie(&cookie).send().await.json()[0]
        .get("pane")
        .is_none());
}

// ---- keys --------------------------------------------------------------------------------------

#[tokio::test]
async fn keys_are_made_listed_and_revoked() {
    let h = setup();
    let cookie = h.cookie().await;
    let r = h
        .post("/api/keys", r#"{"name":" instinct "}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 201);
    let made = r.json();
    let secret = made["key"].as_str().unwrap().to_string();
    assert!(secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(made["name"], "instinct");
    let entry = &made["keys"][0];
    assert_eq!(
        (
            entry["name"].clone(),
            entry["hint"].clone(),
            entry["account"].clone()
        ),
        (json!("instinct"), json!(secret[60..]), json!("the box"))
    );
    assert!(
        entry["github"].is_null()
            && entry["error"].is_null()
            && entry["created"].as_u64().unwrap() > 1_700_000_000
    );
    // Listed without the secret; the key works as a bearer at once.
    let listed = h.get("/api/keys").cookie(&cookie).send().await;
    assert!(!listed.text().contains(&secret));
    assert_eq!(h.get("/api/ping").bearer(&secret).send().await.status, 200);
    // Names are plain and unique.
    let r = h
        .post("/api/keys", r#"{"name":"instinct"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (409, "a key with that name exists: revoke it first")
    );
    for bad in ["", "Bad Name", "-x", "a/b", "x".repeat(33).as_str()] {
        let r = h
            .post("/api/keys", &json!({"name": bad}).to_string())
            .cookie(&cookie)
            .send()
            .await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (400, "name: a-z, 0-9 and -, up to 32 characters"),
            "{bad:?}"
        );
    }
    // The core refusing a duplicate that raced past the check is the same answer.
    h.core
        .inject_error("api_key_create", CoreError::Conflict("exists".into()));
    assert_eq!(
        h.post("/api/keys", r#"{"name":"other"}"#)
            .cookie(&cookie)
            .send()
            .await
            .status,
        409
    );
    // Revoke: gone at once, 404 the second time.
    let r = h.delete("/api/keys/instinct").cookie(&cookie).send().await;
    assert_eq!((r.status, r.json()), (200, json!([])));
    assert_eq!(h.get("/api/ping").bearer(&secret).send().await.status, 401);
    assert_eq!(
        h.delete("/api/keys/instinct")
            .cookie(&cookie)
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn a_key_can_carry_a_github_account() {
    let h = setup();
    let cookie = h.cookie().await;
    h.tool(
        "gh",
        r#"case "$1 $2" in
  "api user") [ "$GH_TOKEN" = "ghp_goodtokengoodtokengood" ] && echo '{"login":"octocat","name":"Octo Cat","id":583231}' || { echo 'HTTP 401' >&2; exit 1; } ;;
  *) exit 1 ;;
esac"#,
    );
    // A token typed in: its owner is asked of GitHub; name and noreply address come from there.
    let r = h
        .post(
            "/api/keys",
            r#"{"name":"onur","github_token":" ghp_goodtokengoodtokengood "}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["keys"][0]["account"], "Octo Cat (octocat)");
    let key = h.core.api_keys().await.unwrap().pop().unwrap();
    assert_eq!(
        (
            key.github_token.as_deref(),
            key.github_login.as_deref(),
            key.git_name.as_deref(),
            key.git_email.as_deref()
        ),
        (
            Some("ghp_goodtokengoodtokengood"),
            Some("octocat"),
            Some("Octo Cat"),
            Some("583231+octocat@users.noreply.github.com")
        )
    );
    assert!(
        !r.text().contains("ghp_goodtoken"),
        "the token is never sent back"
    );
    // A token GitHub does not know, or one that is not shaped like a token: no key is made.
    for (token, msg) in [
        (
            "ghp_badtokenbadtokenbadtok",
            "GitHub did not accept that token",
        ),
        (
            "short",
            "github_token: a GitHub token is letters, digits and _",
        ),
        (
            "ghp bad token with spaces!",
            "github_token: a GitHub token is letters, digits and _",
        ),
    ] {
        let r = h
            .post(
                "/api/keys",
                &json!({"name": "other", "github_token": token}).to_string(),
            )
            .cookie(&cookie)
            .send()
            .await;
        assert_eq!((r.status, r.error().as_str()), (400, msg), "{token}");
    }
    assert_eq!(h.core.api_keys().await.unwrap().len(), 1);
    // A token without a GitHub name shows its login; a key with a token and no name info says so.
    let k = ApiKey {
        name: "bare".into(),
        key: random_hex(),
        github_token: Some("ghp_x".into()),
        ..Default::default()
    };
    h.core.api_key_create(k).await.unwrap();
    let listed = h.get("/api/keys").cookie(&cookie).send().await.json();
    assert_eq!(listed[1]["account"], "its own token");
}

#[tokio::test]
async fn a_github_sign_in_gives_the_key_its_account() {
    let h = setup();
    let cookie = h.cookie().await;
    let done = h.state.join("gh-done");
    let fail = h.state.join("gh-fail");
    h.tool(
        "gh",
        &format!(
            r#"case "$1 $2" in
  "auth login") echo '! First copy your one-time code: 24E9-601B'; echo 'Open this URL to continue in your web browser: https://github.com/login/device'
     while [ ! -e '{done}' ]; do sleep 0.05; done; [ -e '{fail}' ] && {{ echo 'error: authorization expired'; exit 1; }}; exit 0 ;;
  "auth token") echo ghp_devicetokendevicetoken ;;
  "api user") echo '{{"login":"octocat","name":"","id":5}}' ;;
  *) exit 1 ;;
esac"#,
            done = done.display(),
            fail = fail.display()
        ),
    );
    h.post("/api/keys", r#"{"name":"onur"}"#)
        .cookie(&cookie)
        .send()
        .await;
    // No such key, no sign-in.
    assert_eq!(
        h.post("/api/keys/nobody/github", "{}")
            .cookie(&cookie)
            .send()
            .await
            .status,
        404
    );
    // Start: the one-time code shows in the key's entry.
    let r = h
        .post("/api/keys/onur/github", "{}")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()[0]["github"],
        json!({"code": "24E9-601B", "url": "https://github.com/login/device"})
    );
    assert_eq!(
        h.get("/api/keys").cookie(&cookie).send().await.json()[0]["github"]["code"],
        "24E9-601B"
    );
    // The owner enters the code at GitHub; gh ends; the next listing puts the account on the key.
    fs::write(&done, "").unwrap();
    let mut account = Value::Null;
    for _ in 0..100 {
        let listed = h.get("/api/keys").cookie(&cookie).send().await.json();
        account = listed[0]["account"].clone();
        if account != "the box" {
            assert!(listed[0]["github"].is_null());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        account, "octocat (octocat)",
        "a GitHub account without a name is shown by its login"
    );
    let key = h.core.api_keys().await.unwrap().pop().unwrap();
    assert_eq!(
        (key.github_token.as_deref(), key.git_email.as_deref()),
        (
            Some("ghp_devicetokendevicetoken"),
            Some("5+octocat@users.noreply.github.com")
        )
    );
    // A sign-in that fails leaves its reason on the key until the next attempt.
    fs::remove_file(&done).unwrap();
    fs::write(&fail, "").unwrap();
    h.post("/api/keys/onur/github", "{}")
        .cookie(&cookie)
        .send()
        .await;
    fs::write(&done, "").unwrap();
    let mut error = Value::Null;
    for _ in 0..100 {
        error = h.get("/api/keys").cookie(&cookie).send().await.json()[0]["error"].clone();
        if !error.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(error, "error: authorization expired");
    // Cancelling ends a sign-in in progress.
    fs::remove_file(&done).unwrap();
    assert_eq!(
        h.post("/api/keys/onur/github", "{}")
            .cookie(&cookie)
            .send()
            .await
            .json()[0]["error"],
        Value::Null
    );
    let r = h
        .delete("/api/keys/onur/github")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()[0]["github"].clone()),
        (200, Value::Null)
    );
    // gh failing to start at all is a 502 with its own words.
    h.tool("gh", "echo 'gh: not logged in' ; exit 1");
    let r = h
        .post("/api/keys/onur/github", "{}")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (502, "gh: gh: not logged in")
    );
}

// ---- agents ------------------------------------------------------------------------------------

/// Instinct's standing Claude as the core would list it: headless, in the `instinct` machine.
fn instinct_session(h: &Harness, name: &str, handle: &str) -> femme_proto::Sid {
    let mut s = FakeCore::session(handle);
    s.machine = Some("instinct".into());
    s.name = Some(name.into());
    s.owner = "key:instinct".into();
    s.mode = femme_proto::state::Mode::Headless;
    s.cwd = h.home.join("femme-work").join(name).display().to_string();
    let sid = s.sid;
    h.core.add_session(s);
    sid
}

const STOPPED: &str = "changes ready, not pushed to main: read the report, then POST /api/agents/instinct/push or /discard";

#[tokio::test]
async fn a_task_goes_from_send_to_report_to_push() {
    let h = setup();
    let key = h.key("instinct").await;
    let dir = h.clone_dir("instinct");
    let sid = instinct_session(&h, "instinct", "7");
    let report = h.state.join("report-instinct.md");
    let remind = format!(
        "When done, write the report to {} (overwrite it), as briefed.",
        report.display()
    );
    // Send: the task reaches the pane with the report reminder, and the session is working.
    let body = json!({"text": "fix the form", "repo": "example-org/example-repo"}).to_string();
    let r = h
        .post("/api/agents/instinct", &body)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            200,
            json!({"pane_id": "instinct~w7:p1", "status": "working"})
        )
    );
    assert_eq!(
        h.core.prompts(),
        vec![("instinct~w7:p1".to_string(), format!("fix the form\n\n{remind} Do not commit or push: that comes as a separate push request."))]
    );
    // Working: nothing to say about the outcome yet.
    let g = h
        .get("/api/agents/instinct")
        .bearer(&key)
        .send()
        .await
        .json();
    assert_eq!(
        g,
        json!({"pane_id": "instinct~w7:p1", "status": "working", "message": null, "pushed": false, "screen": "screen of instinct~w7:p1", "report": null})
    );
    // Claude stops with changes in the tree and a report written: the owner is asked.
    h.core.set_status(sid, SessionStatus::Idle, Attention::Done);
    fs::write(&report, "  Form fixed.\n- checked\n  ").unwrap();
    sh(&dir, "echo x > f");
    let g = h
        .get("/api/agents/instinct")
        .bearer(&key)
        .send()
        .await
        .json();
    assert_eq!(
        (
            g["status"].as_str(),
            g["message"].as_str(),
            g["pushed"].as_bool()
        ),
        (Some("done"), Some(STOPPED), Some(false))
    );
    assert_eq!(g["report"], "Form fixed.\n- checked");
    // The owner says yes, with a note: the push request goes in, the old report goes, and no "do not push" rides along.
    let r = h
        .post("/api/agents/instinct/push", r#"{"text":"ship it"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            200,
            json!({"pane_id": "instinct~w7:p1", "status": "working"})
        )
    );
    let sent = h.core.prompts().pop().unwrap().1;
    assert_eq!(
        sent,
        format!("Push request from the owner. Commit every change from the last task as one commit whose message says what changed and why, then git pull --rebase origin main && git push origin main. If there is nothing to commit, say so and push nothing.\n\nNote from the owner:\nship it\n\n{remind}")
    );
    assert!(
        !report.exists(),
        "the last report goes when a message is sent"
    );
    assert_eq!(
        h.get("/api/agents/instinct")
            .bearer(&key)
            .send()
            .await
            .json()["report"],
        Value::Null
    );
    // Claude pushed: clean tree, HEAD on its remote branch, HEAD moved since the task was sent.
    sh(&dir, "git add f && git -c user.name=t -c user.email=t@t commit -q -m task && git update-ref refs/remotes/origin/main HEAD");
    let head = agents::git(h.app.cfg(), &dir, &["rev-parse", "HEAD"]).unwrap();
    h.core.set_status(sid, SessionStatus::Idle, Attention::Done);
    let g = h
        .get("/api/agents/instinct")
        .bearer(&key)
        .send()
        .await
        .json();
    assert_eq!(
        (
            g["message"].as_str(),
            g["pushed"].as_bool(),
            g["commit"].as_str()
        ),
        (
            Some("changes created and pushed to example-repo"),
            Some(true),
            Some(head.as_str())
        )
    );
    // The next task changes nothing: finished, nothing new.
    h.post("/api/agents/instinct", r#"{"text":"look again"}"#)
        .bearer(&key)
        .send()
        .await;
    h.core.set_status(sid, SessionStatus::Idle, Attention::Done);
    let g = h
        .get("/api/agents/instinct")
        .bearer(&key)
        .send()
        .await
        .json();
    assert_eq!(
        (g["message"].as_str(), g["pushed"].as_bool()),
        (
            Some("finished, nothing new to push: read the screen"),
            Some(false)
        )
    );
    // The owner says no.
    let r = h
        .post("/api/agents/instinct/discard", "{}")
        .bearer(&key)
        .send()
        .await;
    assert_eq!(r.status, 200);
    let sent = h.core.prompts().pop().unwrap().1;
    assert_eq!(
        sent,
        format!("Discard request from the owner. Throw away every uncommitted change from the last task (git checkout -- . && git clean -fd) and say so.\n\n{remind} Do not commit or push: that comes as a separate push request.")
    );
    // Close: the session goes, the clone stays.
    let r = h.delete("/api/agents/instinct").bearer(&key).send().await;
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(h.core.closed(), vec![sid]);
    assert!(dir.join(".git").is_dir());
    let r = h.get("/api/agents/instinct").bearer(&key).send().await;
    assert_eq!((r.status, r.error().as_str()), (404, "not running"));
}

#[tokio::test]
async fn a_session_waiting_on_a_question_is_told_so() {
    let h = setup();
    let key = h.key("instinct").await;
    h.clone_dir("instinct");
    let sid = instinct_session(&h, "instinct", "7");
    h.core
        .set_status(sid, SessionStatus::Blocked, Attention::Blocked);
    let g = h
        .get("/api/agents/instinct")
        .bearer(&key)
        .send()
        .await
        .json();
    assert_eq!(
        (g["status"].as_str(), g["message"].as_str()),
        (
            Some("blocked"),
            Some("waiting for an answer: read the screen")
        )
    );
    // A task now is 409 with the pane, as is a verdict; the screen is how to see the question.
    let r = h
        .post("/api/agents/instinct", r#"{"text":"more"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            409,
            json!({"error": "agent_not_ready", "pane_id": "instinct~w7:p1"})
        )
    );
    assert_eq!(
        h.post("/api/agents/instinct/push", "{}")
            .bearer(&key)
            .send()
            .await
            .status,
        409
    );
    assert!(h.core.prompts().is_empty());
    // A screen the core cannot read is an empty one, not an error.
    h.core
        .inject_error("screen", CoreError::Internal("gone".into()));
    let g = h.get("/api/agents/instinct").bearer(&key).send().await;
    assert_eq!((g.status, g.json()["screen"].clone()), (200, json!("")));
}

fn fake_gh_clone(h: &Harness) {
    h.tool("gh", r#"case "$1 $2" in
  "repo clone") [ "$3" = example-org/broken ] && exit 1
    git init -q -b main "$4" && git -C "$4" -c user.name=t -c user.email=t@t commit -q --allow-empty -m init && git -C "$4" remote add origin "https://github.com/$3.git" ;;
  *) exit 1 ;;
esac"#);
}

#[tokio::test]
async fn a_first_task_opens_the_session_with_its_brief() {
    let h = setup();
    fake_gh_clone(&h);
    let key = h.key("instinct").await;
    let report = h.state.join("report-newbie.md");
    let body =
        json!({"text": "hello", "repo": "example-org/example-repo", "push": "develop"}).to_string();
    let r = h
        .post("/api/agents/newbie", &body)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            200,
            json!({"pane_id": "instinct~w11:p1", "status": "working"})
        ),
        "{}",
        r.text()
    );
    // Cloned to femme-work/<name> by the box's gh, then opened headless under that name, owned by the key.
    let dir = h.home.join("femme-work/newbie");
    assert!(dir.join(".git").is_dir());
    assert!(
        h.tool_log().contains(&format!(
            "gh repo clone example-org/example-repo {} -- --quiet",
            dir.display()
        )),
        "{}",
        h.tool_log()
    );
    let made = h.core.created();
    assert_eq!(made.len(), 1);
    let made = &made[0];
    assert_eq!(
        (made.cwd.as_str(), made.agent, made.mode),
        (
            dir.to_str().unwrap(),
            femme_proto::state::Agent::Claude,
            femme_proto::state::Mode::Headless
        )
    );
    assert_eq!(
        (
            made.label.as_deref(),
            made.name.as_deref(),
            made.owner.as_str()
        ),
        (Some("newbie"), Some("newbie"), "key:instinct")
    );
    // The standing brief goes in front of the first message, with the repo and branch.
    let sent = h.core.prompts().pop().unwrap().1;
    let head = "This terminal is newbie's standing Claude Code session on the FEMME box: every message is a new task, and the session is kept between them. The repo example-org/example-repo is cloned here on branch develop. For each task: do it end to end and run the project's checks and tests, but do not commit and do not push: leave the changes in the working tree, the owner reads the report first and then sends a push request or a discard request. Do not open PRs. If a decision is only the owner's to make, say so plainly and stop. Write everything for a reader with ADHD:";
    assert!(sent.starts_with(head), "{sent}");
    assert!(sent.ends_with(&format!("Nothing else.\n\nTask:\nhello\n\nWhen done, write the report to {} (overwrite it), as briefed. Do not commit or push: that comes as a separate push request.", report.display())), "{sent}");
    // The next message goes to the session that is now running: no clone, no new session, no brief.
    let r = h
        .post("/api/agents/newbie", r#"{"text":"second"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(h.core.created().len(), 1);
    assert!(h.core.prompts().pop().unwrap().1.starts_with("second\n\n"));
    // The owner can use the same route; the session is then the board's own.
    let cookie = h.cookie().await;
    let r = h
        .post(
            "/api/agents/other",
            r#"{"text":"hi","repo":"example-org/example-repo"}"#,
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(h.core.created().pop().unwrap().owner, "board");
}

#[tokio::test]
async fn a_session_can_open_in_a_folder_instead_of_a_repo() {
    let h = setup();
    let key = h.key("instinct").await;
    fs::create_dir_all(h.home.join("femme-work/scratch")).unwrap();
    let r = h
        .post(
            "/api/agents/scratch",
            &json!({"text": "hi", "cwd": h.home.join("femme-work/scratch")}).to_string(),
        )
        .bearer(&key)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let made = h.core.created().pop().unwrap();
    assert_eq!(
        made.cwd,
        fs::canonicalize(h.home.join("femme-work/scratch"))
            .unwrap()
            .display()
            .to_string()
    );
    let sent = h.core.prompts().pop().unwrap().1;
    assert!(
        !sent.contains("The repo "),
        "no repo, no repo sentence: {sent}"
    );
    assert!(
        !sent.contains("Do not commit or push"),
        "no clone there, nothing to guard: {sent}"
    );
    // No folder: home. A folder outside home: no.
    assert_eq!(
        h.post("/api/agents/home", r#"{"text":"hi"}"#)
            .bearer(&key)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        h.core.created().pop().unwrap().cwd,
        fs::canonicalize(&h.home).unwrap().display().to_string()
    );
    let r = h
        .post("/api/agents/out", r#"{"text":"hi","cwd":"/etc"}"#)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (
            400,
            "cwd must be an existing folder inside home (or give repo)"
        )
    );
}

#[tokio::test]
async fn a_failed_open_leaves_nothing_running() {
    let h = setup();
    fake_gh_clone(&h);
    let key = h.key("instinct").await;
    let r = h
        .post(
            "/api/agents/newbie",
            r#"{"text":"hi","repo":"example-org/broken"}"#,
        )
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (502, "could not clone example-org/broken")
    );
    assert!(h.core.created().is_empty() && h.core.prompts().is_empty());
    // Claude not starting is the core's message, as a 502.
    h.core.inject_error(
        "create_session",
        CoreError::Internal("Claude did not start: trust question unanswered".into()),
    );
    let r = h
        .post(
            "/api/agents/newbie",
            r#"{"text":"hi","repo":"example-org/example-repo"}"#,
        )
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (502, "Claude did not start: trust question unanswered")
    );
    assert!(h.core.prompts().is_empty());
}

#[tokio::test]
async fn a_key_with_a_github_account_clones_and_commits_as_it() {
    let h = setup();
    // git itself is real; a clone is the one thing that must not touch the network.
    h.tool(
        "git",
        r#"if [ "$1" = clone ]; then /usr/bin/git init -q -b main "$4" && /usr/bin/git -C "$4" remote add origin "$3"; exit $?; fi
exec /usr/bin/git "$@""#,
    );
    let key = ApiKey {
        name: "onur".into(),
        key: random_hex(),
        github_token: Some("ghp_exampletokenexampletoken".into()),
        github_login: Some("onur".into()),
        git_name: Some("Onur".into()),
        git_email: Some("1+onur@users.noreply.github.com".into()),
        created: now_ms(),
    };
    h.core.api_key_create(key.clone()).await.unwrap();
    let body = r#"{"text":"hi","repo":"example-org/example-repo"}"#;
    let dir = h.home.join("femme-work/onurs");
    let r = h
        .post("/api/agents/onurs", body)
        .bearer(&key.key)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let url = "https://x-access-token:ghp_exampletokenexampletoken@github.com/example-org/example-repo.git";
    assert!(
        h.tool_log()
            .contains(&format!("git clone -q {url} {}", dir.display())),
        "{}",
        h.tool_log()
    );
    let git = |args: &[&str]| agents::git(h.app.cfg(), &dir, args).unwrap();
    assert_eq!(git(&["remote", "get-url", "origin"]), url);
    assert_eq!(
        (
            git(&["config", "user.name"]).as_str(),
            git(&["config", "user.email"]).as_str()
        ),
        ("Onur", "1+onur@users.noreply.github.com")
    );
    assert_eq!(h.core.created().pop().unwrap().owner, "key:onur");
    // A key without an account hands the clone back to the box's gh: the earlier token leaves the remote.
    let plain = h.key("plain").await;
    h.core
        .close_session(h.core.session_by_name("onurs").await.unwrap().unwrap().sid)
        .await
        .unwrap();
    let r = h
        .post("/api/agents/onurs", body)
        .bearer(&plain)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        git(&["remote", "get-url", "origin"]),
        "https://github.com/example-org/example-repo.git"
    );
}

#[tokio::test]
async fn agent_input_is_checked() {
    let h = setup();
    let key = h.key("instinct").await;
    let name_rule = "name: a-z, 0-9 and -, up to 32 characters";
    for bad in ["Bad", "-x", "a_b", &"x".repeat(33)] {
        for (method, suffix) in [
            ("GET", ""),
            ("POST", ""),
            ("DELETE", ""),
            ("POST", "/push"),
            ("POST", "/discard"),
        ] {
            let mut c = h
                .call(method, &format!("/api/agents/{bad}{suffix}"))
                .bearer(&key);
            if method == "POST" {
                c = c
                    .header("content-type", "application/json")
                    .body(r#"{"text":"x"}"#);
            }
            let r = c.send().await;
            assert_eq!(
                (r.status, r.error().as_str()),
                (400, name_rule),
                "{method} {bad}{suffix}"
            );
        }
    }
    let post = |name: &str, body: Value| {
        h.post(&format!("/api/agents/{name}"), &body.to_string())
            .bearer(&key)
            .send()
    };
    let r = post("ok", json!({})).await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "text: 1 to 100000 bytes")
    );
    assert_eq!(post("ok", json!({"text": ""})).await.status, 400);
    assert_eq!(post("ok", json!({"text": "\u{0}"})).await.status, 400);
    assert_eq!(
        post("ok", json!({"text": "x".repeat(100_001)}))
            .await
            .status,
        400
    );
    let r = post("ok", json!({"text": "x", "push": "--force", "repo": "a/b"})).await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "push must be a branch name, like main")
    );
    let r = post("ok", json!({"text": "x", "repo": "../x"})).await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "repo must look like owner/name")
    );
    let r = h.post("/api/agents/ok", "{").bearer(&key).send().await;
    assert_eq!((r.status, r.error().as_str()), (400, "invalid JSON"));
    assert!(h.core.created().is_empty());
    // Nothing running under a name: 404 for reading, closing and the owner's verdicts.
    for (method, suffix) in [
        ("GET", ""),
        ("DELETE", ""),
        ("POST", "/push"),
        ("POST", "/discard"),
    ] {
        let mut c = h
            .call(method, &format!("/api/agents/ghost{suffix}"))
            .bearer(&key);
        if method == "POST" {
            c = c.header("content-type", "application/json").body("{}");
        }
        let r = c.send().await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (404, "not running"),
            "{method} {suffix}"
        );
    }
    // A session that never had a repo cannot push; notes have a size limit.
    instinct_session(&h, "norepo", "8");
    let r = h
        .post("/api/agents/norepo/push", "{}")
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "this session has no repo")
    );
    h.clone_dir("hasrepo");
    instinct_session(&h, "hasrepo", "9");
    let note = json!({"text": "n".repeat(10_001)}).to_string();
    let r = h
        .post("/api/agents/hasrepo/push", &note)
        .bearer(&key)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "text: up to 10000 bytes")
    );
    // A body that is not JSON is no note, not an error (the client's `{}` and empty bodies both work).
    assert_eq!(
        h.post("/api/agents/hasrepo/discard", "")
            .bearer(&key)
            .send()
            .await
            .status,
        200
    );
    // The branch the session is on is the one asked to push.
    sh(
        &h.home.join("femme-work/hasrepo"),
        "git checkout -q -b release/2.1",
    );
    h.post("/api/agents/hasrepo/push", "{}")
        .bearer(&key)
        .send()
        .await;
    assert!(h
        .core
        .prompts()
        .pop()
        .unwrap()
        .1
        .contains("git pull --rebase origin release/2.1 && git push origin release/2.1."));
}

// ---- accounts, meta, openapi -------------------------------------------------------------------

fn fake_account_tools(h: &Harness) {
    h.tool(
        "cswap",
        &format!(
            r#"case "$1" in
  list) echo '[{{"n":1,"email":"a@x.test","active":true}},{{"n":2,"email":"b@x.test"}}]' ;;
  switch) [ "$2" = 2 ] && exit 0; echo '{{"error":{{"message":"no such account"}}}}'; exit 1 ;;
  add) echo "added from $CLAUDE_CONFIG_DIR" >> '{log}' ;;
  *) exit 2 ;;
esac"#,
            log = h.log.display()
        ),
    );
    h.tool("aws", r#"echo '{"Account":"123456789012","Arn":"arn:aws:sts::123456789012:assumed-role/box/i-1"}'"#);
}

#[tokio::test]
async fn the_accounts_window_shows_cswap_and_aws() {
    let h = setup();
    fake_account_tools(&h);
    let cookie = h.cookie().await;
    // Before the first refresh there is only the (empty) login list; a page load never waits on cswap or aws.
    let r = h.get("/api/accounts").cookie(&cookie).send().await;
    assert_eq!((r.status, r.json()), (200, json!({"login": {}})));
    assert!(h.tool_log().is_empty());
    accounts::refresh(&h.app).await;
    let v = h.get("/api/accounts").cookie(&cookie).send().await.json();
    assert_eq!(v["cswap"].as_array().unwrap().len(), 2);
    assert_eq!(v["aws"]["Account"], "123456789012");
    assert_eq!(v["login"], json!({}));
    assert!(h.tool_log().contains(
        "aws sts get-caller-identity --output json --cli-connect-timeout 5 --cli-read-timeout 5"
    ));
    // Keys do not see it.
    assert_eq!(
        h.get("/api/accounts")
            .bearer(&h.key("k").await)
            .send()
            .await
            .status,
        403
    );
    // A broken cswap or aws shows as an error entry, not a failed page.
    h.tool("cswap", "echo 'cswap: no config' >&2; exit 1");
    h.tool("aws", "echo 'Unable to locate credentials' >&2; exit 1");
    accounts::refresh(&h.app).await;
    let v = h.get("/api/accounts").cookie(&cookie).send().await.json();
    assert_eq!(
        (v["cswap"]["error"].as_str(), v["aws"]["error"].as_str()),
        (
            Some("cswap: no config"),
            Some("Unable to locate credentials")
        )
    );
}

#[tokio::test]
async fn switching_accounts() {
    let h = setup();
    fake_account_tools(&h);
    let cookie = h.cookie().await;
    for bad in [
        r#"{"account":""}"#,
        r#"{"account":"x"}"#,
        r#"{"account":"1234"}"#,
        r#"{"account":2}"#,
        "{}",
        r#"{"account":"-1"}"#,
    ] {
        let r = h
            .post("/api/accounts/switch", bad)
            .cookie(&cookie)
            .send()
            .await;
        assert_eq!(
            (r.status, r.error().as_str()),
            (400, "account must be a number"),
            "{bad}"
        );
    }
    assert!(h.tool_log().is_empty(), "nothing invalid reached cswap");
    let r = h
        .post("/api/accounts/switch", r#"{"account":"2"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["cswap"].as_array().unwrap().len(),
        2,
        "the list is read again after the switch"
    );
    assert!(h.tool_log().contains("cswap switch 2 --json"));
    // cswap's own error text comes back.
    let r = h
        .post("/api/accounts/switch", r#"{"account":"9"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (502, "no such account"));
}

#[tokio::test]
async fn a_claude_sign_in_takes_the_code_and_stores_the_account() {
    let h = setup();
    fake_account_tools(&h);
    h.tool(
        "claude",
        &format!(
            r#"[ "$1 $2" = "auth login" ] || exit 2
echo "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=x&state=y"
printf 'Paste code here if prompted > '
read code
if [ "$code" = "abc#def" ]; then echo "$CLAUDE_CONFIG_DIR $BROWSER" > '{state}/claude-saw'; exit 0; fi
echo 'error: invalid code'; exit 1"#,
            state = h.state.display()
        ),
    );
    let cookie = h.cookie().await;
    // No sign-in yet: the code has nowhere to go.
    let r = h
        .post("/api/accounts/claude/code", r#"{"code":"abc#def"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (409, "that sign-in ended; start again")
    );
    // Start: the link shows in the window.
    let r = h
        .post("/api/accounts/claude/login", "{}")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let link = "https://claude.com/cai/oauth/authorize?code=true&client_id=x&state=y";
    assert_eq!(r.json()["login"], json!({"claude": link}));
    assert_eq!(
        h.get("/api/accounts").cookie(&cookie).send().await.json()["login"]["claude"],
        link
    );
    // A code with control characters never reaches it; a wrong code is its own error, and ends the attempt.
    let r = h
        .post(
            "/api/accounts/claude/code",
            &json!({"code": "a\u{1}b"}).to_string(),
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (400, "paste the code exactly as the sign-in page shows it")
    );
    let r = h
        .post("/api/accounts/claude/code", r#"{"code":"wrong"}"#)
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.error().as_str()), (502, "error: invalid code"));
    assert_eq!(
        h.get("/api/accounts").cookie(&cookie).send().await.json()["login"],
        json!({})
    );
    // Again, with the right code (wrapped, as copying it can): cswap add runs in the sign-in's own config dir.
    h.post("/api/accounts/claude/login", "{}")
        .cookie(&cookie)
        .send()
        .await;
    let r = h
        .post(
            "/api/accounts/claude/code",
            &json!({"code": " abc#\ndef "}).to_string(),
        )
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let dir = h.state.join("claude-login");
    assert_eq!(
        fs::read_to_string(h.state.join("claude-saw"))
            .unwrap()
            .trim(),
        format!("{} true", dir.display())
    );
    assert!(
        h.tool_log()
            .contains(&format!("added from {}", dir.display())),
        "{}",
        h.tool_log()
    );
    assert_eq!(r.json()["login"], json!({}));
    assert!(!dir.exists(), "the sign-in's config dir goes with it");
    // A sign-in nobody finished expires with its code; cancelling ends it too.
    h.post("/api/accounts/claude/login", "{}")
        .cookie(&cookie)
        .send()
        .await;
    lock(&h.app.logins).get_mut("claude").unwrap().started = now() - LINK_SECS - 5;
    assert_eq!(
        h.get("/api/accounts").cookie(&cookie).send().await.json()["login"],
        json!({})
    );
    h.post("/api/accounts/claude/login", "{}")
        .cookie(&cookie)
        .send()
        .await;
    let r = h
        .delete("/api/accounts/claude/login")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!((r.status, r.json()["login"].clone()), (200, json!({})));
    // claude not coming up with a link is a 502 with its own words; only claude signs in here.
    h.tool("claude", "echo 'claude: not installed'; exit 1");
    let r = h
        .post("/api/accounts/claude/login", "{}")
        .cookie(&cookie)
        .send()
        .await;
    assert_eq!(
        (r.status, r.error().as_str()),
        (502, "claude: not installed")
    );
    assert_eq!(
        h.post("/api/accounts/gh/login", "{}")
            .cookie(&cookie)
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn meta_shows_the_schedulers_word_and_the_last_deploy() {
    let h = setup();
    let cookie = h.cookie().await;
    h.core.set_meta(
        json!({"state": "idle: queue is empty", "cap": 60, "repos": ["example-org/femme"]}),
    );
    // No deploy.json: deploy is null.
    let v = h.get("/api/meta").cookie(&cookie).send().await.json();
    assert_eq!(
        v,
        json!({"state": "idle: queue is empty", "cap": 60, "repos": ["example-org/femme"], "deploy": null})
    );
    fs::write(
        h.state.join("deploy.json"),
        r#"{"ok":false,"error":"build failed","at":1700000000}"#,
    )
    .unwrap();
    let v = h.get("/api/meta").cookie(&cookie).send().await.json();
    assert_eq!(
        v["deploy"],
        json!({"ok": false, "error": "build failed", "at": 1700000000})
    );
    assert_eq!(
        h.get("/api/meta")
            .bearer(&h.key("k").await)
            .send()
            .await
            .status,
        403
    );
    // Closing it removes the file; closing it twice is fine.
    for _ in 0..2 {
        assert_eq!(
            h.delete("/api/deploy").cookie(&cookie).send().await.json(),
            json!({"ok": true})
        );
    }
    assert!(!h.state.join("deploy.json").exists());
    assert_eq!(
        h.get("/api/meta").cookie(&cookie).send().await.json()["deploy"],
        Value::Null
    );
    // A core with nothing to say, or garbage in the file, is still a page.
    h.core.inject_error("meta", CoreError::Internal("x".into()));
    fs::write(h.state.join("deploy.json"), "not json").unwrap();
    assert_eq!(
        h.get("/api/meta").cookie(&cookie).send().await.json(),
        json!({"deploy": null})
    );
    h.core.set_meta(json!("not an object"));
    assert_eq!(
        h.get("/api/meta").cookie(&cookie).send().await.json(),
        json!({"deploy": null})
    );
}

#[tokio::test]
async fn openapi_is_served_with_the_boards_url() {
    let h = setup();
    let key = h.key("instinct").await;
    let r = h.get("/api/openapi.json").bearer(&key).send().await;
    assert_eq!(
        (r.status, r.header("content-type")),
        (200, "application/json")
    );
    let want = OPENAPI.replace("{BOARD_URL}", "https://femme.example.com");
    assert_eq!(
        r.body,
        want.as_bytes(),
        "served byte for byte, with only the URL filled in"
    );
    assert!(!r.text().contains("{BOARD_URL}"));
    assert_eq!(r.json()["servers"][0]["url"], "https://femme.example.com");
    // The owner reads it too; nobody else does.
    assert_eq!(
        h.get("/api/openapi.json")
            .cookie(&h.cookie().await)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(h.get("/api/openapi.json").send().await.status, 401);
    // ponytail: the board is deleted when femmed replaces it; drop this comparison with it.
    if let Ok(old) = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../board/src/openapi.json"
    )) {
        assert_eq!(
            old, OPENAPI,
            "assets/openapi.json is the board's, byte for byte"
        );
    }
}

// ---- over a real socket ------------------------------------------------------------------------

async fn run(program: &str, args: &[&str], envs: &[(&str, &str)]) -> (i32, String) {
    let out = tokio::process::Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// `examples/api-client.sh` is the documented way to use the agents API from a shell: its whole
/// flow, against the real server on a real socket (which is also where the peer address for the
/// hook door comes from).
#[tokio::test]
async fn the_example_client_flow_works() {
    for tool in ["curl", "jq", "bash"] {
        if !StdCommand::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("skipped: {tool} is not installed");
            return;
        }
    }
    let h = setup();
    let key = h.key("instinct").await;
    let dir = h.clone_dir("instinct");
    let sid = instinct_session(&h, "instinct", "7");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::serve_on(listener, h.core.clone(), async move {
        let _ = stopped.await;
    }));
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/api-client.sh");
    async fn client(script: &str, url: &str, key: &str, args: &[&str]) -> (i32, String) {
        let mut all = vec![script];
        all.extend(args);
        run("bash", &all, &[("FEMME_URL", url), ("FEMME_KEY", key)]).await
    }
    // send: the first call names the repo.
    let (code, out) = client(
        script,
        &url,
        &key,
        &[
            "send",
            "instinct",
            "fix the form",
            "example-org/example-repo",
        ],
    )
    .await;
    assert_eq!((code, out.trim()), (0, "sent; status: working"));
    assert!(h.core.prompts()[0].1.starts_with("fix the form"));
    // wait: once Claude stopped, the outcome and the report.
    h.core.set_status(sid, SessionStatus::Idle, Attention::Done);
    fs::write(h.state.join("report-instinct.md"), "Form fixed.\n- checked").unwrap();
    sh(&dir, "echo x > f");
    let (code, out) = client(script, &url, &key, &["wait", "instinct"]).await;
    assert_eq!(
        (code, out.as_str()),
        (0, format!("{STOPPED}\n\nForm fixed.\n- checked\n").as_str())
    );
    // push and discard: the owner's yes and no.
    let (code, out) = client(script, &url, &key, &["push", "instinct", "ship it"]).await;
    assert_eq!((code, out.trim()), (0, "ok"));
    assert!(h
        .core
        .prompts()
        .pop()
        .unwrap()
        .1
        .contains("Note from the owner:\nship it"));
    let (code, out) = client(script, &url, &key, &["discard", "instinct"]).await;
    assert_eq!((code, out.trim()), (0, "ok"));
    assert!(h
        .core
        .prompts()
        .pop()
        .unwrap()
        .1
        .starts_with("Discard request from the owner."));
    // wait on a question: the screen, and exit 3.
    h.core
        .set_status(sid, SessionStatus::Blocked, Attention::Blocked);
    let (code, out) = client(script, &url, &key, &["wait", "instinct"]).await;
    assert_eq!(code, 3);
    assert!(
        out.starts_with("Claude is waiting for an answer; its screen:\nscreen of instinct~w7:p1"),
        "{out}"
    );
    // close.
    let (code, out) = client(script, &url, &key, &["close", "instinct"]).await;
    assert_eq!((code, out.trim()), (0, "closed"));
    assert_eq!(h.core.closed(), vec![sid]);
    // A wrong key is refused by the real server too.
    let (code, _) = run(
        "bash",
        &[script, "wait", "instinct"],
        &[("FEMME_URL", &url), ("FEMME_KEY", "wrong")],
    )
    .await;
    assert_ne!(code, 0);
    // The hook door, from the box itself: curl on loopback is local; one that says it came through a proxy is not.
    let hook = |extra: &'static [&'static str]| {
        let url = format!("{url}/internal/hook");
        async move {
            let mut args = vec![
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-H",
                "content-type: application/json",
                "-d",
                r#"{"agent":"codex","event":"agent-turn-complete","sid":"s1"}"#,
            ];
            args.extend(extra);
            args.push(&url);
            run("curl", &args, &[]).await.1
        }
    };
    assert_eq!(hook(&[]).await, "200");
    assert_eq!(h.core.hooks().len(), 1);
    assert_eq!(hook(&["-H", "X-Forwarded-For: 203.0.113.9"]).await, "403");
    assert_eq!(h.core.hooks().len(), 1);
    // Shutdown drains and returns.
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server stopped")
        .unwrap()
        .unwrap();
}
