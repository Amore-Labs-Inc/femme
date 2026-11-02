//! QUIC transport on loopback: identities, mutual TLS, streams, datagrams, migration.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use femme_proto::codec::Framer;
use femme_proto::rpc::C2S;
use femme_proto::{DeviceId, Id};
use femme_transport::quic::{Allow, KEEPALIVE_FOREGROUND};
use femme_transport::{
    listen, Conn, DeviceIdentity, Error, Listener, QuicClient, ServerIdentity, StreamKind,
};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ClientSessionStore, ResolvesClientCert};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::time::timeout;

use common::random;

const WAIT: Duration = Duration::from_secs(10);

struct Env {
    server: ServerIdentity,
    device: DeviceIdentity,
    device_id: DeviceId,
    listener: Listener,
    addr: SocketAddr,
    /// How often the daemon's allow list was asked.
    allow_calls: Arc<AtomicUsize>,
}

async fn env() -> Env {
    let server = ServerIdentity::generate(&["localhost".into(), "127.0.0.1".into()]);
    let device = DeviceIdentity::generate("alice's laptop");
    let spki = device.spki_sha256().unwrap();
    let device_id = Id::new();
    let allow_calls = Arc::new(AtomicUsize::new(0));
    let calls = allow_calls.clone();
    let allow: Allow = Arc::new(move |h| {
        calls.fetch_add(1, Ordering::SeqCst);
        (*h == spki).then_some(device_id)
    });
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    Env {
        server,
        device,
        device_id,
        listener,
        addr,
        allow_calls,
    }
}

async fn connected(e: &Env) -> (Arc<QuicClient>, Arc<dyn Conn>) {
    let client = QuicClient::connect(
        e.addr,
        e.server.fingerprint(),
        &e.device,
        Some(KEEPALIVE_FOREGROUND),
    )
    .await
    .unwrap();
    let (id, server_side) = timeout(WAIT, e.listener.accept())
        .await
        .expect("the server never accepted")
        .unwrap();
    assert_eq!(id, e.device_id, "the listener names the enrolled device");
    (client, server_side)
}

/// What a refused client sees: the handshake error either from `connect` or, because TLS 1.3 lets
/// the client finish first, from the closed connection straight after.
async fn assert_refused(result: Result<Arc<QuicClient>, Error>) {
    match result {
        Err(e) => assert!(matches!(e, Error::Handshake(_)), "got {e:?}"),
        Ok(conn) => {
            let e = timeout(WAIT, conn.closed())
                .await
                .expect("a refused client stayed connected");
            assert!(matches!(e, Error::Handshake(_)), "got {e:?}");
            assert!(conn.open_kind(StreamKind::Ctrl).await.is_err());
        }
    }
}

#[tokio::test]
async fn the_conn_contract_holds_over_quic() {
    let e = env().await;
    let (client, server_side) = connected(&e).await;
    assert!(client.is_quic() && server_side.is_quic());
    common::conformance(client, server_side).await;
}

