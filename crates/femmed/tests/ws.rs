//! The phone's way in: `/femme/ws` on the HTTP layer, JSON over a WebSocket, served by the same
//! front as the desktop's QUIC. The phone's side is written out here with a plain WebSocket client.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::Box_;
use femme_proto::Id;
use femme_store::sha256;
use femmed::core::Core;
use femmed::daemon::Daemon;
use femmed::front::{serve_front_with, FrontHandle, Waits};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TOKEN: &str = "a-token-the-phone-scanned";

/// A daemon with its front and the HTTP layer (with the phone route) up on loopback.
struct Rig {
    _front: FrontHandle,
    d: Arc<Daemon>,
    http: SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
    _b: Box_,
}

async fn rig() -> Rig {
    std::env::set_var("SHELL", "/bin/sh");
    let b = Box_::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let d = Daemon::open(b.config(http.to_string())).unwrap();
    let front = serve_front_with(
        d.clone(),
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Waits::default(),
    )
    .await
    .expect("the front starts");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let core: Arc<dyn Core> = d.clone();
    tokio::spawn(femmed::serve_on_with(
        listener,
        core,
        Some(front.gate()),
        async move {
            let _ = stopped.await;
        },
    ));
    // enrolled the way `femmed pair` does it: the device's key is the token's hash
    d.store
        .write(|tx| tx.enroll_device("phone", &sha256(TOKEN.as_bytes()), 1_000))
        .expect("enrolls");
    Rig {
        _front: front,
        d,
        http,
        _stop: stop,
        _b: b,
    }
}

impl Rig {
    fn url(&self) -> String {
        format!("ws://{}/femme/ws", self.http)
    }

    async fn connect(&self, token: Option<&str>) -> Result<Socket, String> {
        let mut req = self.url().into_client_request().unwrap();
        if let Some(t) = token {
            req.headers_mut()
                .insert("Authorization", format!("Bearer {t}").parse().unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(e) => Err(e.to_string()),
        }
    }
}

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

async fn next(ws: &mut Socket) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(20), ws.next())
        .await
        .expect("a message within 20 s")
        .expect("the socket is open")
        .expect("a frame");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("json"),
        other => panic!("not text: {other:?}"),
    }
}

/// `Hello` on stream 1, `Welcome` back, the events stream opened by the daemon.
async fn hello(ws: &mut Socket) {
    ws.send(text(json!({"s": 1, "open": "Ctrl"})))
        .await
        .unwrap();
    let hello = json!({"Hello": {"proto": 1, "client_instance": 7,
        "cursors": {"epoch": 0, "global_rev": null, "sessions": []}}});
    ws.send(text(json!({"s": 1, "m": hello}))).await.unwrap();
    let welcome = next(ws).await;
    assert_eq!(welcome["s"], 1, "{welcome}");
    assert_eq!(welcome["m"]["Welcome"]["proto"], 1, "{welcome}");
    assert_eq!(next(ws).await, json!({"s": 2, "open": "Events"}));
}

#[tokio::test]
async fn no_token_or_a_wrong_one_is_refused() {
    let r = rig().await;
    let e = r.connect(None).await.unwrap_err();
    assert!(e.contains("401"), "{e}");
    let e = r.connect(Some("nope")).await.unwrap_err();
    assert!(e.contains("401"), "{e}");
    // the token may come as a query parameter too (a client that cannot set headers)
    let url = format!("{}?token={TOKEN}", r.url());
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connects");
    hello(&mut ws).await;
}

#[tokio::test]
async fn a_phone_asks_subscribes_and_is_answered_in_json() {
    let r = rig().await;
    let mut ws = r.connect(Some(TOKEN)).await.expect("connects");
    hello(&mut ws).await;

    let op = Id::new().to_string();
    ws.send(text(
        json!({"s": 1, "m": {"Req": {"op": op, "req": "ServerInfo"}}}),
    ))
    .await
    .unwrap();
    let resp = next(&mut ws).await;
    assert_eq!(resp["s"], 1, "{resp}");
    assert_eq!(resp["m"]["Resp"]["op"], op, "{resp}");
    assert!(
        resp["m"]["Resp"]["result"]["Ok"]["ServerInfo"]["version"].is_string(),
        "{resp}"
    );

    // a session made behind the phone's back shows up on the events stream once it subscribes
    let s =
        r.d.create_session(femmed::core::CreateSession {
            cwd: r._b.home().display().to_string(),
            agent: femme_proto::state::Agent::Shell,
            mode: femme_proto::state::Mode::Tui,
            label: Some("from the box".into()),
            name: None,
            owner: "board".into(),
            model: None,
            resume: None,
        })
        .await
        .expect("a shell opens");
    let op = Id::new().to_string();
    let sub = json!({"Subscribe": {"scope": "Global", "after": null}});
    ws.send(text(json!({"s": 1, "m": {"Req": {"op": op, "req": sub}}})))
        .await
        .unwrap();
    let (mut answered, mut seen) = (false, false);
    while !(answered && seen) {
        let m = next(&mut ws).await;
        if m["s"] == 1 {
            assert_eq!(m["m"]["Resp"]["op"], op, "{m}");
            assert_eq!(m["m"]["Resp"]["result"], json!({"Ok": "Ok"}), "{m}");
            answered = true;
        } else {
            assert_eq!(m["s"], 2, "{m}");
            let changes = &m["m"]["Changes"];
            assert_eq!(changes["scope"], "Global", "{m}");
            for row in changes["rows"].as_array().expect("rows") {
                if row["Session"]["id"] == s.sid.to_string() {
                    assert_eq!(row["Session"]["label"], "from the box");
                    assert_eq!(row["Session"]["agent"], "Shell");
                    seen = true;
                }
            }
        }
    }

    // a bad request is an error for its op, not the end of the connection
    let op = Id::new().to_string();
    let bad = json!({"SessionLabel": {"sid": "not-a-ulid", "label": null}});
    ws.send(text(json!({"s": 1, "m": {"Req": {"op": op, "req": bad}}})))
        .await
        .unwrap();
    // the envelope does not decode as C2S: the ctrl stream, and with it the connection, ends
    let end = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(Ok(m)) = ws.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    assert!(
        end.is_ok(),
        "the socket closes after a message that is no request"
    );
}

