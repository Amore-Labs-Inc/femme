//! QUIC transport: quinn on rustls/aws-lc-rs, TLS 1.3 only, ALPN `femme/1`, mutual TLS pinned by
//! fingerprints instead of a CA (plan §6).
//!
//! - The daemon has a self-signed [`ServerIdentity`]. Clients pin the SHA-256 of its certificate
//!   (what `femmed enroll` prints); no CA, no host name check.
//! - Every device has a self-signed [`DeviceIdentity`]. The daemon accepts a client certificate iff
//!   the `allow` callback knows the SHA-256 of its public key (SPKI) and the TLS 1.3 signature
//!   proves possession of that key (`rustls::crypto::verify_tls13_signature`, never stubbed).
//! - Resumption tickets are off, so every connection is a full handshake and a revoked device is
//!   refused at once. (Connections already open stay open: the daemon closes them itself.)
//! - A client that hears nothing for its keepalive interval plus three round trips (at least 1.5 s,
//!   plan rule 7) closes the connection; the daemon end sends no keepalive and relies on the idle
//!   timeout.
//! - There is no Retry: the daemon does not validate client addresses (see [`listen`]).
//!
//! Timeouts and sizes are constants here on purpose: keepalive 1 s foreground (client only; see
//! [`KEEPALIVE_BACKGROUND`]), idle timeout 30 s, initial RTT 100 ms, 128 concurrent streams, a
//! 64 KiB datagram buffer.

use std::net::SocketAddr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use femme_proto::{DeviceId, ALPN};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{
    prio, read_kind, Conn, Error, Rx, RxIo, StreamKind, Tx, TxIo, KIND_TIMEOUT, MAX_DATAGRAM,
};

/// QUIC keepalive while the window is focused: the desktop's default.
pub const KEEPALIVE_FOREGROUND: Duration = Duration::from_secs(1);
/// QUIC keepalive while in the background. Quinn fixes it when the connection is created, so the
/// desktop picks one at connect time (see [`QuicConn::connect`]).
pub const KEEPALIVE_BACKGROUND: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A peer is dead when nothing has come from it for three round trips beyond a keepalive interval,
/// and never in less than this (plan rule 7).
const DEAD_MIN: Duration = Duration::from_millis(1500);
/// The application close code the daemon uses on a connection whose device turned out not to be
/// enrolled after all (see `accept_loop`): the client reads it as a refused handshake.
const NOT_ENROLLED: quinn::VarInt = quinn::VarInt::from_u32(1);
/// Sent as SNI and ignored: the server certificate is pinned by hash, not by name.
const SERVER_NAME: &str = "femmed";

// ---------------------------------------------------------------------------------------------
// hashes and certificate parsing

/// SHA-256 through the aws-lc-rs hash that rustls already carries (no extra dependency).
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let suite = rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let hash = suite.tls13().expect("a TLS 1.3 suite").common.hash_provider;
    hash.hash(data)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// SHA-256 of the DER `SubjectPublicKeyInfo` of the certificate: the stable identity of a device.
/// `None` if the certificate does not parse.
pub fn spki_sha256_of_cert(cert_der: &[u8]) -> Option<[u8; 32]> {
    let der = CertificateDer::from(cert_der);
    let cert = webpki::EndEntityCert::try_from(&der).ok()?;
    Some(sha256(cert.subject_public_key_info().as_ref()))
}

// ---------------------------------------------------------------------------------------------
// identities

/// Called with the SHA-256 of a client's public key (SPKI) during the handshake: `Some(device)` if
/// that key is enrolled. Runs on the connection's I/O task, twice per connection (in the TLS
/// verifier and once more after the handshake), so keep it quick and non-blocking: look in a set
/// held in memory, not in the database.
pub type Allow = Arc<dyn Fn(&[u8; 32]) -> Option<DeviceId> + Send + Sync>;

fn cfg_err(e: impl std::fmt::Display) -> Error {
    Error::Io(Arc::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        e.to_string(),
    )))
}

fn generate_pair(sans: &[String], common_name: &str) -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("generating an ECDSA P-256 key");
    // Names only decorate the certificate (clients pin the hash): drop ones rcgen refuses.
    let mut params = rcgen::CertificateParams::new(sans.to_vec())
        .or_else(|_| rcgen::CertificateParams::new(Vec::<String>::new()))
        .expect("certificate params without names");
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, common_name);
    params.distinguished_name = dn;
    let cert = params
        .self_signed(&key)
        .expect("self-signing a certificate");
    (cert.der().to_vec(), key.serialize_der())
}