#[tokio::test]
async fn five_megabytes_of_bulk_each_way_alongside_a_term_stream() {
    const TOTAL: usize = 5 << 20;
    const PART: usize = 64 << 10;
    let e = env().await;
    let (client, server) = connected(&e).await;
    let (mut ctx, mut crx) = client.open_kind(StreamKind::Bulk(1)).await.unwrap();
    let up = tokio::spawn(async move {
        for i in 0..TOTAL / PART {
            ctx.send_bytes(&random(PART, i as u64 + 10)).await.unwrap();
        }
        ctx.finish().await.unwrap();
        ctx
    });
    let (kind, mut stx, mut srx) = server.accept().await.unwrap();
    assert_eq!(kind, StreamKind::Bulk(1));
    let down = tokio::spawn(async move {
        for i in 0..TOTAL / PART {
            stx.send_bytes(&random(PART, i as u64 + 500)).await.unwrap();
        }
        stx.finish().await.unwrap();
        stx
    });
    let server_reader = tokio::spawn(async move {
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
    // A keystroke stream stays responsive meanwhile.
    let (mut ktx, mut krx) = client.open_kind(StreamKind::Ctrl).await.unwrap();
    let echo = tokio::spawn({
        let server = server.clone();
        async move {
            let (kind, mut tx, mut rx) = server.accept().await.unwrap();
            assert_eq!(kind, StreamKind::Ctrl);
            while let Some(m) = rx.recv_msg::<u32>().await.unwrap() {
                tx.send(&m).await.unwrap();
            }
        }
    });
    for i in 0..20u32 {
        ktx.send(&i).await.unwrap();
        assert_eq!(krx.recv_msg::<u32>().await.unwrap(), Some(i));
    }
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
    server_reader.await.unwrap();
    let _ = (up.await.unwrap(), down.await.unwrap());
    ktx.finish().await.unwrap();
    echo.await.unwrap();
}

#[tokio::test]
async fn rebind_mid_session_keeps_the_connection_and_its_streams() {
    let e = env().await;
    let (client, server) = connected(&e).await;
    let (mut tx, mut rx) = client.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&1u32).await.unwrap();
    let (_, mut stx, mut srx) = server.accept().await.unwrap();
    assert_eq!(srx.recv_msg::<u32>().await.unwrap(), Some(1));

    let before = client.local_addr().unwrap();
    client.rebind().unwrap();
    let after = client.local_addr().unwrap();
    assert_ne!(
        before.port(),
        after.port(),
        "rebind must switch to a new socket"
    );

    // The stream opened before keeps working, in both directions...
    tx.send(&2u32).await.unwrap();
    assert_eq!(
        timeout(WAIT, srx.recv_msg::<u32>())
            .await
            .expect("data lost after rebind")
            .unwrap(),
        Some(2)
    );
    stx.send(&3u32).await.unwrap();
    assert_eq!(
        timeout(WAIT, rx.recv_msg::<u32>())
            .await
            .expect("data lost after rebind")
            .unwrap(),
        Some(3)
    );
    // ...and so does a new one.
    let (mut tx2, _rx2) = client.open_kind(StreamKind::Events).await.unwrap();
    tx2.send(&4u32).await.unwrap();
    let (kind, _tx, mut rx3) = timeout(WAIT, server.accept())
        .await
        .expect("no new stream after rebind")
        .unwrap();
    assert_eq!(kind, StreamKind::Events);
    assert_eq!(rx3.recv_msg::<u32>().await.unwrap(), Some(4));
    // Datagrams too.
    client
        .send_datagram(Bytes::from_static(b"still here"))
        .unwrap();
    assert_eq!(
        timeout(WAIT, server.recv_datagram())
            .await
            .expect("datagram lost after rebind")
            .unwrap(),
        Bytes::from_static(b"still here")
    );
}

#[tokio::test]
async fn rebind_through_the_conn_trait_mid_transfer_keeps_the_stream_intact() {
    const PART: usize = 64 << 10;
    const PARTS: usize = 48;
    let e = env().await;
    let (client, server) = connected(&e).await;
    let concrete = client.clone(); // only for `local_addr`: the call under test goes through `dyn Conn`
    let client: Arc<dyn Conn> = client;
    let (mut tx, _rx) = client.open_kind(StreamKind::Bulk(1)).await.unwrap();
    let (_, _stx, mut srx) = server.accept().await.unwrap();
    let upload = tokio::spawn(async move {
        for i in 0..PARTS {
            tx.send_bytes(&random(PART, i as u64 + 10)).await.unwrap();
        }
        tx.finish().await.unwrap();
        tx
    });
    let before = concrete.local_addr().unwrap();
    let mut n = 0;
    while let Some(b) = timeout(WAIT, srx.recv())
        .await
        .expect("the transfer stalled")
        .unwrap()
    {
        assert_eq!(b, Bytes::from(random(PART, n as u64 + 10)), "part {n}");
        n += 1;
        if n == 8 {
            client.rebind().expect("rebind through the trait");
            assert_ne!(
                concrete.local_addr().unwrap().port(),
                before.port(),
                "still on the old socket"
            );
        }
    }
    assert_eq!(n, PARTS);
    let _ = upload.await.unwrap();
}

#[tokio::test]
async fn a_device_that_is_not_enrolled_is_refused_in_the_handshake() {
    let e = env().await;
    let stranger = DeviceIdentity::generate("mallory");
    assert_refused(QuicClient::connect(e.addr, e.server.fingerprint(), &stranger, None).await)
        .await;
    assert!(
        timeout(Duration::from_millis(300), e.listener.accept())
            .await
            .is_err(),
        "a refused device reached accept"
    );
    // The enrolled one is still welcome.
    connected(&e).await;
}

#[tokio::test]
async fn a_client_pinning_the_wrong_server_fingerprint_fails_the_handshake() {
    let e = env().await;
    let mut wrong = e.server.fingerprint();
    wrong[0] ^= 1;
    let err = QuicClient::connect(e.addr, wrong, &e.device, None)
        .await
        .err()
        .expect("must not connect");
    assert!(matches!(err, Error::Handshake(_)), "got {err:?}");
    assert!(timeout(Duration::from_millis(300), e.listener.accept())
        .await
        .is_err());

    // Another daemon's identity is no better than a wrong number.
    let other = ServerIdentity::generate(&[]);
    let err = QuicClient::connect(e.addr, other.fingerprint(), &e.device, None)
        .await
        .err()
        .expect("must not connect");
    assert!(matches!(err, Error::Handshake(_)), "got {err:?}");
}

#[tokio::test]
async fn revoking_a_device_refuses_its_next_connection_but_leaves_the_open_one() {
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("alice");
    let spki = device.spki_sha256().unwrap();
    let revoked = Arc::new(AtomicBool::new(false));
    let (id, flag) = (Id::new(), revoked.clone());
    let allow: Allow =
        Arc::new(move |h| (*h == spki && !flag.load(Ordering::SeqCst)).then_some(id));
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    let first = QuicClient::connect(addr, server.fingerprint(), &device, None)
        .await
        .unwrap();
    let (_, first_server) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    revoked.store(true, Ordering::SeqCst);

    assert_refused(QuicClient::connect(addr, server.fingerprint(), &device, None).await).await;
    assert!(timeout(Duration::from_millis(300), listener.accept())
        .await
        .is_err());
    // ponytail: the daemon, not the transport, closes connections of revoked devices.
    let (mut tx, _rx) = first.open_kind(StreamKind::Ctrl).await.unwrap();
    tx.send(&C2S::Hello {
        proto: 1,
        client_instance: 1,
        cursors: Default::default(),
    })
    .await
    .unwrap();
    assert_eq!(
        first_server.accept().await.unwrap().0,
        StreamKind::Ctrl,
        "the connection that was open before keeps working"
    );
}

#[tokio::test]
async fn devices_are_told_apart_by_their_public_key() {
    let server = ServerIdentity::generate(&[]);
    let (alice, bob) = (
        DeviceIdentity::generate("alice"),
        DeviceIdentity::generate("alice"),
    );
    let (spki_a, spki_b) = (alice.spki_sha256().unwrap(), bob.spki_sha256().unwrap());
    let (id_a, id_b) = (Id::new(), Id::new());
    let allow: Allow = Arc::new(move |h| {
        if *h == spki_a {
            Some(id_a)
        } else if *h == spki_b {
            Some(id_b)
        } else {
            None
        }
    });
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let _a = QuicClient::connect(addr, server.fingerprint(), &alice, None)
        .await
        .unwrap();
    assert_eq!(
        timeout(WAIT, listener.accept()).await.unwrap().unwrap().0,
        id_a
    );
    let _b = QuicClient::connect(addr, server.fingerprint(), &bob, None)
        .await
        .unwrap();
    assert_eq!(
        timeout(WAIT, listener.accept()).await.unwrap().unwrap().0,
        id_b,
        "same name, different key, different device"
    );
}

// ---------------------------------------------------------------------------------------------
// raw quinn clients, to attack the server in ways the real client cannot

#[derive(Debug)]
struct TrustAnyServer(Arc<CryptoProvider>);

impl ServerCertVerifier for TrustAnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Presents one device's certificate while signing with whatever key it was given.
#[derive(Debug)]
struct Identity(Arc<CertifiedKey>);

impl ResolvesClientCert for Identity {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

fn signing_key(dev: &DeviceIdentity) -> Arc<dyn SigningKey> {
    rustls::crypto::aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(dev.key_der.clone()),
    ))
    .unwrap()
}

