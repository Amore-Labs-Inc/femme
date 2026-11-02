//! The pump between a pane and the daemon's terminal stream: what the pane says upward goes to the
//! client's [`Term`](femme_client::Term), what the holder sends comes back down. It runs on the
//! side runtime and hands the pane the same two channels a synthetic source does.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

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

/// Attaches `pane` through `client` on the side runtime and returns what the pane talks to: the
/// holder's messages, bounded at twice the holder's unacked window, and the channel for what the
/// pane says. `refused` counts the input the `Term` handed back (past 64 KiB it has not been able to
/// deliver, a long way into an outage): such input is dropped. The task ends when the pane drops
/// its sender or the pane exits.
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
        loop {
            tokio::select! {
                down = term.recv() => match down {
                    Some(msg) => {
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
    use std::time::Duration;

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