fn save_pair(dir: &Path, stem: &str, cert_der: &[u8], key_der: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    // The key goes first: the certificate is what clients pin and `femmed enroll` prints, so a crash
    // between the two must not leave a certificate that has no key behind it.
    // The key is created 0600 under a temporary name and renamed into place, so it is never
    // readable by others, not even for a moment.
    let tmp = dir.join(format!("{stem}.key.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(pem::encode(&pem::Pem::new("PRIVATE KEY", key_der)).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(format!("{stem}.key")))?;
    let crt = pem::encode(&pem::Pem::new("CERTIFICATE", cert_der));
    std::fs::write(dir.join(format!("{stem}.crt")), crt)?;
    Ok(())
}

fn read_pem(path: &Path, tag: &str) -> Result<Vec<u8>, Error> {
    let text = std::fs::read(path)?;
    let p = pem::parse(&text).map_err(|e| cfg_err(format!("{}: {e}", path.display())))?;
    if p.tag() != tag {
        return Err(cfg_err(format!(
            "{}: expected a {tag} block, found {}",
            path.display(),
            p.tag()
        )));
    }
    Ok(p.into_contents())
}

fn load_pair(dir: &Path, stem: &str) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let cert = read_pem(&dir.join(format!("{stem}.crt")), "CERTIFICATE")?;
    let key_path = dir.join(format!("{stem}.key"));
    // ponytail: unix permission bits only; Windows ACLs are not looked at.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path)?.permissions().mode();
        if mode & 0o077 != 0 {
            let path = key_path.display();
            return Err(cfg_err(format!(
                "{path} is readable by other users (mode {:o}); run `chmod 600 {path}`",
                mode & 0o777
            )));
        }
    }
    let key = read_pem(&key_path, "PRIVATE KEY")?;
    if spki_sha256_of_cert(&cert).is_none() {
        return Err(cfg_err(format!(
            "{stem}.crt in {} is not a usable certificate",
            dir.display()
        )));
    }
    // A key from another identity would fail every handshake, far from the file that is wrong.
    let (chain, der) = (
        vec![CertificateDer::from(cert.clone())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.clone())),
    );
    CertifiedKey::from_der(chain, der, &provider()).map_err(|e| {
        cfg_err(format!(
            "{stem}.key in {} does not belong to {stem}.crt: {e}",
            dir.display()
        ))
    })?;
    Ok((cert, key))
}

fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

/// The daemon's TLS identity: a self-signed ECDSA P-256 certificate and its PKCS#8 key.
#[derive(Clone)]
pub struct ServerIdentity {
    /// DER certificate.
    pub cert_der: Vec<u8>,
    /// DER PKCS#8 private key.
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &crate::fp_to_hex(&self.fingerprint()))
            .finish_non_exhaustive()
    }
}

impl ServerIdentity {
    /// Make a fresh identity. `names` (host names or IPs) only decorate the certificate, since
    /// clients pin its fingerprint; ones that are not valid are ignored.
    pub fn generate(names: &[String]) -> Self {
        let (cert_der, key_der) = generate_pair(names, "femmed");
        ServerIdentity { cert_der, key_der }
    }

    /// SHA-256 of the certificate DER: what clients pin and `femmed enroll` prints.
    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.cert_der)
    }

    /// Read `server.crt` and `server.key` (PEM) from `dir`. Refuses a key that does not belong to
    /// the certificate, and (on unix) one that other users can read.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let (cert_der, key_der) = load_pair(dir, "server")?;
        Ok(ServerIdentity { cert_der, key_der })
    }

    /// Write `server.crt` (PEM) and `server.key` (PEM, mode 0600) into `dir`, creating it.
    /// Overwrites: replacing a daemon's identity invalidates every client's pin.
    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        save_pair(dir, "server", &self.cert_der, &self.key_der)
    }

    /// [`load`](ServerIdentity::load), or make and [`save`](ServerIdentity::save) a new identity when
    /// `dir` has none yet (first start of the daemon).
    pub fn load_or_generate(dir: &Path, names: &[String]) -> Result<Self, Error> {
        match Self::load(dir) {
            Err(e) if is_not_found(&e) => {
                let id = Self::generate(names);
                id.save(dir)?;
                Ok(id)
            }
            other => other,
        }
    }
}