/// The TLS side of a bare quinn client: it trusts any server and, with `identity`, shows `.0`'s
/// certificate while signing with `.1`'s key (without it, shows none).
fn raw_tls(identity: Option<(&DeviceIdentity, &DeviceIdentity)>) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAnyServer(provider)));
    let mut tls = match identity {
        Some((cert_of, key_of)) => {
            let key = CertifiedKey::new(
                vec![CertificateDer::from(cert_of.cert_der.clone())],
                signing_key(key_of),
            );
            builder.with_client_cert_resolver(Arc::new(Identity(Arc::new(key))))
        }
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![femme_proto::ALPN.to_vec()];
    tls
}

fn raw_endpoint_and_config(tls: rustls::ClientConfig) -> (quinn::Endpoint, quinn::ClientConfig) {
    let cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
    (
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap(),
        cfg,
    )
}

async fn raw_connect_with(
    addr: SocketAddr,
    tls: rustls::ClientConfig,
) -> Result<(quinn::Endpoint, quinn::Connection), quinn::ConnectionError> {
    let (ep, cfg) = raw_endpoint_and_config(tls);
    let conn = ep.connect_with(cfg, addr, "femmed").unwrap().await?;
    Ok((ep, conn))
}

/// A bare quinn connection that shows `cert_of`'s certificate and signs with `key_of`'s key.
async fn raw_connect(
    addr: SocketAddr,
    cert_of: &DeviceIdentity,
    key_of: &DeviceIdentity,
) -> Result<(quinn::Endpoint, quinn::Connection), quinn::ConnectionError> {
    raw_connect_with(addr, raw_tls(Some((cert_of, key_of)))).await
}

