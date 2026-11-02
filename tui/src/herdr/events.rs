//! A long-lived `events.subscribe` connection. herdr acknowledges first (`subscription_started`),
//! then pushes one event object per line. Events are only a hint to take a fresh snapshot: the
//! docs say not to replay them onto cached state.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use super::client::Error;

/// Blocks until `stop` is set (Ok) or the stream ends or errors (Err). `events_lost` arrives as an
/// Api error: the caller resubscribes and takes a snapshot.
pub fn subscribe(path: &Path, params: Value, stop: &AtomicBool, mut on_event: impl FnMut(&Value)) -> Result<(), Error> {
    let s = UnixStream::connect(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    // A short read timeout, so a stop request is noticed within a second.
    s.set_read_timeout(Some(Duration::from_secs(1))).map_err(|e| Error::Io(e.to_string()))?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let mut conn = BufReader::new(s);
    let line = json!({"id": "femme_sub", "method": "events.subscribe", "params": params}).to_string();
    conn.get_mut().write_all(line.as_bytes()).and_then(|_| conn.get_mut().write_all(b"\n")).map_err(|e| Error::Io(e.to_string()))?;
    let mut buf = String::new();
    let mut acked = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        buf.clear();
        match conn.read_line(&mut buf) {
            Ok(0) => return Err(Error::Io("herdr closed the event stream".into())),
            Ok(_) => {
                let Ok(v) = serde_json::from_str::<Value>(&buf) else { continue };
                if let Some(err) = v.get("error") {
                    return Err(Error::Api {
                        code: err["code"].as_str().unwrap_or("error").to_string(),
                        message: err["message"].as_str().unwrap_or("").to_string(),
                    });
                }
                if !acked {
                    acked = true;
                    if v.get("result").is_some() {
                        continue; // the subscription_started acknowledgement
                    }
                }
                on_event(&v);
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
}

/// What the list needs: lifecycle events for everything, status changes per agent pane (herdr
/// takes `pane.agent_status_changed` only with a pane id).
pub fn list_subscriptions(agent_panes: &[String]) -> Value {
    let mut subs: Vec<Value> = [
        "workspace.created",
        "workspace.updated",
        "workspace.closed",
        "workspace.renamed",
        "pane.created",
        "pane.closed",
        "pane.updated",
        "pane.exited",
        "pane.agent_detected",
    ]
    .iter()
    .map(|t| json!({"type": t}))
    .collect();
    for p in agent_panes {
        subs.push(json!({"type": "pane.agent_status_changed", "pane_id": p}));
    }
    json!({"subscriptions": subs})
}