/// A device's TLS identity: a self-signed ECDSA P-256 certificate and its PKCS#8 key. The SHA-256
/// of its public key ([`spki_sha256`](DeviceIdentity::spki_sha256)) is what `femmed enroll` records.
#[derive(Clone)]
pub struct DeviceIdentity {
    /// DER certificate.
    pub cert_der: Vec<u8>,
    /// DER PKCS#8 private key.
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field(
                "spki_sha256",
                &self.spki_sha256().map(|h| crate::fp_to_hex(&h)),
            )
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Make a fresh identity named `name` (the certificate's common name; informational only).
    pub fn generate(name: &str) -> Self {
        let (cert_der, key_der) = generate_pair(&[], name);
        DeviceIdentity { cert_der, key_der }
    }

    /// SHA-256 over the DER `SubjectPublicKeyInfo` of the certificate, the same value
    /// `openssl x509 -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum` prints. `None`
    /// only if the certificate is unparseable (a loaded identity is checked, so effectively never).
    pub fn spki_sha256(&self) -> Option<[u8; 32]> {
        spki_sha256_of_cert(&self.cert_der)
    }

    /// Read `device.crt` and `device.key` (PEM) from `dir`. Refuses a key that does not belong to
    /// the certificate, and (on unix) one that other users can read.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let (cert_der, key_der) = load_pair(dir, "device")?;
        Ok(DeviceIdentity { cert_der, key_der })
    }

    /// Write `device.crt` (PEM) and `device.key` (PEM, mode 0600) into `dir`, creating it.
    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        save_pair(dir, "device", &self.cert_der, &self.key_der)
    }

    /// [`load`](DeviceIdentity::load), or make and [`save`](DeviceIdentity::save) an identity named
    /// `name` when `dir` has none yet (first run of the desktop).
    pub fn load_or_generate(dir: &Path, name: &str) -> Result<Self, Error> {
        match Self::load(dir) {
            Err(e) if is_not_found(&e) => {
                let id = Self::generate(name);
                id.save(dir)?;
                Ok(id)
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TLS verifiers

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// What the client says when the server's certificate is not the pinned one.
#[derive(Debug)]
struct PinMismatch {
    got: [u8; 32],
}

impl std::fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "server certificate {} is not the pinned one",
            crate::fp_to_hex(&self.got)
        )
    }
}

impl std::error::Error for PinMismatch {}

/// Client side: accept exactly the server certificate whose SHA-256 is `fp`. No CA, no name check.
#[derive(Debug)]
struct PinnedServer {
    fp: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = sha256(end_entity.as_ref());
        if got == self.fp {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(
                rustls::OtherError(Arc::new(PinMismatch { got })),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Server side: require a client certificate and accept it iff `allow` knows its SPKI hash.
struct EnrolledDevices {
    allow: Allow,
    provider: Arc<CryptoProvider>,
}

impl std::fmt::Debug for EnrolledDevices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnrolledDevices")
    }
}

impl ClientCertVerifier for EnrolledDevices {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA to name: an empty list makes the client present whatever certificate it has.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let spki = spki_sha256_of_cert(end_entity.as_ref()).ok_or(
            rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
        )?;
        if (self.allow)(&spki).is_some() {
            Ok(ClientCertVerified::assertion())
        } else {
            tracing::warn!(spki = %crate::fp_to_hex(&spki), "QUIC client refused: device key not enrolled");
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------------------------
// quinn configuration and error mapping

fn transport(keepalive: Option<Duration>) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(keepalive)
        .max_idle_timeout(Some(
            IDLE_TIMEOUT.try_into().expect("30 s fits an idle timeout"),
        ))
        .datagram_receive_buffer_size(Some(64 << 10))
        .datagram_send_buffer_size(64 << 10)
        .max_concurrent_bidi_streams(128u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .initial_rtt(Duration::from_millis(100));
    // BBR, not quinn's default Cubic. p2_latency under netem (80 ms +-10 ms, 1% loss) with bulk on the
    // same connection: Cubic backs off on every random loss, so bulk crawled at 0.2 MB/s and keystrokes
    // still ran +22 ms (p50) / +43 ms (p95) over idle; BBR moved 5.1 MB/s with keystrokes within 1 ms
    // of idle. Cost: on loopback (RTT 0.1 ms) bulk adds a few ms to the keystroke tail (p99 2.6 -> 6.6 ms).
    // ponytail: quinn labels its BBR experimental; deleting this line goes back to Cubic.
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    Arc::new(t)
}

fn server_config(id: &ServerIdentity, allow: Allow) -> Result<quinn::ServerConfig, Error> {
    let provider = provider();
    let verifier = Arc::new(EnrolledDevices {
        allow,
        provider: provider.clone(),
    });
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_der.clone()));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(cfg_err)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![CertificateDer::from(id.cert_der.clone())], key)
        .map_err(cfg_err)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // Full handshake every time so `allow` always has the last word (no tickets, no 0-RTT).
    tls.send_tls13_tickets = 0;
    tls.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    let quic = QuicServerConfig::try_from(tls).map_err(cfg_err)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    // The server never pings: the client's keepalive (or its own traffic) keeps the idle timer fed,
    // and a ping from this side would wake a backgrounded desktop every second.
    cfg.transport_config(transport(None));
    Ok(cfg)
}