#[tokio::test]
async fn a_stolen_certificate_without_its_private_key_is_refused() {
    let e = env().await;
    let mallory = DeviceIdentity::generate("mallory");

    // Control: the same raw client with the real key is accepted, so the harness works.
    let (_ep, conn) = raw_connect(e.addr, &e.device, &e.device)
        .await
        .expect("the genuine device must connect");
    let (id, _server_side) = timeout(WAIT, e.listener.accept())
        .await
        .expect("genuine raw client never accepted")
        .unwrap();
    assert_eq!(id, e.device_id);
    drop(conn);

    // Alice's certificate (which the server trusts) signed with mallory's key: the TLS signature
    // check must catch it, even though the certificate itself is on the list.
    match raw_connect(e.addr, &e.device, &mallory).await {
        Err(err) => assert!(
            format!("{err:?}").contains("BadSignature"),
            "refused for the wrong reason: {err:?}"
        ),
        Ok((_ep, conn)) => {
            let err = timeout(WAIT, conn.closed())
                .await
                .expect("the forged client stayed connected");
            assert!(
                format!("{err:?}").contains("BadSignature"),
                "refused for the wrong reason: {err:?}"
            );
        }
    }
    assert!(
        timeout(Duration::from_millis(500), e.listener.accept())
            .await
            .is_err(),
        "a forged certificate reached accept"
    );
}

#[tokio::test]
async fn a_stream_with_a_garbage_kind_is_reset_and_other_streams_still_work() {
    let e = env().await;
    let (_ep, conn) = raw_connect(e.addr, &e.device, &e.device).await.unwrap();
    let (_, server) = timeout(WAIT, e.listener.accept()).await.unwrap().unwrap();

    // 0x63 is no StreamKind variant, so the server cannot route it.
    let (mut bad_tx, mut bad_rx) = conn.open_bi().await.unwrap();
    let mut garbage = Vec::new();
    Framer::new(false).frame(&99u8, &mut garbage).unwrap();
    bad_tx.write_all(&garbage).await.unwrap();

    let (mut good_tx, _good_rx) = conn.open_bi().await.unwrap();
    let mut kind = Vec::new();
    Framer::new(true)
        .frame(&StreamKind::Ctrl, &mut kind)
        .unwrap();
    good_tx.write_all(&kind).await.unwrap();

    let (accepted, _tx, _rx) = timeout(WAIT, server.accept())
        .await
        .expect("the good stream was held up")
        .unwrap();
    assert_eq!(
        accepted,
        StreamKind::Ctrl,
        "only the routable stream is accepted"
    );
    let mut byte = [0u8; 1];
    let end = timeout(WAIT, bad_rx.read(&mut byte))
        .await
        .expect("the garbage stream was not reset");
    assert!(
        matches!(end, Err(quinn::ReadError::Reset(_))),
        "got {end:?}"
    );
}

