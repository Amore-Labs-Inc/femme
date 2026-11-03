//! The pump between a pane and the daemon's terminal stream: what the pane says upward goes to the
//! client's [`Term`](femme_client::Term), what the holder sends comes back down. It runs on the
//! side runtime and hands the pane the same two channels a synthetic source does.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use femme_client::Client;
use femme_proto::term::{TermDown, TermUp};
use femme_proto::PaneId;
use tokio::sync::mpsc;

use super::synth::WINDOW;

/// What to do with one message of the pane.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// The pane applied every frame up to this rev: the `Term` acknowledges it, which keeps the
    /// holder sending.
    Ack(u64),
    /// Input, `Resize`, `Focus` or `GetRows`: the `Term` sends it (and stamps the input's seq).
    Send(TermUp),
    /// A frame did not apply: attach the pane again, from nothing.
    Reopen,
    /// Nothing to do.
    Skip,
}

/// Routes `up`. `opened` says whether the pane's first `Open` has been seen: that one only means
/// "I want frames", the `Term` has opened the stream by itself. Every later `Open` is a resync.
pub fn route(up: TermUp, opened: &mut bool) -> Route {
    match up {
        TermUp::Ack { rev } => Route::Ack(rev),
        TermUp::Open { .. } if !std::mem::replace(opened, true) => Route::Skip,
        TermUp::Open { .. } => Route::Reopen,
        up => Route::Send(up),
    }
}

/// How often a pane's stream is opened again when it fails before its first frame, and how long
/// to wait before the first try (the wait doubles up to [`RETRY_MAX`]). The daemon writes a new
/// session's rows, and so lists its pane, a moment before the session's holder listens: a
/// terminal stream opened in that moment is answered `Exited { status: -1 }`, and the same stream
/// a moment later is fine.
const RETRIES: u32 = 8;
const RETRY_FIRST: Duration = Duration::from_millis(150);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// Whether to open the stream again, and after how long, for `msg` coming from it: `seen` says a
/// frame has come already (the pane is there, so an `Exited` is its own), `tried` how many times
/// the stream was opened again.
pub fn retry_after(msg: &TermDown, seen: bool, tried: u32) -> Option<Duration> {
    let failed = matches!(msg, TermDown::Exited { status: -1 });
    (failed && !seen && tried < RETRIES).then(|| (RETRY_FIRST * 2u32.pow(tried)).min(RETRY_MAX))
}

/// The last `Resize` and `Focus` the pane said: a `Term` keeps them for its own reconnects, a new
/// one has to be told again.
#[derive(Default)]
struct Told {
    size: Option<TermUp>,
    focus: Option<TermUp>,
}

impl Told {
    fn note(&mut self, up: &TermUp) {
        match up {
            TermUp::Resize { .. } => self.size = Some(up.clone()),
            TermUp::Focus(_) => self.focus = Some(up.clone()),
            _ => {}
        }
    }

    fn again(&self) -> impl Iterator<Item = &TermUp> {
        self.size.iter().chain(&self.focus)
    }
}