fn client_tls(server_fp: [u8; 32], dev: &DeviceIdentity) -> Result<rustls::ClientConfig, Error> {
    let provider = provider();
    let verifier = Arc::new(PinnedServer {
        fp: server_fp,
        provider: provider.clone(),
    });
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(dev.key_der.clone()));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(cfg_err)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![CertificateDer::from(dev.cert_der.clone())], key)
        .map_err(cfg_err)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // The daemon issues no tickets (see `server_config`): keep no resumption store either.
    tls.resumption = rustls::client::Resumption::disabled();
    Ok(tls)
}

fn client_config(
    server_fp: [u8; 32],
    dev: &DeviceIdentity,
    keepalive: Option<Duration>,
) -> Result<quinn::ClientConfig, Error> {
    let quic = QuicClientConfig::try_from(client_tls(server_fp, dev)?).map_err(cfg_err)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(transport(keepalive));
    Ok(cfg)
}

fn alert_name(alert: u64) -> &'static str {
    match alert {
        40 => "handshake_failure",
        42 => "bad_certificate",
        44 => "certificate_revoked",
        46 => "certificate_unknown",
        48 => "unknown_ca",
        49 => "access_denied",
        51 => "decrypt_error",
        70 => "protocol_version",
        80 => "internal_error",
        116 => "certificate_required",
        120 => "no_application_protocol",
        _ => "other",
    }
}

/// A QUIC connection error as ours. Codes 0x100..0x200 are TLS alerts: the handshake was refused
/// (the server's `allow` said no, or the pinned fingerprint did not match). So is the daemon's own
/// [`NOT_ENROLLED`] close, which is what a revocation that raced the handshake looks like.
pub(crate) fn conn_err(e: quinn::ConnectionError) -> Error {
    use quinn::ConnectionError as C;
    let code = match &e {
        C::ConnectionClosed(c) => Some(u64::from(c.error_code)),
        C::TransportError(t) => Some(u64::from(t.code)),
        _ => None,
    };
    match code.filter(|c| (0x100..0x200).contains(c)) {
        Some(code) => Error::Handshake(format!(
            "TLS alert {} ({}): {e}",
            code & 0xff,
            alert_name(code & 0xff)
        )),
        None if matches!(&e, C::ApplicationClosed(c) if c.error_code == NOT_ENROLLED) => {
            Error::Handshake(e.to_string())
        }
        None => Error::Closed(e.to_string()),
    }
}

pub(crate) fn read_err(e: quinn::ReadError) -> Error {
    match e {
        quinn::ReadError::Reset(_) => Error::Reset,
        quinn::ReadError::ConnectionLost(c) => conn_err(c),
        other => Error::Closed(other.to_string()),
    }
}

