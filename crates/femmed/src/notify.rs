//! Phone pushes through ntfy.sh, on attention transitions (replaces server/notify.sh's 5 s poll):
//! `blocked` → "needs you" priority 5, `done` → "finished" priority 3, nothing otherwise.
//! One push per session per 10 s at most; pushes are fire-and-forget with a 10 s timeout.
//!
//! A transition goes to the box's own `NTFY_TOPIC` and to every phone that registered an ntfy
//! topic (`Req::PushRegister`) whose rules let it through ([`wants`]). The registrations are read
//! from the store again before each fan-out: `femmed devices revoke` runs in another process, and
//! a revoked phone must stop getting pushes at once.
//!
//! Claude's own words go the same way ([`Notifier::claude_push`]): Claude Code's `PushNotification`
//! tool only delivers through Remote Control, which a FEMME chat does not have.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Timelike;
use femme_proto::rpc::{PushPlatform, PushRules};
use femme_proto::state::Attention;
use femme_proto::Sid;
use femme_store::{push, Push, Store};

pub const DEBOUNCE: Duration = Duration::from_secs(10);
const NTFY: &str = "https://ntfy.sh/";

/// What to say for a transition, if anything. Pure.
pub fn decide(prev: Attention, next: Attention) -> Option<(&'static str, u8)> {
    if prev == next {
        return None;
    }
    match next {
        Attention::Blocked => Some(("needs you", 5)),
        Attention::Done => Some(("finished", 3)),
        Attention::None => None,
    }
}

/// The deep link a phone's push opens: the session's chat in the FEMME app.
pub fn session_link(sid: Sid) -> String {
    format!("femme://session/{sid}")
}

/// Whether a phone with `rules` is pushed that session `sid` became `next`, at local hour `hour`.
/// Pure.
pub fn wants(rules: &PushRules, sid: Sid, next: Attention, hour: u8) -> bool {
    let kind = match next {
        Attention::Blocked => rules.blocked,
        Attention::Done => rules.done,
        Attention::None => false,
    };
    kind && !rules.muted.contains(&sid) && !rules.quiet.is_some_and(|q| quiet(q, hour))
}

/// Whether `hour` falls in `[from, to)`, which wraps midnight when `from > to`.
fn quiet((from, to): (u8, u8), hour: u8) -> bool {
    if from <= to {
        (from..to).contains(&hour)
    } else {
        hour >= from || hour < to
    }
}

/// The longest message of Claude's that is pushed, in chars.
const CLAUDE_PUSH_MAX: usize = 300;

pub struct Notifier {
    topic: Option<String>,
    /// The ntfy server, ending in `/`.
    base: String,
    client: reqwest::Client,
    last: Mutex<HashMap<Sid, (Attention, Instant)>>,
    /// The phones' registrations, as last read from `store`.
    regs: Mutex<Vec<Push>>,
    store: Option<Arc<Store>>,
}

impl Notifier {
    /// `topic` empty → the box's own pushes are logged, not sent (same as an unset NTFY_TOPIC).
    pub fn new(topic: Option<String>) -> Notifier {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Notifier {
            topic: topic.filter(|t| !t.is_empty()),
            base: NTFY.into(),
            client,
            last: Mutex::new(HashMap::new()),
            regs: Mutex::new(Vec::new()),
            store: None,
        }
    }

    /// Posts to another ntfy server than ntfy.sh (`NTFY_URL`, a test's stub). Empty keeps ntfy.sh.
    pub fn with_base(mut self, base: impl Into<String>) -> Notifier {
        let base: String = base.into();
        if !base.is_empty() {
            self.base = format!("{}/", base.trim_end_matches('/'));
        }
        self
    }

    /// Reads the phones' registrations from `store`, now and before each fan-out.
    pub fn with_store(mut self, store: Arc<Store>) -> Notifier {
        self.store = Some(store);
        self.reload();
        self
    }

    /// Reads the registrations again (after a register or an unregister). A failed read keeps
    /// the ones it had.
    pub fn reload(&self) {
        let Some(store) = &self.store else { return };
        match store.read(push::push_list) {
            Ok(regs) => *self.regs.lock().unwrap() = regs,
            Err(e) => tracing::warn!(%e, "push registrations not read"),
        }
    }

    /// Replaces the registrations (a notifier without a store).
    pub fn set_registrations(&self, regs: Vec<Push>) {
        *self.regs.lock().unwrap() = regs;
    }

