//! The SSH-side multiplexer over in-memory pipes (and one real child process).

mod common;

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use femme_proto::codec::{get_varint, put_varint, Framer};
use femme_proto::rpc::{C2S, S2C};
use femme_proto::term::{TermDown, TermUp};
use femme_proto::{Id, PaneId};
use femme_transport::{
    command_connect, mux_client, mux_server, prio, Conn, Error, StreamKind, MAX_DATAGRAM,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::time::{sleep, timeout};

const OPEN: u8 = 1;
const FIN: u8 = 2;
const RST: u8 = 4;
const DGRAM: u8 = 8;
const WINDOW: u8 = 16;
const STOP: u8 = 32;
const PING: u8 = 64;
const PONG: u8 = 128;

fn pair() -> (Arc<dyn Conn>, Arc<dyn Conn>) {
    let (a, b) = tokio::io::duplex(4096);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    (mux_client(ar, aw), mux_server(br, bw))
}

#[tokio::test]
async fn conformance_over_the_mux() {
    let (c, s) = pair();
    common::conformance(c, s).await;
}

/// A mux server whose peer is us, speaking raw frames.
fn raw_server() -> (Arc<dyn Conn>, DuplexStream) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (br, bw) = tokio::io::split(b);
    (mux_server(br, bw), a)
}

/// A mux client whose peer is us, speaking raw frames.
fn raw_client() -> (Arc<dyn Conn>, DuplexStream) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    (mux_client(ar, aw), b)
}

fn frame(id: u64, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    put_varint(&mut v, id);
    v.push(flags);
    put_varint(&mut v, data.len() as u64);
    v.extend_from_slice(data);
    v
}

/// The first frame of a stream: its kind, as the codec frames it.
fn kind_frame(kind: StreamKind) -> Vec<u8> {
    let mut out = Vec::new();
    Framer::new(true).frame(&kind, &mut out).unwrap();
    out
}