pub(crate) fn write_err(e: quinn::WriteError) -> Error {
    match e {
        quinn::WriteError::Stopped(_) => Error::Reset,
        quinn::WriteError::ConnectionLost(c) => conn_err(c),
        other => Error::Closed(other.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// the connection

type Accepted = (StreamKind, Tx, Rx);

/// One QUIC connection, either end. The server end only ever appears as `Arc<dyn Conn>`; the client
/// end is [`QuicClient`], which [`Conn::rebind`] can move to a fresh socket.
pub struct QuicConn {
    conn: quinn::Connection,
    /// The client's endpoint, kept for [`Conn::rebind`].
    ep: Option<quinn::Endpoint>,
    accepted: Mutex<mpsc::UnboundedReceiver<Accepted>>,
    pump: JoinHandle<()>,
}

/// The client end of a QUIC connection: a [`QuicConn`] that owns its endpoint, so it can
/// [`rebind`](Conn::rebind) to a fresh UDP socket.
pub type QuicClient = QuicConn;

impl Drop for QuicConn {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl QuicConn {
    fn new(
        conn: quinn::Connection,
        ep: Option<quinn::Endpoint>,
        keepalive: Option<Duration>,
    ) -> Arc<QuicConn> {
        let (out, accepted) = mpsc::unbounded_channel();
        let pump = tokio::spawn(accept_pump(conn.clone(), out, keepalive));
        Arc::new(QuicConn {
            conn,
            ep,
            accepted: Mutex::new(accepted),
            pump,
        })
    }

    /// Why the connection is dead, or a generic reason if quinn has not recorded one yet.
    fn dead(&self) -> Error {
        self.conn
            .close_reason()
            .map(conn_err)
            .unwrap_or_else(|| Error::Closed("connection closed".into()))
    }
}

/// Resolves when the peer has been silent too long: no UDP datagram for the keepalive interval plus
/// three round trips (at least 1.5 s; plan rule 7). Our keepalive makes a live peer answer within one
/// round trip, so silence means it or the path is gone. Quinn's 30 s idle timeout stays as the backstop.
async fn silent_peer(conn: &quinn::Connection, keepalive: Duration) {
    let mut tick = tokio::time::interval((keepalive + DEAD_MIN) / 4);
    let (mut seen, mut since) = (conn.stats().udp_rx.datagrams, Instant::now());
    loop {
        tick.tick().await;
        let n = conn.stats().udp_rx.datagrams;
        if n != seen {
            (seen, since) = (n, Instant::now());
        } else if since.elapsed() >= keepalive + (conn.rtt() * 3).max(DEAD_MIN) {
            return;
        }
    }
}

/// Accept streams the peer opens and read each one's kind frame in its own task, so a stream that
/// never sends one cannot hold up the others, and `Conn::accept` stays cancel-safe. With a
/// `keepalive` (the client end) it also closes the connection when the peer goes silent.
async fn accept_pump(
    conn: quinn::Connection,
    out: mpsc::UnboundedSender<Accepted>,
    keepalive: Option<Duration>,
) {
    let silent = async {
        match keepalive {
            Some(k) => silent_peer(&conn, k).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(silent);
    loop {
        let (send, recv) = tokio::select! {
            accepted = conn.accept_bi() => match accepted {
                Ok(s) => s,
                Err(_) => return,
            },
            () = &mut silent => {
                tracing::warn!("QUIC peer silent; closing the connection");
                conn.close(0u32.into(), b"peer silent");
                return;
            }
        };
        let out = out.clone();
        tokio::spawn(async move {
            let mut rx = Rx::new(RxIo::Quic(recv));
            match tokio::time::timeout(KIND_TIMEOUT, read_kind(&mut rx)).await {
                Ok(Ok(kind)) => {
                    let _ = send.set_priority(prio::of(&kind));
                    let _ = out.send((kind, Tx::new(TxIo::Quic(send), &kind), rx));
                }
                _ => {
                    let mut send = send;
                    let _ = send.reset(quinn::VarInt::from_u32(1));
                }
            }
        });
    }
}

#[async_trait]
impl Conn for QuicConn {
    async fn open(&self, kind: StreamKind, prio: i32) -> Result<(Tx, Rx), Error> {
        let (send, recv) = self.conn.open_bi().await.map_err(conn_err)?;
        let _ = send.set_priority(prio);
        let mut tx = Tx::new(TxIo::Quic(send), &kind);
        // The peer's accept_bi only fires once data arrives, so the kind frame goes out right away.
        tx.send(&kind).await?;
        Ok((tx, Rx::new(RxIo::Quic(recv))))
    }

    async fn accept(&self) -> Result<Accepted, Error> {
        match self.accepted.lock().await.recv().await {
            Some(s) => Ok(s),
            None => Err(self.dead()),
        }
    }

    fn send_datagram(&self, data: Bytes) -> Result<(), Error> {
        use quinn::SendDatagramError as E;
        if data.len() > MAX_DATAGRAM {
            return Err(Error::Datagram(format!(
                "{} bytes, the limit is {MAX_DATAGRAM}",
                data.len()
            )));
        }
        self.conn.send_datagram(data).map_err(|e| match e {
            E::ConnectionLost(c) => conn_err(c),
            E::TooLarge => Error::Datagram("larger than the path allows".into()),
            E::UnsupportedByPeer => Error::Datagram("the peer has datagrams disabled".into()),
            E::Disabled => Error::Datagram("datagrams are disabled".into()),
        })
    }

    async fn recv_datagram(&self) -> Result<Bytes, Error> {
        self.conn.read_datagram().await.map_err(conn_err)
    }

    fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    fn is_quic(&self) -> bool {
        true
    }

    fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }

    fn rebind(&self) -> Result<(), Error> {
        let ep = self
            .ep
            .as_ref()
            .ok_or_else(|| cfg_err("rebind: this is the server end of the connection"))?;
        // ponytail: an IPv6 socket relies on the OS default for dual-stack (true on Linux and
        // macOS, not Windows); build it with socket2 if Windows ever matters.
        let bind: SocketAddr = if ep.local_addr()?.is_ipv6() {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        };
        ep.rebind(std::net::UdpSocket::bind(bind)?)?;
        Ok(())
    }

    // ponytail: quinn fixes the keepalive interval when the connection is made, so there is one
    // interval for its whole life: connect with KEEPALIVE_BACKGROUND, or reconnect, when the app hides.
    fn set_background(&self, _background: bool) {}

    async fn closed(&self) -> Error {
        conn_err(self.conn.closed().await)
    }
}

impl QuicConn {
    /// Connect to the daemon at `addr`, which must present the certificate whose SHA-256 is
    /// `server_fp`, and authenticate as `dev`. Fails after 5 s if the handshake does not complete
    /// (UDP blocked, daemon down).
    ///
    /// `keepalive` is the QUIC keepalive period (`Some(KEEPALIVE_FOREGROUND)` normally,
    /// `Some(KEEPALIVE_BACKGROUND)` for a connection made while the app is in the background). Quinn
    /// fixes it for the life of the connection; to change it, reconnect. With a keepalive the
    /// connection is closed when nothing has come from the daemon for that period plus three round
    /// trips (at least 1.5 s); with `None` there are no pings and only the 30 s idle timeout.
    ///
    /// With mutual TLS 1.3 the client finishes its handshake before the server has looked at its
    /// certificate, so a device that is not enrolled (or was revoked) can see `Ok` here and then
    /// [`Error::Handshake`] from the first stream operation or from [`Conn::closed`]. The free
    /// function [`connect`] waits that verdict out; here it is yours to handle. Either way the
    /// server's `Welcome` is the proof that the connection is accepted.
    pub async fn connect(
        addr: SocketAddr,
        server_fp: [u8; 32],
        dev: &DeviceIdentity,
        keepalive: Option<Duration>,
    ) -> Result<Arc<QuicClient>, Error> {
        let cfg = client_config(server_fp, dev, keepalive)?;
        let bind: SocketAddr = if addr.is_ipv6() {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        };
        let ep = quinn::Endpoint::client(bind)?;
        let connecting = ep
            .connect_with(cfg, addr, SERVER_NAME)
            .map_err(|e| Error::Connect(e.to_string()))?;
        // ponytail: this is where `connecting.into_0rtt()` would go to send an idempotent
        // Hello/Subscribe before the handshake completes (M5). It needs session tickets on both
        // sides and `ClientConfig::enable_early_data`, which this build turns off so that revoking
        // a device takes effect on its next connection.
        let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting).await {
            Err(_) => {
                return Err(Error::Connect(format!(
                    "no QUIC handshake with {addr} within {HANDSHAKE_TIMEOUT:?}"
                )))
            }
            Ok(Err(e)) => {
                return Err(match conn_err(e) {
                    Error::Closed(m) => Error::Connect(m),
                    other => other,
                })
            }
            Ok(Ok(conn)) => conn,
        };
        Ok(QuicConn::new(conn, Some(ep), keepalive))
    }

    /// The local UDP address of the endpoint (changes after [`rebind`](Conn::rebind)).
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        let ep = self
            .ep
            .as_ref()
            .ok_or_else(|| cfg_err("local_addr: this is the server end of the connection"))?;
        Ok(ep.local_addr()?)
    }
}

/// [`QuicClient::connect`] with the foreground keepalive, as a plain `Arc<dyn Conn>`, once the daemon
/// has had time to refuse the device: TLS 1.3 lets the client finish before the daemon looks at its
/// certificate, so the refusal (or a revocation that raced the handshake) arrives about a round trip
/// after the handshake, and is returned here as [`Error::Handshake`] (it waits twice the RTT, at least
/// 50 ms). Use [`QuicClient::connect`] for another keepalive or to skip the wait.
pub async fn connect(
    addr: SocketAddr,
    server_fp: [u8; 32],
    dev: &DeviceIdentity,
) -> Result<Arc<dyn Conn>, Error> {
    let conn = QuicClient::connect(addr, server_fp, dev, Some(KEEPALIVE_FOREGROUND)).await?;
    let settle = (conn.rtt() * 2).max(Duration::from_millis(50));
    tokio::select! {
        e = conn.closed() => Err(e),
        () = tokio::time::sleep(settle) => Ok(conn),
    }
}

// ---------------------------------------------------------------------------------------------
// the listener

/// The daemon's QUIC endpoint: hands out one authenticated connection per enrolled device
/// connection. Dropping it closes the endpoint and every connection it accepted.
pub struct Listener {
    ep: quinn::Endpoint,
    rx: Mutex<mpsc::Receiver<(DeviceId, Arc<dyn Conn>)>>,
    task: JoinHandle<()>,
}

/// Bind a UDP socket at `bind` and accept QUIC connections from enrolled devices.
///
/// `allow` maps the SHA-256 of a client's public key (SPKI) to its [`DeviceId`], or `None` to
/// refuse it; see [`Allow`] for what it may do. Handshakes that fail, time out (5 s) or are refused
/// are dropped with a log line and never reach [`Listener::accept`].
///
/// ponytail: no Retry or handshake limit; quinn's own caps apply. When floods show up, call
/// `incoming.retry()` for unvalidated addresses in `accept_loop`.
pub async fn listen(
    bind: SocketAddr,
    id: &ServerIdentity,
    allow: Allow,
) -> Result<Listener, Error> {
    let cfg = server_config(id, allow.clone())?;
    let ep = quinn::Endpoint::server(cfg, bind)?;
    let (tx, rx) = mpsc::channel(32);
    let task = tokio::spawn(accept_loop(ep.clone(), allow, tx));
    Ok(Listener {
        ep,
        rx: Mutex::new(rx),
        task,
    })
}

impl Listener {
    /// The next authenticated connection and the device behind it. Cancel-safe. Fails once the
    /// listener is closed.
    pub async fn accept(&self) -> Result<(DeviceId, Arc<dyn Conn>), Error> {
        self.rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| Error::Closed("listener closed".into()))
    }

    /// The bound UDP address (useful after binding port 0).
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.ep.local_addr()?)
    }

    /// Stop accepting and close every connection.
    pub fn close(&self) {
        self.ep.close(0u32.into(), b"server shutting down");
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
        self.ep.close(0u32.into(), b"server shutting down");
    }
}