    /// Call on every session attention change; `label` names the session in the push
    /// (its label, name or handle).
    pub fn attention(&self, sid: Sid, label: &str, next: Attention) {
        let hour = chrono::Local::now().hour() as u8;
        self.attention_at(sid, label, next, hour);
    }

    /// [`Notifier::attention`] at local hour `hour`.
    pub(crate) fn attention_at(&self, sid: Sid, label: &str, next: Attention, hour: u8) {
        let msg = {
            let mut g = self.last.lock().unwrap();
            let now = Instant::now();
            let (prev, at) = g
                .get(&sid)
                .copied()
                .unwrap_or((Attention::None, now - DEBOUNCE));
            let d = decide(prev, next);
            g.insert(sid, (next, if d.is_some() { now } else { at }));
            match d {
                Some(m) if now.duration_since(at) >= DEBOUNCE || prev == Attention::None => Some(m),
                _ => None,
            }
        };
        let Some((text, prio)) = msg else { return };
        let (title, message) = (label.to_string(), format!("{text} ({label})"));
        self.push(title.clone(), message.clone(), prio);
        self.reload();
        let topics: Vec<String> = self
            .regs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.platform == PushPlatform::Ntfy && wants(&r.rules, sid, next, hour))
            .map(|r| r.token.clone())
            .collect();
        // A phone's push opens the session when tapped (the app registers the `femme` scheme).
        let click = session_link(sid);
        for topic in topics {
            self.post_to(
                topic,
                title.clone(),
                message.clone(),
                prio,
                Some(click.clone()),
            );
        }
    }

    /// Pushes what Claude asked to tell the user (its `PushNotification` call in session `sid`):
    /// `title` names the session, `message` is Claude's text. Goes to the box's topic and to every
    /// phone that would hear this session is blocked (its mute and quiet hours hold). Never
    /// debounced: the caller sends one per call. `Err` says why nothing arrived.
    pub async fn claude_push(
        &self,
        sid: Sid,
        title: &str,
        message: &str,
    ) -> Result<(), &'static str> {
        let hour = chrono::Local::now().hour() as u8;
        self.claude_push_at(sid, title, message, hour).await
    }

    /// [`Notifier::claude_push`] at local hour `hour`.
    pub(crate) async fn claude_push_at(
        &self,
        sid: Sid,
        title: &str,
        message: &str,
        hour: u8,
    ) -> Result<(), &'static str> {
        self.reload();
        let (phones, wanting): (usize, Vec<String>) = {
            let regs = self.regs.lock().unwrap();
            let ntfy = regs.iter().filter(|r| r.platform == PushPlatform::Ntfy);
            (
                ntfy.clone().count(),
                ntfy.filter(|r| wants(&r.rules, sid, Attention::Blocked, hour))
                    .map(|r| r.token.clone())
                    .collect(),
            )
        };
        let topics: Vec<String> = self.topic.iter().cloned().chain(wanting).collect();
        if topics.is_empty() {
            return Err(if phones == 0 {
                "no phone registered"
            } else {
                "muted or quiet hours"
            });
        }
        let message: String = message.trim().chars().take(CLAUDE_PUSH_MAX).collect();
        let mut sent = false;
        // ponytail: one after the other, the caller waits for all; join them if a box has many phones
        for topic in topics {
            let body = serde_json::json!({ "topic": topic, "title": title, "message": message,
                "priority": 4, "click": session_link(sid) });
            sent |= post(&self.client, &self.base, &body).await;
        }
        sent.then_some(()).ok_or("ntfy refused it")
    }

    /// A push to the box's own topic only (the owner's alerts).
    pub fn push(&self, title: String, message: String, priority: u8) {
        let Some(topic) = self.topic.clone() else {
            tracing::info!(%title, %message, "ntfy (no topic): not sent");
            return;
        };
        self.post_to(topic, title, message, priority, None);
    }

    /// Posts one push in its own task: a failure is logged and touches no other push. `click`
    /// is ntfy's tap target.
    fn post_to(
        &self,
        topic: String,
        title: String,
        message: String,
        priority: u8,
        click: Option<String>,
    ) {
        let (client, base) = (self.client.clone(), self.base.clone());
        tokio::spawn(async move {
            let mut body = serde_json::json!({ "topic": topic, "title": title, "message": message, "priority": priority });
            if let Some(click) = click {
                body["click"] = serde_json::Value::String(click);
            }
            post(&client, &base, &body).await;
        });
    }
}

