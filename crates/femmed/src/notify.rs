//! Phone pushes through ntfy.sh, on attention transitions (replaces server/notify.sh's 5 s poll):
//! `blocked` → "needs you" priority 5, `done` → "finished" priority 3, nothing otherwise.
//! One push per session per 10 s at most; pushes are fire-and-forget with a 10 s timeout.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use femme_proto::state::Attention;
use femme_proto::Sid;

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

pub struct Notifier {
    topic: Option<String>,
    client: reqwest::Client,
    last: Mutex<HashMap<Sid, (Attention, Instant)>>,
}

impl Notifier {
    /// `topic` empty → pushes are logged, not sent (same as an unset NTFY_TOPIC today).
    pub fn new(topic: Option<String>) -> Notifier {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Notifier {
            topic: topic.filter(|t| !t.is_empty()),
            client,
            last: Mutex::new(HashMap::new()),
        }
    }

    /// Call on every session attention change; `label` names the session in the push
    /// (its label, name or handle).
    pub fn attention(&self, sid: Sid, label: &str, next: Attention) {
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
        if let Some((text, prio)) = msg {
            self.push(label.to_string(), format!("{text} ({label})"), prio);
        }
    }

    pub fn push(&self, title: String, message: String, priority: u8) {
        let Some(topic) = self.topic.clone() else {
            tracing::info!(%title, %message, "ntfy (no topic): not sent");
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({ "topic": topic, "title": title, "message": message, "priority": priority });
            if let Err(e) = client
                .post(NTFY)
                .json(&body)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                tracing::warn!(%e, "ntfy push failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