async fn accept_loop(
    ep: quinn::Endpoint,
    allow: Allow,
    out: mpsc::Sender<(DeviceId, Arc<dyn Conn>)>,
) {
    while let Some(incoming) = ep.accept().await {
        let (allow, out) = (allow.clone(), out.clone());
        // One task per handshake, so a slow or hostile peer holds up nobody else.
        tokio::spawn(async move {
            let peer = incoming.remote_address();
            let handshake = async { incoming.accept()?.await };
            let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return tracing::debug!(%peer, "QUIC handshake failed: {e}"),
                Err(_) => return tracing::debug!(%peer, "QUIC handshake timed out"),
            };
            // The verifier already asked `allow`; ask again so a revocation that raced the handshake
            // still wins, and to learn which device this is.
            let device = peer_spki(&conn).and_then(|spki| allow(&spki));
            let Some(device) = device else {
                conn.close(NOT_ENROLLED, b"device not enrolled");
                return tracing::warn!(%peer, "QUIC connection dropped: device not enrolled");
            };
            let conn: Arc<dyn Conn> = QuicConn::new(conn, None, None);
            let _ = out.send((device, conn)).await;
        });
    }
}

/// SPKI hash of the end-entity certificate the peer authenticated with.
fn peer_spki(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let chain = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    spki_sha256_of_cert(chain.first()?.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_keeps_no_resumption_store() {
        let tls = client_tls([0; 32], &DeviceIdentity::generate("alice")).unwrap();
        // The stock store would be a memory cache; `disabled()` prints as no storage at all.
        assert!(
            format!("{:?}", tls.resumption).contains("NoClientSessionStorage"),
            "{:?}",
            tls.resumption
        );
    }

    #[test]
    fn a_failed_signature_check_is_named() {
        // Alert 51 is what a client that cannot sign for its certificate gets back.
        assert_eq!(alert_name(51), "decrypt_error");
    }

    // A certificate made with `openssl req -x509` and the values openssl itself printed for it:
    //   openssl x509 -in c.pem -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum
    //   openssl x509 -in c.pem -outform DER | sha256sum
    const VECTOR_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBkDCCATWgAwIBAgIUCMui9YeP5QJNiV/lpRbpVrdOWaYwCgYIKoZIzj0EAwIw
HDEaMBgGA1UEAwwRZmVtbWUtdGVzdC12ZWN0b3IwIBcNMjYxMTAyMDk0NzE0WhgP
MjEyNjEwMDkwOTQ3MTRaMBwxGjAYBgNVBAMMEWZlbW1lLXRlc3QtdmVjdG9yMFkw
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEq8NIpgGsCES64QSe2l1XnDc615ZQmOe/
Z+qvZDJ2Cg8NscAWuxPRqVGV0RT1I/TwQY2wx/F9lEbGEHsH++YaxqNTMFEwHQYD
VR0OBBYEFOprROts5XstRJJXnf2lH+1YOm6NMB8GA1UdIwQYMBaAFOprROts5Xst
RJJXnf2lH+1YOm6NMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSQAwRgIh
AN8ECmuvcrLsMTsteuL+PvGtRNosatspMu9QbymVH7c2AiEAnKF8ydIF9anGN0Sf
cXux51C4rqhtt4/LgPxKPFoWQrs=
-----END CERTIFICATE-----
";
    const VECTOR_SPKI_SHA256: &str =
        "1c704699e05e245534a82baac477029e1806f7a5c07eb9a7a1364efd66e1ce45";
    const VECTOR_CERT_SHA256: &str =
        "8c8303d3ca6f0e127a47a818ab34b0279efe86054ed469a7268890ea79178c0f";

    #[test]
    fn sha256_known_answer() {
        // FIPS 180-2 test vector for "abc".
        let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(crate::fp_to_hex(&sha256(b"abc")), want);
        assert_eq!(
            crate::fp_to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn spki_and_cert_hash_match_openssl() {
        let der = pem::parse(VECTOR_PEM).unwrap().into_contents();
        assert_eq!(
            crate::fp_to_hex(&spki_sha256_of_cert(&der).unwrap()),
            VECTOR_SPKI_SHA256
        );
        assert_eq!(crate::fp_to_hex(&sha256(&der)), VECTOR_CERT_SHA256);
        assert!(spki_sha256_of_cert(b"not a certificate").is_none());
    }

    #[test]
    fn identities_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let server = ServerIdentity::load_or_generate(
            dir.path(),
            &[
                "localhost".into(),
                "10.0.0.1".into(),
                "not a name\u{e9}".into(),
            ],
        )
        .unwrap();
        let again = ServerIdentity::load_or_generate(dir.path(), &[]).unwrap();
        assert_eq!(server.cert_der, again.cert_der);
        assert_eq!(server.key_der, again.key_der);
        assert_eq!(server.fingerprint(), sha256(&server.cert_der));

        let dev = DeviceIdentity::load_or_generate(dir.path(), "alice's laptop").unwrap();
        let dev2 = DeviceIdentity::load(dir.path()).unwrap();
        assert_eq!(dev.spki_sha256(), dev2.spki_sha256());
        assert_ne!(dev.spki_sha256().unwrap(), server.fingerprint());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for key in ["server.key", "device.key"] {
                let mode = std::fs::metadata(dir.path().join(key))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{key} must be private");
            }
        }
        assert!(ServerIdentity::load(&dir.path().join("missing")).is_err());
        std::fs::write(dir.path().join("server.crt"), "junk").unwrap();
        assert!(ServerIdentity::load(dir.path()).is_err());
    }

    #[test]
    fn two_generated_devices_differ() {
        let (a, b) = (DeviceIdentity::generate("a"), DeviceIdentity::generate("a"));
        assert_ne!(a.spki_sha256(), b.spki_sha256());
    }
}