/// Posts one push; a failure is logged. Whether ntfy took it.
async fn post(client: &reqwest::Client, base: &str, body: &serde_json::Value) -> bool {
    let r = client.post(base).json(body).send().await;
    match r.and_then(|r| r.error_for_status()) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(%e, topic = %body["topic"], "ntfy push failed");
            false
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn transitions() {
        assert_eq!(
            decide(Attention::None, Attention::Blocked),
            Some(("needs you", 5))
        );
        assert_eq!(
            decide(Attention::Blocked, Attention::Done),
            Some(("finished", 3))
        );
        assert_eq!(decide(Attention::Blocked, Attention::Blocked), None);
        assert_eq!(decide(Attention::Done, Attention::None), None);
    }

    #[tokio::test]
    async fn debounce_per_session() {
        let n = Notifier::new(None);
        let sid = femme_proto::Id(1);
        // first blocked: sent (logged); flip to done within 10 s: suppressed by the debounce
        n.attention(sid, "x", Attention::Blocked);
        let g = n.last.lock().unwrap();
        assert_eq!(g[&sid].0, Attention::Blocked);
    }

    fn rules(blocked: bool, done: bool) -> PushRules {
        PushRules {
            blocked,
            done,
            ..PushRules::default()
        }
    }

    #[test]
    fn rules_pick_the_transition() {
        let sid = femme_proto::Id(1);
        let both = rules(true, true);
        assert!(wants(&both, sid, Attention::Blocked, 12));
        assert!(wants(&both, sid, Attention::Done, 12));
        assert!(!wants(&both, sid, Attention::None, 12));
        assert!(wants(&rules(true, false), sid, Attention::Blocked, 12));
        assert!(!wants(&rules(true, false), sid, Attention::Done, 12));
        assert!(!wants(&rules(false, true), sid, Attention::Blocked, 12));
        assert!(wants(&rules(false, true), sid, Attention::Done, 12));
        assert!(!wants(&PushRules::default(), sid, Attention::Blocked, 12));
    }

    #[test]
    fn muted_sessions_are_never_pushed() {
        let (a, b) = (femme_proto::Id(1), femme_proto::Id(2));
        let r = PushRules {
            muted: vec![a],
            ..rules(true, true)
        };
        assert!(!wants(&r, a, Attention::Blocked, 12));
        assert!(wants(&r, b, Attention::Blocked, 12));
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let sid = femme_proto::Id(1);
        let night = PushRules {
            quiet: Some((23, 7)),
            ..rules(true, true)
        };
        for h in [23, 0, 3, 6] {
            assert!(!wants(&night, sid, Attention::Blocked, h), "{h}h is quiet");
        }
        for h in [7, 12, 22] {
            assert!(wants(&night, sid, Attention::Blocked, h), "{h}h is not");
        }
        let lunch = PushRules {
            quiet: Some((12, 14)),
            ..rules(true, true)
        };
        assert!(!wants(&lunch, sid, Attention::Done, 12));
        assert!(!wants(&lunch, sid, Attention::Done, 13));
        assert!(wants(&lunch, sid, Attention::Done, 14));
        assert!(wants(&lunch, sid, Attention::Done, 11));
        let never = PushRules {
            quiet: Some((5, 5)),
            ..rules(true, true)
        };
        assert!((0..24).all(|h| wants(&never, sid, Attention::Done, h)));
    }

    /// An ntfy stand-in on loopback: every posted body comes out of the channel; a post to the
    /// topic `broken` is answered 500.
    pub(crate) fn stub() -> (String, mpsc::Receiver<serde_json::Value>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", l.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let status = if v["topic"] == "broken" {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = tx.send(v);
            }
        });
        (base, rx)
    }

    fn reg(token: &str, rules: PushRules) -> Push {
        Push {
            device: femme_proto::Id(9),
            platform: PushPlatform::Ntfy,
            token: token.into(),
            rules,
            created: 0,
        }
    }

    /// The bodies posted until none comes for a moment, by topic.
    pub(crate) fn drain(
        rx: &mpsc::Receiver<serde_json::Value>,
    ) -> HashMap<String, serde_json::Value> {
        let mut got = HashMap::new();
        while let Ok(v) = rx.recv_timeout(Duration::from_secs(2)) {
            got.insert(v["topic"].as_str().unwrap().to_string(), v);
        }
        got
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transition_fans_out_to_the_phones_that_want_it() {
        let (base, rx) = stub();
        let n = Notifier::new(Some("owner".into())).with_base(base);
        let (sid, other) = (femme_proto::Id(1), femme_proto::Id(2));
        n.set_registrations(vec![
            reg("broken", rules(true, true)),
            reg("all", rules(true, true)),
            reg("done-only", rules(false, true)),
            reg(
                "muted",
                PushRules {
                    muted: vec![sid],
                    ..rules(true, true)
                },
            ),
            reg(
                "asleep",
                PushRules {
                    quiet: Some((22, 6)),
                    ..rules(true, true)
                },
            ),
            Push {
                platform: PushPlatform::Apns,
                ..reg("apns", rules(true, true))
            },
        ]);
        n.attention_at(sid, "fix-ci", Attention::Blocked, 23);
        let got = drain(&rx);
        let mut topics: Vec<&str> = got.keys().map(String::as_str).collect();
        topics.sort();
        // `broken` was answered 500, and the others were posted all the same
        assert_eq!(topics, ["all", "broken", "owner"]);
        // a phone's push opens the session when tapped; the owner's topic has no click
        assert_eq!(
            got["all"],
            serde_json::json!({"topic": "all", "title": "fix-ci",
                "message": "needs you (fix-ci)", "priority": 5,
                "click": format!("femme://session/{sid}")})
        );
        assert_eq!(
            got["owner"],
            serde_json::json!({"topic": "owner", "title": "fix-ci",
                "message": "needs you (fix-ci)", "priority": 5})
        );

        // the same session again within 10 s: debounced for everyone
        n.attention_at(sid, "fix-ci", Attention::Done, 12);
        assert!(drain(&rx).is_empty());

        // another session finishing at noon
        n.attention_at(other, "docs", Attention::Done, 12);
        let got = drain(&rx);
        let mut topics: Vec<&str> = got.keys().map(String::as_str).collect();
        topics.sort();
        assert_eq!(
            topics,
            ["all", "asleep", "broken", "done-only", "muted", "owner"]
        );
        assert_eq!(got["done-only"]["message"], "finished (docs)");
        assert_eq!(got["done-only"]["priority"], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn claudes_push_goes_to_the_box_and_the_phones_that_would_hear_blocked() {
        let (base, rx) = stub();
        let n = Notifier::new(Some("owner".into())).with_base(base.clone());
        let sid = femme_proto::Id(1);
        n.set_registrations(vec![
            reg("phone", rules(true, false)),
            reg(
                "muted",
                PushRules {
                    muted: vec![sid],
                    ..rules(true, true)
                },
            ),
            reg(
                "asleep",
                PushRules {
                    quiet: Some((22, 6)),
                    ..rules(true, true)
                },
            ),
        ]);
        let long = format!("  tests green {}", "x".repeat(400));
        assert_eq!(n.claude_push_at(sid, "fix-ci", &long, 23).await, Ok(()));
        let got = drain(&rx);
        let mut topics: Vec<&str> = got.keys().map(String::as_str).collect();
        topics.sort();
        assert_eq!(topics, ["owner", "phone"]);
        let message: String = long.trim().chars().take(300).collect();
        for topic in ["owner", "phone"] {
            assert_eq!(
                got[topic],
                serde_json::json!({"topic": topic, "title": "fix-ci", "message": message,
                    "priority": 4, "click": format!("femme://session/{sid}")})
            );
        }

        // not debounced: the next call's push goes out too
        assert_eq!(n.claude_push_at(sid, "fix-ci", "again", 12).await, Ok(()));
        assert_eq!(drain(&rx).len(), 3, "owner, phone and the one that slept");

        // nothing arrived: the reason
        let none = Notifier::new(None).with_base(base.clone());
        assert_eq!(
            none.claude_push_at(sid, "x", "m", 12).await,
            Err("no phone registered")
        );
        none.set_registrations(vec![reg(
            "muted",
            PushRules {
                muted: vec![sid],
                ..rules(true, true)
            },
        )]);
        assert_eq!(
            none.claude_push_at(sid, "x", "m", 12).await,
            Err("muted or quiet hours")
        );
        none.set_registrations(vec![reg("broken", rules(true, true))]);
        assert_eq!(
            none.claude_push_at(sid, "x", "m", 12).await,
            Err("ntfy refused it")
        );
    }
}