/// Read one mux frame from `raw`, buffering in `buf`.
async fn read_frame(raw: &mut DuplexStream, buf: &mut Vec<u8>) -> Option<(u64, u8, Vec<u8>)> {
    loop {
        if let Ok(Some((id, n1))) = get_varint(buf) {
            if buf.len() > n1 {
                if let Ok(Some((len, n2))) = get_varint(&buf[n1 + 1..]) {
                    let head = n1 + 1 + n2;
                    if buf.len() >= head + len as usize {
                        let flags = buf[n1];
                        let data = buf[head..head + len as usize].to_vec();
                        buf.drain(..head + len as usize);
                        return Some((id, flags, data));
                    }
                }
            }
        }
        let mut chunk = [0u8; 4096];
        let n = raw.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Incompressible bytes (the Bulk streams compress, and zeros would shrink to nothing).
fn random(n: usize, seed: u64) -> Vec<u8> {
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

#[tokio::test]
async fn streams_open_and_accept_in_both_directions() {
    let (c, s) = pair();
    let (mut ctx, mut crx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    let hello = C2S::Hello {
        proto: 1,
        client_instance: 7,
        cursors: Default::default(),
    };
    ctx.send(&hello).await.unwrap();
    let (kind, mut stx, mut srx) = s.accept().await.unwrap();
    assert_eq!(kind, StreamKind::Ctrl);
    assert_eq!(srx.recv_msg::<C2S>().await.unwrap(), Some(hello));
    let welcome = S2C::Welcome {
        proto: 1,
        server_version: "test".into(),
        epoch: 3,
    };
    stx.send(&welcome).await.unwrap();
    assert_eq!(crx.recv_msg::<S2C>().await.unwrap(), Some(welcome));

    // The server opens too (even ids), and kinds arrive in the order they were opened.
    let pane = PaneId {
        sid: Id::new(),
        no: 2,
    };
    let (mut ttx, _trx) = s.open_kind(StreamKind::Term(pane)).await.unwrap();
    let (mut btx, _brx) = s.open_kind(StreamKind::Bulk(9)).await.unwrap();
    ttx.send(&TermDown::Bell).await.unwrap();
    btx.send_bytes(b"blob").await.unwrap();
    let (k1, _t1, mut r1) = c.accept().await.unwrap();
    let (k2, _t2, mut r2) = c.accept().await.unwrap();
    assert_eq!((k1, k2), (StreamKind::Term(pane), StreamKind::Bulk(9)));
    assert_eq!(
        r1.recv_msg::<TermDown>().await.unwrap(),
        Some(TermDown::Bell)
    );
    assert_eq!(r2.recv().await.unwrap(), Some(Bytes::from_static(b"blob")));
}

#[tokio::test]
async fn finish_ends_a_stream_after_its_data_and_dropping_does_the_same() {
    let (c, s) = pair();
    let (mut tx, _rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&1u32).await.unwrap();
    tx.send(&2u32).await.unwrap();
    tx.finish().await.unwrap();
    tx.finish().await.unwrap(); // twice is fine
    assert!(
        matches!(tx.send(&3u32).await, Err(Error::Closed(_))),
        "writing after finish fails"
    );
    let (_, _stx, mut srx) = s.accept().await.unwrap();
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
    let (_, _stx, mut srx) = s.accept().await.unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(9));
    assert_eq!(
        srx.recv_msg::<u32>().await.unwrap(),
        None,
        "dropping a Tx finishes the stream"
    );
}

#[tokio::test]
async fn reset_aborts_one_direction_only() {
    let (c, s) = pair();
    let (mut tx, mut rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&1u32).await.unwrap();
    let (_, mut stx, mut srx) = s.accept().await.unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(1));
    tx.reset();
    assert!(matches!(srx.recv_msg::<u32>().await, Err(Error::Reset)));
    assert!(
        matches!(srx.recv_msg::<u32>().await, Err(Error::Reset)),
        "and it stays reset"
    );
    stx.send(&5u32).await.unwrap();
    assert_eq!(
        rx.recv_msg::<u32>().await.unwrap(),
        Some(5),
        "the other direction still works"
    );
}

#[tokio::test]
async fn dropping_the_receiver_stops_the_sender() {
    let (c, s) = pair();
    let (mut tx, _rx) = c.open_kind(StreamKind::Bulk(1)).await.unwrap();
    tx.send_bytes(&random(100, 1)).await.unwrap();
    let (_, _stx, srx) = s.accept().await.unwrap();
    drop(srx);
    let err = timeout(Duration::from_secs(5), async {
        loop {
            if let Err(e) = tx.send_bytes(&random(1000, 2)).await {
                break e;
            }
        }
    })
    .await
    .expect("the sender must be told to stop");
    assert!(matches!(err, Error::Reset), "got {err:?}");
}

#[tokio::test]
async fn sixty_four_kib_of_credit_blocks_the_sender_until_the_receiver_reads() {
    let (c, s) = pair();
    let (mut tx, _rx) = c.open_kind(StreamKind::Bulk(1)).await.unwrap();
    let (_, _stx, mut srx) = s.accept().await.unwrap();
    let (first, second) = (random(60_000, 3), random(20_000, 4));
    tx.send_bytes(&first).await.unwrap(); // fits in the 64 KiB window
    let second2 = second.clone();
    let blocked = tokio::spawn(async move {
        tx.send_bytes(&second2).await.unwrap();
        tx
    });
    sleep(300 * MS).await;
    assert!(
        !blocked.is_finished(),
        "the second frame went through without credit"
    );
    assert_eq!(srx.recv().await.unwrap().unwrap(), Bytes::from(first));
    let _tx = timeout(Duration::from_secs(5), blocked)
        .await
        .expect("credit never came")
        .unwrap();
    assert_eq!(srx.recv().await.unwrap().unwrap(), Bytes::from(second));
}

const MS: Duration = Duration::from_millis(1);

#[tokio::test]
async fn a_large_transfer_arrives_intact_in_both_directions_at_once() {
    const TOTAL: usize = 3 << 20;
    const PART: usize = 100_000;
    let (c, s) = pair();
    let (mut ctx, mut crx) = c.open_kind(StreamKind::Bulk(1)).await.unwrap();
    let up = tokio::spawn(async move {
        for i in 0..TOTAL / PART {
            ctx.send_bytes(&random(PART, i as u64 + 10)).await.unwrap();
        }
        ctx.finish().await.unwrap();
        ctx
    });
    let (_, mut stx, mut srx) = s.accept().await.unwrap();
    let down = tokio::spawn(async move {
        for i in 0..TOTAL / PART {
            stx.send_bytes(&random(PART, i as u64 + 500)).await.unwrap();
        }
        stx.finish().await.unwrap();
        stx
    });
    let reader = tokio::spawn(async move {
        let mut n = 0;
        while let Some(b) = srx.recv().await.unwrap() {
            assert_eq!(
                b,
                Bytes::from(random(PART, n as u64 + 10)),
                "upload part {n}"
            );
            n += 1;
        }
        assert_eq!(n, TOTAL / PART);
    });
    let mut n = 0;
    while let Some(b) = crx.recv().await.unwrap() {
        assert_eq!(
            b,
            Bytes::from(random(PART, n as u64 + 500)),
            "download part {n}"
        );
        n += 1;
    }
    assert_eq!(n, TOTAL / PART);
    reader.await.unwrap();
    let _ = (up.await.unwrap(), down.await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn datagrams_flow_both_ways_and_the_oldest_are_dropped_when_nobody_reads() {
    let (c, s) = pair();
    c.send_datagram(Bytes::from_static(b"up")).unwrap();
    s.send_datagram(Bytes::from_static(b"down")).unwrap();
    assert_eq!(s.recv_datagram().await.unwrap(), Bytes::from_static(b"up"));
    assert_eq!(
        c.recv_datagram().await.unwrap(),
        Bytes::from_static(b"down")
    );

    assert!(matches!(
        c.send_datagram(Bytes::from(vec![0u8; MAX_DATAGRAM + 1])),
        Err(Error::Datagram(_))
    ));
    c.send_datagram(Bytes::from(vec![0u8; MAX_DATAGRAM]))
        .unwrap();
    assert_eq!(s.recv_datagram().await.unwrap().len(), MAX_DATAGRAM);

    // 200 datagrams with nobody awaiting: the queue keeps the newest 64.
    for i in 0..200u8 {
        c.send_datagram(Bytes::from(vec![i])).unwrap();
    }
    sleep(100 * MS).await;
    let mut got = Vec::new();
    while let Ok(d) = timeout(50 * MS, s.recv_datagram()).await {
        got.push(d.unwrap()[0]);
    }
    assert!(
        got.len() <= 64 && got.len() >= 32,
        "kept {} datagrams",
        got.len()
    );
    assert_eq!(*got.last().unwrap(), 199, "the newest survives");
    assert!(
        got.windows(2).all(|w| w[0] < w[1]),
        "and they stay in order"
    );
}

#[tokio::test]
async fn both_ends_measure_rtt_from_their_own_pings() {
    let (c, s) = pair();
    assert!(
        c.rtt().is_zero() && s.rtt().is_zero(),
        "no sample before the first pong"
    );
    timeout(Duration::from_secs(3), async {
        while c.rtt().is_zero() || s.rtt().is_zero() {
            sleep(5 * MS).await;
        }
    })
    .await
    .expect("no pong came back");
    assert!(c.rtt() < Duration::from_secs(1) && s.rtt() < Duration::from_secs(1));
    assert!(!c.is_quic() && !s.is_quic());
}

#[tokio::test(start_paused = true)]
async fn a_duplicated_datagram_arrives_twice_the_second_copy_one_gap_later() {
    let (c, s) = pair();
    let (gap, t0) = (Duration::from_millis(10), tokio::time::Instant::now());
    c.clone().send_datagram_dup(Bytes::from_static(b"key"), gap);
    assert_eq!(s.recv_datagram().await.unwrap(), Bytes::from_static(b"key"));
    let first = t0.elapsed();
    assert_eq!(s.recv_datagram().await.unwrap(), Bytes::from_static(b"key"));
    assert!(
        t0.elapsed() - first >= gap,
        "the copies came {:?} apart",
        t0.elapsed() - first
    );
    assert!(
        timeout(Duration::from_secs(1), s.recv_datagram())
            .await
            .is_err(),
        "more than two copies"
    );
}

#[tokio::test(start_paused = true)]
async fn in_the_background_the_mux_pings_every_15_seconds_and_at_once_when_it_comes_back() {
    let pings = |seen: &[(u64, u8)]| seen.iter().filter(|f| **f == (0, PING)).count();
    let (client, raw) = raw_client();
    let (raw, seen) = answer_pings(raw, Duration::from_secs(10)).await;
    assert!(
        pings(&seen) >= 9,
        "a ping a second in the foreground, saw {}",
        pings(&seen)
    );

    // The switch pings once by itself, then one every 15 s.
    client.set_background(true);
    let (raw, seen) = answer_pings(raw, Duration::from_secs(20)).await;
    assert!(
        (1..=3).contains(&pings(&seen)),
        "{} pings in 20 s of background",
        pings(&seen)
    );

    // Coming back does not wait for the next 15 s tick (the next one is 10 s away).
    client.set_background(false);
    let (_raw, seen) = answer_pings(raw, 100 * MS).await;
    assert!(
        pings(&seen) >= 1,
        "coming back to the foreground did not ping at once"
    );
}

#[tokio::test]
async fn close_is_seen_by_both_ends_and_fails_later_calls() {
    let (c, s) = pair();
    let (_tx, _rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    c.close();
    c.close(); // idempotent
    assert!(matches!(c.closed().await, Error::Closed(_)));
    assert!(matches!(
        timeout(Duration::from_secs(2), s.closed())
            .await
            .expect("server never noticed"),
        Error::Closed(_)
    ));
    assert!(c.open_kind(StreamKind::Ctrl).await.is_err());
    assert!(c.accept().await.is_err());
    assert!(c.send_datagram(Bytes::from_static(b"x")).is_err());
    assert!(c.recv_datagram().await.is_err());
    assert!(s.accept().await.is_err());
}

#[tokio::test]
async fn the_connection_lives_while_any_stream_does_and_closes_with_the_last_handle() {
    let (c, s) = pair();
    let (mut tx, rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    let (_, _stx, mut srx) = s.accept().await.unwrap();
    drop(c);
    tx.send(&1u32).await.unwrap();
    assert_eq!(
        srx.recv_msg::<u32>().await.unwrap(),
        Some(1),
        "a live stream keeps the connection up"
    );
    assert!(timeout(100 * MS, s.closed()).await.is_err());
    drop(tx);
    drop(rx);
    let e = timeout(Duration::from_secs(2), s.closed())
        .await
        .expect("the server never saw the connection close");
    assert!(matches!(e, Error::Closed(_)));
}

#[tokio::test]
async fn dropping_the_last_handle_still_delivers_what_was_queued_and_the_end_of_the_stream() {
    let (c, s) = pair();
    let (mut tx, rx) = c.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&"last words").await.unwrap();
    drop((tx, rx, c)); // the end of the stream is queued behind the data, then the connection goes
    let (_, _stx, mut srx) = timeout(Duration::from_secs(2), s.accept())
        .await
        .expect("accept stalled")
        .expect("the stream was lost with the connection");
    assert_eq!(
        srx.recv_msg::<String>().await.unwrap().as_deref(),
        Some("last words")
    );
    assert_eq!(
        srx.recv_msg::<String>().await.unwrap(),
        None,
        "and the end of the stream after it"
    );
    let e = timeout(Duration::from_secs(2), s.closed())
        .await
        .expect("the server never saw the connection close");
    assert!(matches!(e, Error::Closed(_)));
}

#[tokio::test]
async fn recv_and_accept_are_cancel_safe() {
    let (c, s) = pair();
    let (mut tx, _rx) = c.open_kind(StreamKind::Bulk(1)).await.unwrap();
    let (_, _stx, mut srx) = s.accept().await.unwrap();
    let big = random(300_000, 8);
    let big2 = big.clone();
    let sender = tokio::spawn(async move {
        tx.send_bytes(&big2).await.unwrap();
        tx
    });
    // Abandon `recv` over and over while the frame trickles in; nothing may be lost.
    let got = loop {
        if let Ok(r) = timeout(Duration::from_micros(200), srx.recv()).await {
            break r.unwrap().unwrap();
        }
    };
    assert_eq!(got, Bytes::from(big));
    let _tx = sender.await.unwrap();

    let opener = {
        let c = c.clone();
        tokio::spawn(async move {
            let mut keep = Vec::new();
            for i in 0..20 {
                keep.push(c.open_kind(StreamKind::Bulk(100 + i)).await.unwrap());
                tokio::task::yield_now().await;
            }
            keep
        })
    };
    let mut seen = Vec::new();
    while seen.len() < 20 {
        if let Ok(r) = timeout(Duration::from_micros(100), s.accept()).await {
            seen.push(r.unwrap().0);
        }
    }
    let want: Vec<StreamKind> = (0..20).map(|i| StreamKind::Bulk(100 + i)).collect();
    assert_eq!(seen, want, "each stream accepted exactly once, in order");
    let _ = opener.await.unwrap();
}

#[tokio::test]
async fn at_most_128_streams_are_open_at_once_and_the_129th_waits() {
    let (c, s) = pair();
    // The server reads each stream to its end and then lets go of it.
    tokio::spawn(async move {
        while let Ok((_, tx, mut rx)) = s.accept().await {
            tokio::spawn(async move {
                let _tx = tx;
                while let Ok(Some(_)) = rx.recv().await {}
            });
        }
    });
    let mut open = Vec::new();
    for i in 0..128 {
        open.push(c.open_kind(StreamKind::Bulk(i)).await.unwrap());
    }
    let c2 = c.clone();
    let waiting =
        tokio::spawn(async move { c2.open_kind(StreamKind::Bulk(999)).await.map(|_| ()) });
    sleep(200 * MS).await;
    assert!(!waiting.is_finished(), "a 129th stream was opened");
    drop(open.pop());
    timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the freed slot never reached the waiter")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_burst_of_short_streams_loses_none_even_when_the_acceptor_is_slow() {
    const N: u32 = 1000;
    let (c, s) = pair();
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        while seen.len() < N as usize {
            let (_, _tx, mut rx) = s.accept().await.unwrap();
            seen.push(rx.recv_msg::<u32>().await.unwrap().expect("a message"));
            tokio::task::yield_now().await; // a slow acceptor: opened streams pile up behind it
        }
        seen
    });
    let mut seen = timeout(Duration::from_secs(30), async {
        for i in 0..N {
            let (mut tx, _rx) = c.open_kind(StreamKind::Events).await.unwrap();
            tx.send(&i).await.unwrap();
        }
        server.await.unwrap()
    })
    .await
    .expect("opening stalled or the acceptor never saw every stream");
    seen.sort_unstable();
    assert_eq!(
        seen,
        (0..N).collect::<Vec<_>>(),
        "every stream arrived exactly once"
    );
}

#[tokio::test(start_paused = true)]
async fn stream_credit_comes_back_when_we_forget_a_stream_and_a_peer_without_it_is_cut_off() {
    let (server, mut raw) = raw_server();
    let kind = kind_frame(StreamKind::Ctrl);
    let mut wire = Vec::new();
    for n in 0..128u64 {
        let id = 2 * n + 1;
        wire.extend(frame(id, OPEN, b""));
        wire.extend(frame(id, 0, &kind));
    }
    raw.write_all(&wire).await.unwrap();
    let mut held = Vec::new();
    for _ in 0..128 {
        held.push(
            timeout(Duration::from_secs(5), server.accept())
                .await
                .expect("accept stalled")
                .unwrap(),
        );
    }
    assert!(
        timeout(100 * MS, server.closed()).await.is_err(),
        "128 streams are within the limit"
    );

    // The peer ends one stream and we let go of it: that is the credit for one more.
    raw.write_all(&frame(1, FIN, b"")).await.unwrap();
    drop(held.remove(0));
    let mut buf = Vec::new();
    timeout(Duration::from_secs(5), async {
        while let Some((id, flags, data)) = read_frame(&mut raw, &mut buf).await {
            if (id, flags) == (0, WINDOW) {
                assert_eq!(data, [1], "one stream's worth");
                return;
            }
        }
        panic!("the pipe closed");
    })
    .await
    .expect("the credit never came back");
    raw.write_all(&[frame(257, OPEN, b""), frame(257, 0, &kind)].concat())
        .await
        .unwrap();
    held.push(
        timeout(Duration::from_secs(5), server.accept())
            .await
            .expect("accept stalled")
            .unwrap(),
    );
    assert!(timeout(100 * MS, server.closed()).await.is_err());

    // Out of credit again: one more OPEN is a protocol violation.
    raw.write_all(&frame(259, OPEN, b"")).await.unwrap();
    let e = timeout(Duration::from_secs(2), server.closed())
        .await
        .expect("the excess stream was tolerated");
    assert!(
        matches!(&e, Error::Proto(m) if m.contains("credit")),
        "got {e:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn late_frames_for_a_stream_we_already_forgot_are_ignored() {
    let (server, mut raw) = raw_server();
    let kind = kind_frame(StreamKind::Ctrl);
    raw.write_all(&[frame(1, OPEN, b""), frame(1, 0, &kind), frame(1, FIN, b"")].concat())
        .await
        .unwrap();
    drop(server.accept().await.unwrap()); // both handles gone: the stream is forgotten
    sleep(100 * MS).await;
    // What a slow peer sends after we let go: credit, a stop, data, an end, a reset.
    let late = [
        frame(1, WINDOW, &[5]),
        frame(1, STOP, b""),
        frame(1, 0, b"late"),
        frame(1, FIN, b""),
        frame(1, RST, b""),
    ]
    .concat();
    raw.write_all(&late).await.unwrap();
    // The reader is still alive: a new stream gets through.
    raw.write_all(&[frame(3, OPEN, b""), frame(3, 0, &kind)].concat())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), server.accept())
        .await
        .expect("accept stalled")
        .unwrap();
    assert!(
        timeout(100 * MS, server.closed()).await.is_err(),
        "a late frame killed the connection"
    );
}

#[tokio::test(start_paused = true)]
async fn protocol_violations_close_the_connection_with_a_proto_error() {
    async fn closes_with_proto(wire: Vec<u8>, what: &str) {
        let (server, mut raw) = raw_server();
        raw.write_all(&wire).await.unwrap();
        let e = timeout(Duration::from_secs(2), server.closed())
            .await
            .unwrap_or_else(|_| panic!("{what}: the connection stayed open"));
        assert!(matches!(e, Error::Proto(_)), "{what}: got {e:?}");
        // Everything else reports `Closed`, as the crate docs promise, with the reason in the text.
        assert!(
            matches!(
                server.open_kind(StreamKind::Ctrl).await,
                Err(Error::Closed(_))
            ),
            "{what}: open"
        );
        assert!(
            matches!(
                server.send_datagram(Bytes::from_static(b"x")),
                Err(Error::Closed(_))
            ),
            "{what}: send_datagram"
        );
        assert!(
            matches!(server.recv_datagram().await, Err(Error::Closed(_))),
            "{what}: recv_datagram"
        );
        let accept_err = loop {
            match timeout(Duration::from_secs(2), server.accept())
                .await
                .unwrap_or_else(|_| panic!("{what}: accept hangs"))
            {
                Ok(_) => {} // a stream that was complete before the violation may still be queued
                Err(e) => break e,
            }
        };
        assert!(
            matches!(&accept_err, Error::Closed(m) if m.contains("protocol error")),
            "{what}: accept gave {accept_err:?}"
        );
    }
    let kind = kind_frame(StreamKind::Ctrl);

    closes_with_proto(
        [frame(1, OPEN, b""), frame(1, 3, b"")].concat(),
        "two flags in one frame",
    )
    .await;
    closes_with_proto(
        [frame(1, 0, b"")[..2].to_vec(), vec![0x81, 0x80, 0x01]].concat(),
        "a frame over 16 KiB",
    )
    .await;
    closes_with_proto(frame(2, OPEN, b""), "an even id from the client").await;
    closes_with_proto(
        [frame(3, OPEN, b""), frame(1, OPEN, b"")].concat(),
        "ids going backwards",
    )
    .await;
    closes_with_proto(frame(0, FIN, b""), "an unknown connection-level frame").await;
    closes_with_proto(frame(0, WINDOW, b""), "a truncated connection-level WINDOW").await;
    closes_with_proto(
        [frame(1, OPEN, b""), frame(1, WINDOW, b"")].concat(),
        "a truncated WINDOW",
    )
    .await;
    closes_with_proto(
        frame(0, WINDOW, &[1]),
        "stream credit past the initial limit",
    )
    .await;
    closes_with_proto(
        [frame(1, OPEN, b""), frame(1, WINDOW, &[1])].concat(),
        "byte credit past the initial window",
    )
    .await;
    closes_with_proto(
        [frame(1, OPEN, b""), frame(1, OPEN, b"")].concat(),
        "the same stream opened twice",
    )
    .await;
    closes_with_proto(
        frame(u64::MAX, OPEN, b""),
        "the highest id, which has no successor",
    )
    .await;
    closes_with_proto(frame(5, 0, b"x"), "data for a stream nobody opened").await;
    closes_with_proto(
        frame(2, STOP, b""),
        "a stop for a server stream the server never opened",
    )
    .await;
    closes_with_proto(
        frame(7, PONG, b""),
        "a connection-level flag on a stream id",
    )
    .await;
    closes_with_proto(frame(7, 3, b""), "two flags on an id nobody opened").await;
    closes_with_proto(vec![0xff; 12], "an endless varint").await;
    closes_with_proto(
        [
            frame(1, OPEN, b""),
            frame(1, 0, &kind),
            frame(1, FIN, b""),
            frame(1, 0, b"x"),
        ]
        .concat(),
        "data after FIN",
    )
    .await;

    // Sending past the 64 KiB credit of a stream nobody is reading.
    let (server, mut raw) = raw_server();
    raw.write_all(&[frame(1, OPEN, b""), frame(1, 0, &kind)].concat())
        .await
        .unwrap();
    let (_, mut tx, _rx) = server.accept().await.unwrap();
    let mut flood = Vec::new();
    for _ in 0..5 {
        flood.extend(frame(1, 0, &[7u8; 16 << 10]));
    }
    raw.write_all(&flood).await.unwrap();
    let e = timeout(Duration::from_secs(2), server.closed())
        .await
        .expect("credit was not enforced");
    assert!(
        matches!(&e, Error::Proto(m) if m.contains("credit")),
        "got {e:?}"
    );
    // A live stream handle reports `Closed` too, with the reason.
    assert!(matches!(tx.send_bytes(b"x").await, Err(Error::Closed(m)) if m.contains("credit")));
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_never_names_its_kind_is_reset_and_blocks_nobody() {
    let (server, mut raw) = raw_server();
    raw.write_all(
        &[
            frame(1, OPEN, b""),
            frame(3, OPEN, b""),
            frame(3, 0, &kind_frame(StreamKind::Events)),
        ]
        .concat(),
    )
    .await
    .unwrap();
    // Stream 3 gets through while stream 1 says nothing.
    let (kind, _tx, _rx) = server.accept().await.unwrap();
    assert_eq!(kind, StreamKind::Events);
    // The peer itself stays alive (it answers pings) while stream 1 says nothing for 11 s.
    let (_raw, seen) = answer_pings(raw, Duration::from_secs(11)).await;
    assert!(seen.contains(&(1, RST)), "the silent stream was not reset");
}

/// Plays a mux peer: answers every PING for `how_long`, then goes silent without closing the pipe.
/// Returns the pipe and the `(id, flags)` of every frame that came in, pings included.
async fn answer_pings(mut raw: DuplexStream, how_long: Duration) -> (DuplexStream, Vec<(u64, u8)>) {
    let end = tokio::time::Instant::now() + how_long;
    let (mut buf, mut seen) = (Vec::new(), Vec::new());
    while let Ok(Some((id, flags, data))) =
        tokio::time::timeout_at(end, read_frame(&mut raw, &mut buf)).await
    {
        seen.push((id, flags));
        if (id, flags) == (0, PING) {
            raw.write_all(&frame(0, PONG, &data)).await.unwrap();
        }
    }
    (raw, seen)
}

#[tokio::test(start_paused = true)]
async fn a_kind_frame_that_claims_more_than_256_bytes_is_refused_before_it_arrives() {
    let (_server, mut raw) = raw_server();
    // OPEN, then only the start of a frame whose header says 1 MiB: the stream is reset at once,
    // not after the 10 s the kind frame is otherwise given (or the 1 MiB it asked for).
    let mut claim = Vec::new();
    put_varint(&mut claim, 1 << 20);
    raw.write_all(&[frame(1, OPEN, b""), frame(1, 0, &claim)].concat())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let reset = timeout(Duration::from_secs(1), async {
        while let Some((id, flags, _)) = read_frame(&mut raw, &mut buf).await {
            if (id, flags) == (1, RST) {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(reset, Ok(true)),
        "the oversized kind frame was not refused: {reset:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_stops_answering_pings_is_declared_dead_within_1_5_seconds() {
    let (a, _b) = tokio::io::duplex(4096); // the other end exists and says nothing
    let (ar, aw) = tokio::io::split(a);
    let client = mux_client(ar, aw);
    let started = tokio::time::Instant::now();
    let e = timeout(Duration::from_secs(10), client.closed())
        .await
        .expect("never declared dead");
    assert!(
        matches!(&e, Error::Closed(m) if m.contains("ping")),
        "got {e:?}"
    );
    assert!(
        started.elapsed() <= Duration::from_millis(1600),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn the_server_end_declares_a_silent_client_dead_too() {
    let (server, _raw) = raw_server(); // the client exists and says nothing, forever
    let started = tokio::time::Instant::now();
    let e = timeout(Duration::from_secs(10), server.closed())
        .await
        .expect("a client that went silent was never declared dead");
    assert!(
        matches!(&e, Error::Closed(m) if m.contains("nothing came")),
        "got {e:?}"
    );
    assert!(
        started.elapsed() <= Duration::from_millis(1600),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_goes_silent_mid_session_is_dead_within_one_ping_interval_plus_1_5_seconds() {
    let (client, raw) = raw_client();
    let started = tokio::time::Instant::now();
    // Pings go out at 0, 1, 2, ... and are answered up to 4 s; the one at 5 s gets no answer.
    let (_raw, seen) = answer_pings(raw, Duration::from_millis(4500)).await;
    assert_eq!(
        seen.iter().filter(|f| **f == (0, PING)).count(),
        5,
        "the peer was alive for the first pings"
    );
    let e = timeout(Duration::from_secs(10), client.closed())
        .await
        .expect("never declared dead");
    assert!(
        matches!(&e, Error::Closed(m) if m.contains("nothing came")),
        "got {e:?}"
    );
    // Last heard at 4 s: one interval (the ping at 5 s) plus 1.5 s.
    assert!(
        started.elapsed() <= Duration::from_millis(6600),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn any_traffic_from_the_peer_counts_as_alive_not_only_ping_answers() {
    let (client, mut raw) = raw_client();
    // The peer never answers a ping, but a datagram every second shows it is there: 20 s go by.
    for _ in 0..20 {
        raw.write_all(&frame(0, DGRAM, b"hi")).await.unwrap();
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        timeout(100 * MS, client.closed()).await.is_err(),
        "declared dead while the peer was talking"
    );
    // Silent from here on: a ping interval plus 1.5 s later it is over.
    let e = timeout(Duration::from_secs(3), client.closed())
        .await
        .expect("never declared dead");
    assert!(
        matches!(&e, Error::Closed(m) if m.contains("nothing came")),
        "got {e:?}"
    );
}

#[tokio::test]
async fn every_datagram_waiter_wakes_when_the_connection_dies() {
    let (c, _s) = pair();
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let c = c.clone();
            tokio::spawn(async move { c.recv_datagram().await })
        })
        .collect();
    sleep(50 * MS).await;
    c.close();
    for w in waiters {
        let r = timeout(Duration::from_secs(2), w)
            .await
            .expect("a waiter was left hanging")
            .unwrap();
        assert!(matches!(r, Err(Error::Closed(_))), "got {r:?}");
    }
}

/// A pipe that takes writes but never finishes a flush, like one whose far end stopped reading.
struct StuckFlush(#[allow(dead_code)] Arc<()>);

impl AsyncWrite for StuckFlush {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn a_pipe_that_never_finishes_a_flush_does_not_outlive_close() {
    let token = Arc::new(());
    let (a, _b) = tokio::io::duplex(64);
    let (ar, _aw) = tokio::io::split(a);
    let client = mux_client(ar, StuckFlush(token.clone()));
    sleep(100 * MS).await; // the first ping went out; the writer is now stuck in `flush`
    assert_eq!(
        Arc::strong_count(&token),
        2,
        "the writer should still hold the pipe"
    );
    client.close();
    timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&token) > 1 {
            sleep(10 * MS).await;
        }
    })
    .await
    .expect("the writer task never let go of its pipe");
}

#[tokio::test(start_paused = true)]
async fn dropping_the_last_handle_gives_a_pipe_that_never_drains_one_second_and_no_more() {
    let token = Arc::new(());
    let (a, _b) = tokio::io::duplex(64);
    let (ar, _aw) = tokio::io::split(a);
    let client = mux_client(ar, StuckFlush(token.clone()));
    sleep(100 * MS).await; // the first ping went out; the writer is now stuck in `flush`
    let dropped = tokio::time::Instant::now();
    drop(client);
    timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&token) > 1 {
            sleep(10 * MS).await;
        }
    })
    .await
    .expect("the writer never let go of its pipe");
    // Not at once (that is what flushing first means) and not 1.5 s either (the silent peer timer).
    let took = dropped.elapsed();
    assert!(
        (Duration::from_millis(900)..Duration::from_millis(1400)).contains(&took),
        "closed after {took:?}"
    );
}

/// End to end on one thread: every keystroke is answered while 64 MiB of bulk keeps the pipe full.
/// (The wire-level proof that a keystroke waits behind at most one 16 KiB chunk is the unit test
/// `a_keystroke_waits_behind_at_most_one_chunk_but_only_with_priority`; round-trip times here also
/// include the single-threaded scheduler, so they are not asserted.)
#[tokio::test(flavor = "current_thread")]
async fn keystrokes_get_through_while_bulk_saturates_the_pipe() {
    const BULK_STREAMS: usize = 8;
    const PER_STREAM: usize = 8 << 20;
    let (c, s) = pair();
    let received = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    {
        let (received, finished) = (received.clone(), finished.clone());
        tokio::spawn(async move {
            while let Ok((kind, mut tx, mut rx)) = s.accept().await {
                let (received, finished) = (received.clone(), finished.clone());
                tokio::spawn(async move {
                    while let Ok(Some(b)) = rx.recv().await {
                        match kind {
                            StreamKind::Term(_) => {
                                let _ = tx.send(&TermDown::Bell).await;
                            }
                            _ => {
                                received.fetch_add(b.len(), Ordering::Relaxed);
                            }
                        }
                    }
                    if matches!(kind, StreamKind::Bulk(_)) {
                        finished.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
    }
    for i in 0..BULK_STREAMS {
        let (mut tx, rx) = c
            .open(StreamKind::Bulk(i as u64), prio::BULK)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _rx = rx;
            for part in 0..PER_STREAM / 16_000 {
                tx.send_bytes(&random(16_000, (i * 1000 + part) as u64))
                    .await
                    .unwrap();
            }
            tx.finish().await.unwrap();
        });
    }
    let (mut ktx, mut krx) = c
        .open_kind(StreamKind::Term(PaneId {
            sid: Id::new(),
            no: 0,
        }))
        .await
        .unwrap();
    while received.load(Ordering::Relaxed) < 128 << 10 {
        tokio::task::yield_now().await; // let the bulk get going
    }
    let before = received.load(Ordering::Relaxed);
    for seq in 0..100u32 {
        ktx.send(&TermUp::Paste {
            seq,
            text: "x".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            krx.recv_msg::<TermDown>().await.unwrap(),
            Some(TermDown::Bell)
        );
    }
    let during = received.load(Ordering::Relaxed) - before;
    assert!(
        finished.load(Ordering::Relaxed) < BULK_STREAMS,
        "the bulk finished before the keystrokes did: nothing was competing with them"
    );
    assert!(
        during > 0,
        "the bulk made no progress during the keystrokes"
    );
}

/// Relays its stdio to the unix socket given as argv[1] (what `femmed proxy` does) and writes its
/// pid to argv[2]. With argv[3] it first takes that many seconds, like an `ssh` that is logging in.
const RELAY: &str = r#"
import os, socket, sys, threading, time
open(sys.argv[2], "w").write(str(os.getpid()))
if len(sys.argv) > 3: time.sleep(float(sys.argv[3]))
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
def up():
    while True:
        d = os.read(0, 65536)
        if not d: break
        s.sendall(d)
    s.shutdown(socket.SHUT_WR)
threading.Thread(target=up, daemon=True).start()
while True:
    d = s.recv(65536)
    if not d: break
    os.write(1, d)
"#;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn command_connect_speaks_the_mux_over_a_child_and_kills_and_reaps_it_on_close() {
    if std::process::Command::new("python3")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("python3 not found: skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (sock, pidfile) = (dir.path().join("femmed.sock"), dir.path().join("pid"));
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = stream.into_split();
        mux_server(r, w)
    });
    let mut cmd = tokio::process::Command::new("python3");
    cmd.args(["-c", RELAY]).arg(&sock).arg(&pidfile);
    let conn = command_connect(cmd)
        .await
        .expect("connect through the child");
    assert!(
        !conn.rtt().is_zero(),
        "Ok must mean a pong already came back"
    );
    let server = server.await.unwrap();

    let (mut tx, mut rx) = conn.open_kind(StreamKind::Ctrl).await.unwrap();
    let (kind, mut stx, mut srx) = server.accept().await.unwrap();
    assert_eq!(kind, StreamKind::Ctrl);
    tx.send(&"ping").await.unwrap();
    assert_eq!(
        srx.recv_msg::<String>().await.unwrap().as_deref(),
        Some("ping")
    );
    stx.send(&"pong").await.unwrap();
    assert_eq!(
        rx.recv_msg::<String>().await.unwrap().as_deref(),
        Some("pong")
    );

    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let proc_dir = format!("/proc/{pid}");
    assert!(std::path::Path::new(&proc_dir).exists());
    conn.close();
    // Gone, not a zombie: `conn`, `tx` and `rx` are all still held, and nobody else will wait for it.
    timeout(Duration::from_secs(5), async {
        while std::path::Path::new(&proc_dir).exists() {
            sleep(20 * MS).await;
        }
    })
    .await
    .expect("the child was not killed and reaped when the connection closed");
    let _ = timeout(Duration::from_secs(2), server.closed())
        .await
        .expect("the server end never saw the pipe close");
}

/// The 1.5 s dead-peer rule starts with the first answer: before that the peer may be `ssh` logging in.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn command_connect_waits_out_a_login_that_takes_longer_than_the_dead_peer_window() {
    if std::process::Command::new("python3")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("python3 not found: skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (sock, pidfile) = (dir.path().join("femmed.sock"), dir.path().join("pid"));
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = stream.into_split();
        mux_server(r, w)
    });
    let mut cmd = tokio::process::Command::new("python3");
    cmd.args(["-c", RELAY]).arg(&sock).arg(&pidfile).arg("2.2");
    let started = std::time::Instant::now();
    let conn = command_connect(cmd)
        .await
        .expect("a 2.2 s login was taken for a dead peer");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the child did not take its time: {:?}",
        started.elapsed()
    );
    let server = server.await.unwrap();
    // And it is a working connection, not one that is about to be cut off.
    let (mut tx, _rx) = conn.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&"hello").await.unwrap();
    let (_, _stx, mut srx) = server.accept().await.unwrap();
    assert_eq!(
        srx.recv_msg::<String>().await.unwrap().as_deref(),
        Some("hello")
    );
}

#[tokio::test]
async fn command_connect_reports_a_child_that_dies_or_is_missing() {
    let err = command_connect(tokio::process::Command::new(
        "/nonexistent/femme-test-binary",
    ))
    .await
    .err()
    .expect("must fail");
    assert!(
        matches!(&err, Error::Connect(m) if m.contains("cannot run")),
        "got {err:?}"
    );

    // `true` exits at once: the pipe closes before any pong.
    let err = command_connect(tokio::process::Command::new("true"))
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(&err, Error::Connect(m) if m.contains("true")),
        "got {err:?}"
    );
}
