//! M1's day-one gate: what `examples/api-client.sh` does, over HTTP, against a daemon with a real
//! holder and the fake `claude` + `gh`: send → wait → push → close. Plus the `/screen` budget.

mod common;

use std::time::{Duration, Instant};

use common::{claude, headless, pane_of, wait_for, Box_};
use femme_proto::state::SessionStatus;
use femmed::core::{ApiKey, Core};
use serde_json::{json, Value};

struct Api {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.key);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    /// `api-client.sh wait`: polls until the agent is not working.
    async fn wait(&self, name: &str) -> Value {
        self.wait_while(name, &["working"]).await
    }

    /// Polls until the status is none of `busy` (an answer just given leaves `blocked` a moment later).
    async fn wait_while(&self, name: &str, busy: &[&str]) -> Value {
        let t0 = Instant::now();
        loop {
            let (st, v) = self.get(&format!("/api/agents/{name}")).await;
            assert_eq!(st, 200, "{v}");
            if !busy.iter().any(|b| v["status"] == *b) {
                return v;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "still {busy:?}: {v}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_client_flow_send_wait_push_close_works_against_the_daemon() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    d.api_key_create(ApiKey {
        name: "bot".into(),
        key: "femme-test-key".into(),
        github_token: None,
        github_login: None,
        git_name: None,
        git_email: None,
        created: 0,
    })
    .await
    .unwrap();
    let api = Api {
        base: format!("http://{}", d.cfg.http_bind),
        key: "femme-test-key".into(),
        http: reqwest::Client::new(),
    };

    // send: the first message opens a headless session in the clone
    let (st, v) = api
        .post(
            "/api/agents/bot",
            json!({"text": "do the thing", "repo": "acme/widgets"}),
        )
        .await;
    assert_eq!((st, v["status"].as_str()), (200, Some("working")), "{v}");
    let pane_id = v["pane_id"].as_str().unwrap().to_owned();
    assert!(b.home().join("femme-work/bot/.git").is_dir(), "cloned");
    let s = d.session_by_name("bot").await.unwrap().unwrap();
    assert_eq!(
        (s.mode, s.owner.as_str(), s.pane_id(1)),
        (
            femme_proto::state::Mode::Headless,
            "key:bot",
            pane_id.clone()
        )
    );

    // wait: the agent (the fake echoes its brief) stops; the report and the git state are read
    let v = api.wait("bot").await;
    assert_eq!(v["status"], "done", "{v}");
    assert!(
        v["screen"]
            .as_str()
            .unwrap()
            .contains("echo: This terminal is bot's standing Claude Code session"),
        "{}",
        v["screen"]
    );
    assert!(
        v["screen"].as_str().unwrap().contains("do the thing"),
        "{}",
        v["screen"]
    );
    assert_eq!(v["pushed"], false, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("changes ready, not pushed to main"),
        "{v}"
    );
    assert_eq!(v["report"], Value::Null, "the fake writes no report");
    // `done` is shown once, then the session is idle
    let (_, v) = api.get("/api/agents/bot").await;
    assert_eq!(v["status"], "idle", "{v}");

    // a second task on the same session, and a question the caller sees as `blocked`
    let (st, v) = api.post("/api/agents/bot", json!({"text": "ask"})).await;
    assert_eq!(st, 200, "{v}");
    let v = api.wait("bot").await;
    assert_eq!(v["status"], "blocked", "{v}");
    assert_eq!(
        v["message"], "waiting for an answer: read the screen",
        "{v}"
    );
    assert!(
        v["screen"].as_str().unwrap().contains("Which?"),
        "{}",
        v["screen"]
    );
    let (st, v) = api.post("/api/agents/bot", json!({"text": "more"})).await;
    assert_eq!(st, 409, "a prompt while blocked: {v}");
    let (st, _) = api
        .post(
            &format!("/api/terminals/{pane_id}/input"),
            json!({"data": "1"}),
        )
        .await;
    assert_eq!(st, 200);
    let v = api.wait_while("bot", &["blocked", "working"]).await;
    assert_eq!(v["status"], "done", "{v}");
    assert!(
        v["screen"].as_str().unwrap().contains("you chose: a"),
        "{}",
        v["screen"]
    );

    // push: the owner's verdict is a prompt; discard likewise
    let (st, v) = api
        .post("/api/agents/bot/push", json!({"text": "ship it"}))
        .await;
    assert_eq!((st, v["status"].as_str()), (200, Some("working")), "{v}");
    let v = api.wait("bot").await;
    let screen = v["screen"].as_str().unwrap();
    assert!(
        screen.contains("echo: Push request from the owner"),
        "{screen}"
    );
    assert!(screen.contains("Note from the owner:\nship it"), "{screen}");
    let (st, _) = api.post("/api/agents/bot/discard", json!({})).await;
    assert_eq!(st, 200);
    let v = api.wait("bot").await;
    assert!(
        v["screen"]
            .as_str()
            .unwrap()
            .contains("echo: Discard request from the owner"),
        "{}",
        v["screen"]
    );

    // the phone board's list shows it; close removes it
    let (st, v) = api.get("/api/terminals").await;
    assert_eq!(st, 200);
    assert!(v.to_string().contains(&pane_id), "{v}");
    let (st, _) = api
        .call(reqwest::Method::DELETE, "/api/agents/bot", None)
        .await;
    assert_eq!(st, 200);
    let (st, v) = api.get("/api/agents/bot").await;
    assert_eq!((st, v["error"].as_str()), (404, Some("not running")), "{v}");
    assert!(d.terminals().await.unwrap().is_empty());
}

/// M1 criterion: `/screen` p99 server time < 20 ms, in a terminal and on a transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_reads_stay_under_the_budget_in_both_modes() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    for (what, req) in [
        ("tui", claude(&b.home())),
        ("headless", headless(&b.home())),
    ] {
        let s = d.create_session(req).await.unwrap();
        let (sid, pane) = (s.sid, pane_of(&s));
        wait_for(&d, sid, "idle", |s| {
            s.is_some_and(|s| s.status == SessionStatus::Idle)
        })
        .await;
        for i in 0..20 {
            d.prompt(&pane, &format!("line {i}")).await.unwrap();
            wait_for(&d, sid, "idle", |s| {
                s.is_some_and(|s| s.status == SessionStatus::Idle)
            })
            .await;
        }
        let mut t = Vec::with_capacity(200);
        for _ in 0..200 {
            let t0 = Instant::now();
            let screen = d.screen(&pane, false).await.unwrap();
            t.push(t0.elapsed());
            assert!(screen.text.contains("echo: line 19"));
        }
        t.sort();
        let (p50, p99, max) = (t[100], t[198], t[199]);
        eprintln!("/screen {what}: p50 {p50:?} p99 {p99:?} max {max:?}");
        assert!(p99 < Duration::from_millis(20), "{what}: p99 {p99:?}");
        d.close_session(sid).await.unwrap();
    }
}
