//! Checks every `Conn` implementation must pass, run against QUIC and the mux alike.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use femme_proto::rpc::{C2S, S2C};
use femme_proto::term::TermDown;
use femme_proto::{Id, PaneId};
use femme_transport::{Conn, Error, StreamKind, MAX_DATAGRAM};
use tokio::time::timeout;

/// Incompressible bytes (Bulk streams compress, and zeros would shrink to nothing).
pub fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

const WAIT: Duration = Duration::from_secs(10);

/// `client` and `server` are the two ends of one live connection. Ends with the connection closed.
pub async fn conformance(client: Arc<dyn Conn>, server: Arc<dyn Conn>) {
    let (c, s) = (client, server);

    // Kinds route, and messages round-trip in both directions.
    let (mut ctx, mut crx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    let hello = C2S::Hello {
        proto: 1,
        client_instance: 7,
        cursors: Default::default(),
    };
    ctx.send(&hello).await.unwrap();
    let (kind, mut stx, mut srx) = timeout(WAIT, s.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    assert_eq!(kind, StreamKind::Ctrl);
    assert_eq!(srx.recv_msg::<C2S>().await.unwrap(), Some(hello));
    let welcome = S2C::Welcome {
        proto: 1,
        server_version: "conformance".into(),
        epoch: 3,
    };
    stx.send(&welcome).await.unwrap();
    assert_eq!(crx.recv_msg::<S2C>().await.unwrap(), Some(welcome));

    // The server opens streams too. Acceptance order is not promised, so match by kind.
    let pane = PaneId {
        sid: Id::new(),
        no: 2,
    };
    let (mut ttx, _trx) = s.open_kind(StreamKind::Term(pane)).await.unwrap();
    let (mut btx, _brx) = s.open_kind(StreamKind::Bulk(9)).await.unwrap();
    ttx.send(&TermDown::Bell).await.unwrap();
    btx.send_bytes(b"blob").await.unwrap();
    let mut got = HashMap::new();
    for _ in 0..2 {
        let (kind, _tx, rx) = timeout(WAIT, c.accept())
            .await
            .expect("accept stalled")
            .unwrap();
        got.insert(kind, rx);
    }
    assert_eq!(
        got.remove(&StreamKind::Term(pane))
            .unwrap()
            .recv_msg::<TermDown>()
            .await
            .unwrap(),
        Some(TermDown::Bell)
    );
    assert_eq!(
        got.remove(&StreamKind::Bulk(9))
            .unwrap()
            .recv()
            .await
            .unwrap(),
        Some(Bytes::from_static(b"blob"))
    );

    // finish ends a stream after its data; so does dropping the Tx; writing after finish fails.
    let (mut tx, _rx) = c.open_kind(StreamKind::Events).await.unwrap();
    tx.send(&1u32).await.unwrap();
    tx.send(&2u32).await.unwrap();
    tx.finish().await.unwrap();
    tx.finish().await.unwrap();
    assert!(
        matches!(tx.send(&3u32).await, Err(Error::Closed(_))),
        "write after finish"
    );
    let (_, _stx, mut srx) = timeout(WAIT, s.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(1));
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(2));
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), None);
    assert_eq!(
        srx.recv_msg::<u32>().await.unwrap(),
        None,
        "and it stays ended"
    );
    let (mut tx, _rx) = c.open_kind(StreamKind::Events).await.unwrap();
    tx.send(&9u32).await.unwrap();
    drop(tx);
    let (_, _stx, mut srx) = timeout(WAIT, s.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(9));
    assert_eq!(
        srx.recv_msg::<u32>().await.unwrap(),
        None,
        "dropping a Tx finishes the stream"
    );

    // reset aborts one direction only.
    let (mut tx, mut rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&1u32).await.unwrap();
    let (_, mut stx, mut srx) = timeout(WAIT, s.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(1));
    tx.reset();
    assert!(matches!(srx.recv_msg::<u32>().await, Err(Error::Reset)));
    stx.send(&5u32).await.unwrap();
    assert_eq!(
        rx.recv_msg::<u32>().await.unwrap(),
        Some(5),
        "the other direction still works"
    );

    // Dropping the Rx stops the sender.
    let (mut tx, _rx) = c.open_kind(StreamKind::Bulk(1)).await.unwrap();
    tx.send_bytes(&random(100, 1)).await.unwrap();
    let (_, _stx, srx) = timeout(WAIT, s.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    drop(srx);
    let err = timeout(WAIT, async {
        loop {
            if let Err(e) = tx.send_bytes(&random(1000, 2)).await {
                break e;
            }
        }
    })
    .await
    .expect("the sender was never told to stop");
    assert!(matches!(err, Error::Reset), "got {err:?}");

    // Datagrams both ways; the size limit is the same on both transports.
    c.send_datagram(Bytes::from_static(b"up")).unwrap();
    s.send_datagram(Bytes::from_static(b"down")).unwrap();
    assert_eq!(
        timeout(WAIT, s.recv_datagram())
            .await
            .expect("datagram lost")
            .unwrap(),
        Bytes::from_static(b"up")
    );
    assert_eq!(
        timeout(WAIT, c.recv_datagram())
            .await
            .expect("datagram lost")
            .unwrap(),
        Bytes::from_static(b"down")
    );
    assert!(matches!(
        c.send_datagram(Bytes::from(vec![0u8; MAX_DATAGRAM + 1])),
        Err(Error::Datagram(_))
    ));
    c.send_datagram(Bytes::from(vec![1u8; MAX_DATAGRAM]))
        .unwrap();
    assert_eq!(
        timeout(WAIT, s.recv_datagram())
            .await
            .expect("datagram lost")
            .unwrap()
            .len(),
        MAX_DATAGRAM
    );

    // close: both ends learn of it, and later calls fail. quinn withholds CONNECTION_CLOSE while the
    // sender's congestion window is full of unacknowledged data (its trace says "blocked by congestion
    // control"), which is where the bulk loop above leaves BBR; the peer would then only find out by
    // idle timeout. So let the connection go quiet first (about 200 ms is enough on loopback).
    tokio::time::sleep(Duration::from_millis(500)).await;
    c.close();
    c.close();
    assert!(matches!(c.closed().await, Error::Closed(_)));
    assert!(matches!(
        timeout(WAIT, s.closed())
            .await
            .expect("the peer never noticed"),
        Error::Closed(_)
    ));
    assert!(c.open_kind(StreamKind::Ctrl).await.is_err());
    assert!(c.accept().await.is_err());
    assert!(c.send_datagram(Bytes::from_static(b"x")).is_err());
    assert!(c.recv_datagram().await.is_err());
    assert!(s.accept().await.is_err());
}