#[tokio::test]
async fn at_most_128_streams_can_be_open_and_the_129th_waits() {
    let e = env().await;
    let (client, _server) = connected(&e).await;
    let mut open = Vec::new();
    for i in 0..128 {
        open.push(client.open_kind(StreamKind::Bulk(i)).await.unwrap());
    }
    let waiting = tokio::spawn({
        let client = client.clone();
        async move { client.open_kind(StreamKind::Bulk(999)).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "a 129th stream was opened");
}

#[tokio::test]
async fn closing_the_listener_ends_accept_and_the_connections() {
    let e = env().await;
    let (client, server) = connected(&e).await;
    e.listener.close();
    assert!(matches!(
        timeout(WAIT, client.closed())
            .await
            .expect("the client never noticed"),
        Error::Closed(_)
    ));
    assert!(matches!(
        timeout(WAIT, server.closed())
            .await
            .expect("the server end never noticed"),
        Error::Closed(_)
    ));
    assert!(timeout(WAIT, e.listener.accept())
        .await
        .expect("accept stalled")
        .is_err());
}

#[tokio::test]
async fn connecting_to_a_silent_port_times_out_with_a_connect_error() {
    let device = DeviceIdentity::generate("alice");
    let started = std::time::Instant::now();
    // A UDP port nobody answers on: the handshake times out (5 s) with a Connect error.
    let err = QuicClient::connect("127.0.0.1:9".parse().unwrap(), [0u8; 32], &device, None)
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(&err, Error::Connect(m) if m.contains("127.0.0.1:9")),
        "got {err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[tokio::test]
async fn a_client_without_a_certificate_is_refused_with_error_116_and_allow_is_never_asked() {
    let e = env().await;
    let why = match raw_connect_with(e.addr, raw_tls(None)).await {
        Err(err) => err.to_string(),
        Ok((_ep, conn)) => timeout(WAIT, conn.closed())
            .await
            .expect("a client with no certificate stayed connected")
            .to_string(),
    };
    assert!(why.contains("116"), "refused for the wrong reason: {why}");
    assert!(
        timeout(Duration::from_millis(500), e.listener.accept())
            .await
            .is_err(),
        "a client with no certificate reached accept"
    );
    assert_eq!(
        e.allow_calls.load(Ordering::SeqCst),
        0,
        "the allow list must not even be asked"
    );
}

/// Counts the TLS 1.3 session tickets the server hands over; everything else is the stock cache.
#[derive(Debug)]
struct TicketSpy {
    tickets: AtomicUsize,
    inner: rustls::client::ClientSessionMemoryCache,
}

impl ClientSessionStore for TicketSpy {
    fn set_kx_hint(&self, n: ServerName<'static>, g: rustls::NamedGroup) {
        self.inner.set_kx_hint(n, g)
    }
    fn kx_hint(&self, n: &ServerName<'_>) -> Option<rustls::NamedGroup> {
        self.inner.kx_hint(n)
    }
    fn set_tls12_session(
        &self,
        n: ServerName<'static>,
        v: rustls::client::Tls12ClientSessionValue,
    ) {
        self.inner.set_tls12_session(n, v)
    }
    fn tls12_session(&self, n: &ServerName<'_>) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.inner.tls12_session(n)
    }
    fn remove_tls12_session(&self, n: &ServerName<'static>) {
        self.inner.remove_tls12_session(n)
    }
    fn insert_tls13_ticket(
        &self,
        n: ServerName<'static>,
        v: rustls::client::Tls13ClientSessionValue,
    ) {
        self.tickets.fetch_add(1, Ordering::SeqCst);
        self.inner.insert_tls13_ticket(n, v)
    }
    fn take_tls13_ticket(
        &self,
        n: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.inner.take_tls13_ticket(n)
    }
}

#[tokio::test]
async fn no_session_ticket_is_issued_so_every_connection_is_a_full_handshake_without_0rtt() {
    let e = env().await;
    let spy = Arc::new(TicketSpy {
        tickets: AtomicUsize::new(0),
        inner: rustls::client::ClientSessionMemoryCache::new(16),
    });
    for round in 0..3 {
        let mut tls = raw_tls(Some((&e.device, &e.device)));
        tls.resumption = rustls::client::Resumption::store(spy.clone());
        tls.enable_early_data = true; // use 0-RTT whenever a ticket allows it
        let (ep, cfg) = raw_endpoint_and_config(tls);
        let conn = match ep.connect_with(cfg, e.addr, "femmed").unwrap().into_0rtt() {
            Ok(_) => panic!("round {round}: the client was allowed to send 0-RTT data"),
            Err(connecting) => connecting.await.unwrap(),
        };
        timeout(WAIT, e.listener.accept())
            .await
            .expect("never accepted")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await; // a NewSessionTicket would arrive after the handshake
        conn.close(0u32.into(), b"");
    }
    assert_eq!(
        spy.tickets.load(Ordering::SeqCst),
        0,
        "the server issued session tickets"
    );
    assert_eq!(
        e.allow_calls.load(Ordering::SeqCst),
        6,
        "the allow list is asked twice per connection, every time"
    );
}

#[tokio::test]
async fn a_device_revoked_while_its_handshake_runs_is_told_so_not_just_closed() {
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("alice");
    let spki = device.spki_sha256().unwrap();
    // The TLS verifier asks first and says yes; the daemon asks again once the handshake is done,
    // and by then the device has been revoked.
    let asked = Arc::new(AtomicUsize::new(0));
    let (id, n) = (Id::new(), asked.clone());
    let allow: Allow =
        Arc::new(move |h| (*h == spki && n.fetch_add(1, Ordering::SeqCst) == 0).then_some(id));
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let client = QuicClient::connect(
        listener.local_addr().unwrap(),
        server.fingerprint(),
        &device,
        None,
    )
    .await
    .unwrap();
    let e = timeout(WAIT, client.closed())
        .await
        .expect("the revoked device stayed connected");
    assert!(matches!(e, Error::Handshake(_)), "got {e:?}");
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "a revoked device reached accept"
    );
}

/// Two runtimes, so the server can be frozen mid-session: its socket stays open but nothing reads
/// it, like a laptop that went to sleep behind a NAT (no ICMP, no reset, just silence).
#[test]
fn a_server_that_vanishes_is_declared_dead_within_the_dead_window() {
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("alice");
    let spki = device.spki_sha256().unwrap();
    let allow: Allow = Arc::new(move |h| (*h == spki).then(Id::new));
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    };
    let (server_rt, client_rt) = (runtime(), runtime());
    let (listener, addr) = server_rt.block_on(async {
        let l = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
            .await
            .unwrap();
        let a = l.local_addr().unwrap();
        (l, a)
    });
    let client = client_rt
        .block_on(QuicClient::connect(
            addr,
            server.fingerprint(),
            &device,
            Some(KEEPALIVE_FOREGROUND),
        ))
        .unwrap();
    let server_side = server_rt.block_on(async { listener.accept().await.unwrap().1 });
    std::mem::forget((server_side, listener)); // keep the socket open...
    server_rt.shutdown_background(); // ...with nobody answering on it
    client_rt.block_on(async {
        let started = Instant::now();
        let e = timeout(Duration::from_secs(10), client.closed())
            .await
            .expect("a vanished server was never declared dead");
        assert!(matches!(e, Error::Closed(_)), "got {e:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn a_key_that_does_not_match_its_certificate_is_refused_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (ServerIdentity::generate(&[]), ServerIdentity::generate(&[]));
    ServerIdentity {
        cert_der: a.cert_der.clone(),
        key_der: b.key_der.clone(),
    }
    .save(dir.path())
    .unwrap();
    assert!(
        ServerIdentity::load(dir.path()).is_err(),
        "another identity's key was accepted"
    );
    a.save(dir.path()).unwrap();
    assert_eq!(
        ServerIdentity::load(dir.path()).unwrap().fingerprint(),
        a.fingerprint()
    );
}

#[cfg(unix)]
#[test]
fn a_key_file_that_others_can_read_is_refused_on_load() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    DeviceIdentity::generate("alice").save(dir.path()).unwrap();
    DeviceIdentity::load(dir.path()).unwrap(); // saved 0600: fine
    std::fs::set_permissions(
        dir.path().join("device.key"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let err = DeviceIdentity::load(dir.path()).expect_err("a world-readable key must be refused");
    assert!(err.to_string().contains("readable"), "{err}");
    // Not papered over by making a new identity either.
    assert!(DeviceIdentity::load_or_generate(dir.path(), "alice").is_err());
}

#[test]
fn saving_an_identity_writes_the_key_before_the_certificate() {
    // The certificate is what clients pin and `femmed enroll` prints, so it goes down last: make its
    // path a directory so that writing it fails, and the key must already be in place by then.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("server.crt")).unwrap();
    assert!(ServerIdentity::generate(&[]).save(dir.path()).is_err());
    assert!(
        dir.path().join("server.key").exists(),
        "the key was not written first"
    );
}