/// Attaches `pane` through `client` on the side runtime and returns what the pane talks to: the
/// holder's messages, bounded at twice the holder's unacked window, and the channel for what the
/// pane says. `refused` counts the input the `Term` handed back (past 64 KiB it has not been able to
/// deliver, a long way into an outage): such input is dropped. A stream that fails before its first
/// frame is opened again (see [`retry_after`]). The task ends when the pane drops its sender or the
/// pane exits.
pub fn spawn_term(
    rt: &tokio::runtime::Handle,
    client: Arc<Client>,
    pane: PaneId,
    refused: Arc<AtomicU64>,
) -> (mpsc::Receiver<TermDown>, mpsc::UnboundedSender<TermUp>) {
    let (down_tx, down_rx) = mpsc::channel(2 * WINDOW);
    let (up_tx, mut up_rx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let mut term = client.open_term(pane).await;
        let mut opened = false;
        // A frame has come (the pane is there), and how often the stream was opened again for want
        // of one.
        let (mut seen, mut tried) = (false, 0);
        let mut told = Told::default();
        loop {
            tokio::select! {
                down = term.recv() => match down {
                    Some(msg) => {
                        if let Some(wait) = retry_after(&msg, seen, tried) {
                            tried += 1;
                            tokio::time::sleep(wait).await;
                            term = client.open_term(pane).await;
                            for up in told.again() {
                                let _ = term.send(up.clone());
                            }
                            continue;
                        }
                        seen |= matches!(msg, TermDown::Frame(_));
                        if down_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                up = up_rx.recv() => match up.map(|up| route(up, &mut opened)) {
                    None => break,
                    Some(Route::Ack(rev)) => term.applied(rev),
                    Some(Route::Send(up)) => {
                        told.note(&up);
                        if term.send(up).is_err() {
                            refused.fetch_add(1, Relaxed);
                        }
                    }
                    // Dropping the old `Term` detaches it; the new one asks for a full snapshot.
                    Some(Route::Reopen) => term = client.open_term(pane).await,
                    Some(Route::Skip) => {}
                },
            }
        }
    });
    (down_rx, up_tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_client::Config;
    use femme_proto::term::{Key, KeyCode, Mods};
    use femme_proto::Id;
    use femme_transport::DeviceIdentity;

    fn open() -> TermUp {
        TermUp::Open {
            pane: PaneId { sid: Id(1), no: 1 },
            epoch: 0,
            have_rev: 0,
            client_instance: 1,
        }
    }

    fn key() -> TermUp {
        TermUp::Key {
            seq: 0,
            key: Key {
                code: KeyCode::Enter,
                mods: Mods::empty(),
            },
        }
    }

    #[test]
    fn the_first_open_is_the_terms_own_and_later_ones_are_resyncs() {
        let mut opened = false;
        assert_eq!(route(open(), &mut opened), Route::Skip);
        assert_eq!(route(TermUp::Ack { rev: 7 }, &mut opened), Route::Ack(7));
        assert_eq!(route(key(), &mut opened), Route::Send(key()));
        assert_eq!(route(open(), &mut opened), Route::Reopen);
        assert_eq!(route(open(), &mut opened), Route::Reopen);
        let resize = TermUp::Resize { cols: 80, rows: 24 };
        assert_eq!(route(resize.clone(), &mut opened), Route::Send(resize));
        assert_eq!(
            route(TermUp::Focus(true), &mut opened),
            Route::Send(TermUp::Focus(true))
        );
    }

    fn exited(status: i32) -> TermDown {
        TermDown::Exited { status }
    }

    #[test]
    fn a_stream_that_fails_before_its_first_frame_is_opened_again() {
        // The daemon lists a new session's pane a moment before its holder listens.
        let wait = |tried| retry_after(&exited(-1), false, tried);
        assert_eq!(wait(0), Some(RETRY_FIRST));
        assert_eq!(wait(1), Some(RETRY_FIRST * 2));
        assert_eq!(wait(RETRIES - 1), Some(RETRY_MAX), "the wait stops growing");
        assert_eq!(wait(RETRIES), None, "and so do the tries");
        // Once a frame has come the pane is there: an exit is the pane's own.
        assert_eq!(retry_after(&exited(-1), true, 0), None);
        assert_eq!(retry_after(&exited(0), false, 0), None, "the program ended");
        assert_eq!(retry_after(&TermDown::Bell, false, 0), None);
    }

    #[test]
    fn a_stream_opened_again_is_told_the_last_size_and_focus() {
        let mut told = Told::default();
        assert_eq!(told.again().count(), 0);
        for up in [
            TermUp::Resize { cols: 80, rows: 24 },
            key(),
            TermUp::Focus(true),
            TermUp::Resize {
                cols: 100,
                rows: 30,
            },
            TermUp::Focus(false),
        ] {
            told.note(&up);
        }
        assert_eq!(
            told.again().cloned().collect::<Vec<_>>(),
            [
                TermUp::Resize {
                    cols: 100,
                    rows: 30
                },
                TermUp::Focus(false)
            ]
        );
    }

    /// A client that has nowhere to connect: the pump runs, takes everything the pane says
    /// (including a second `Open`, which swaps the `Term`) and ends when the pane lets go.
    #[tokio::test]
    async fn the_pump_takes_what_the_pane_says_and_ends_with_it() {
        let cfg = Config::new(DeviceIdentity::generate("test"), ulid::Ulid::generate());
        let client = Arc::new(Client::start(cfg));
        let refused = Arc::new(AtomicU64::new(0));
        let (mut down, up) = spawn_term(
            &tokio::runtime::Handle::current(),
            client,
            PaneId { sid: Id(1), no: 1 },
            refused.clone(),
        );
        for msg in [open(), key(), TermUp::Ack { rev: 1 }, open(), key()] {
            up.send(msg).unwrap();
        }
        // Past the cap the term refuses input: a paste of 70 KiB does not fit with the rest.
        up.send(TermUp::Paste {
            seq: 0,
            text: "x".repeat(70 << 10),
        })
        .unwrap();
        drop(up);
        let end = tokio::time::timeout(Duration::from_secs(5), down.recv()).await;
        assert_eq!(end, Ok(None), "the pump ends and closes the pane's channel");
        assert_eq!(refused.load(Relaxed), 1);
    }
}