#[tokio::test]
async fn a_revoked_phone_is_cut_off() {
    let r = rig().await;
    let mut ws = r.connect(Some(TOKEN)).await.expect("connects");
    hello(&mut ws).await;
    let phone =
        r.d.store
            .read(femme_store::auth::list_devices)
            .unwrap()
            .into_iter()
            .find(|d| d.name == "phone")
            .unwrap();
    r.d.store.write(|tx| tx.revoke_device(phone.id)).unwrap();
    assert!(r.connect(Some(TOKEN)).await.is_err(), "revoked: refused");
}

/// Sends `req` on the ctrl stream and gives back its `result`.
async fn ask(ws: &mut Socket, req: Value) -> Value {
    let op = Id::new().to_string();
    let msg = json!({"s": 1, "m": {"Req": {"op": op, "req": req}}});
    ws.send(text(msg)).await.unwrap();
    loop {
        let m = next(ws).await;
        if m["s"] == 1 && m["m"]["Resp"]["op"] == op {
            return m["m"]["Resp"]["result"].clone();
        }
    }
}

#[tokio::test]
async fn a_phone_registers_for_pushes() {
    let r = rig().await;
    let mut ws = r.connect(Some(TOKEN)).await.expect("connects");
    hello(&mut ws).await;
    let phone =
        r.d.store
            .read(femme_store::auth::list_devices)
            .unwrap()
            .into_iter()
            .find(|d| d.name == "phone")
            .unwrap();
    let rows = || r.d.store.read(femme_store::push::push_list).unwrap();
    let muted = Id::new();
    let register = |token: &str| {
        json!({"PushRegister": {"platform": "Ntfy", "token": token, "rules": {
            "blocked": true, "done": false, "muted": [muted.to_string()], "quiet": [23, 7]}}})
    };

    assert_eq!(
        ask(&mut ws, register("femme-phone-1")).await,
        json!({"Ok": "Ok"})
    );
    let got = rows();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].device, phone.id);
    assert_eq!(got[0].platform, femme_proto::rpc::PushPlatform::Ntfy);
    assert_eq!(got[0].token, "femme-phone-1");
    assert_eq!(
        got[0].rules,
        femme_proto::rpc::PushRules {
            blocked: true,
            done: false,
            muted: vec![muted],
            quiet: Some((23, 7)),
        }
    );

    // the phone's new topic replaces its old one
    assert_eq!(
        ask(&mut ws, register("femme-phone-2")).await,
        json!({"Ok": "Ok"})
    );
    let tokens: Vec<String> = rows().into_iter().map(|p| p.token).collect();
    assert_eq!(tokens, ["femme-phone-2"]);

    // unregistering: gone, and again is still fine
    let unregister = json!({"PushUnregister": {"token": "femme-phone-2"}});
    assert_eq!(ask(&mut ws, unregister.clone()).await, json!({"Ok": "Ok"}));
    assert!(rows().is_empty());
    assert_eq!(ask(&mut ws, unregister).await, json!({"Ok": "Ok"}));

    // APNs has no relay yet; a topic ntfy would not take is refused
    let apns = json!({"PushRegister": {"platform": "Apns", "token": "abc",
        "rules": {"blocked": true, "done": true, "muted": [], "quiet": null}}});
    let res = ask(&mut ws, apns).await;
    assert_eq!(res["Err"]["code"], "Unsupported", "{res}");
    let res = ask(&mut ws, register("no/slashes")).await;
    assert_eq!(res["Err"]["code"], "Bad", "{res}");
    assert!(rows().is_empty());

    // a revoked phone loses its registration
    assert_eq!(
        ask(&mut ws, register("femme-phone-3")).await,
        json!({"Ok": "Ok"})
    );
    r.d.store.write(|tx| tx.revoke_device(phone.id)).unwrap();
    assert!(rows().is_empty());
}
