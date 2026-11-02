//! Racing QUIC against the SSH mux when connecting (plan §3 rule 7).
//!
//! Re-probing QUIC every 60 s while the app is on SSH is the caller's job: call
//! [`connect`](crate::connect) again and, if it succeeds, move the session over and close the SSH
//! connection.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::{Conn, Error};

/// How long an SSH connection that is ready first waits for QUIC to catch up.
const QUIC_GRACE: Duration = Duration::from_millis(300);

/// A connection attempt in flight, for [`race`].
pub type ConnFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Conn>, Error>> + Send>>;

/// What [`race`] kept.
pub struct Best {
    /// The connection to use.
    pub conn: Arc<dyn Conn>,
    /// Why QUIC lost when it failed outright (not when it was merely slow, or not tried). An
    /// [`Error::Handshake`] means the daemon was reached and refused this device (revoked, wrong pin):
    /// the user has to hear about that even though SSH works.
    pub quic_error: Option<Error>,
}

/// Race two connection attempts and keep one. QUIC wins if it connects first, or within 300 ms of
/// SSH connecting first; otherwise SSH is used and the QUIC attempt is abandoned. The loser is
/// dropped (a dropped `ssh` attempt kills its process; a ready SSH connection is closed). If one
/// attempt fails the other is awaited alone, and when that is QUIC's failure it comes back with the
/// SSH connection in [`Best::quic_error`]; if both fail the error names both. With only one attempt
/// given it is awaited as is, and with none the result is [`Error::Connect`].
pub async fn race(quic: Option<ConnFuture>, ssh: Option<ConnFuture>) -> Result<Best, Error> {
    let best = |conn| Best {
        conn,
        quic_error: None,
    };
    let (mut quic, mut ssh) = match (quic, ssh) {
        (None, None) => return Err(Error::Connect("no transport configured".into())),
        (Some(q), None) => return q.await.map(best),
        (None, Some(s)) => return s.await.map(best),
        (Some(q), Some(s)) => (q, s),
    };
    tokio::select! {
        q = &mut quic => match q {
            Ok(q) => Ok(best(q)),
            Err(qe) => {
                tracing::info!("QUIC failed ({qe}); waiting for SSH");
                match ssh.await {
                    Ok(conn) => Ok(Best { conn, quic_error: Some(qe) }),
                    Err(se) => Err(both_failed(qe, se)),
                }
            }
        },
        s = &mut ssh => match s {
            Ok(s) => match tokio::time::timeout(QUIC_GRACE, &mut quic).await {
                Ok(Ok(q)) => {
                    s.close();
                    Ok(best(q))
                }
                Ok(Err(qe)) => {
                    tracing::warn!("QUIC failed ({qe}); using SSH");
                    Ok(Best { conn: s, quic_error: Some(qe) })
                }
                Err(_) => {
                    tracing::info!("QUIC not up within {QUIC_GRACE:?} of SSH; using SSH");
                    Ok(best(s))
                }
            },
            Err(se) => quic.await.map(best).map_err(|qe| both_failed(qe, se)),
        },
    }
}

/// Both attempts failed. A QUIC handshake refusal stays a [`Error::Handshake`]: the server was
/// reached and said no (wrong pin, device revoked), which the user has to hear about.
fn both_failed(qe: Error, se: Error) -> Error {
    let msg = format!("QUIC: {qe}; SSH: {se}");
    if matches!(qe, Error::Handshake(_)) {
        Error::Handshake(msg)
    } else {
        Error::Connect(msg)
    }
}

/// Connect to the daemon over QUIC at `quic` (address, pinned server fingerprint, device identity)
/// and/or SSH at `ssh_host` (`ssh -T host femmed proxy`, see [`ssh_connect`](crate::ssh_connect)),
/// whichever answers first, with [`race`]'s rules. The QUIC attempt is [`connect`](crate::connect),
/// which waits for the daemon's verdict on the device: a refusal shows up as [`Best::quic_error`],
/// never as a QUIC connection that dies on first use. The server's `Welcome` is still the proof that
/// a connection is accepted.
pub async fn connect_best(
    quic: Option<(std::net::SocketAddr, [u8; 32], crate::DeviceIdentity)>,
    ssh_host: Option<String>,
) -> Result<Best, Error> {
    let quic: Option<ConnFuture> = quic.map(|(addr, fp, dev)| -> ConnFuture {
        Box::pin(async move { crate::quic::connect(addr, fp, &dev).await })
    });
    let ssh: Option<ConnFuture> = ssh_host.map(|host| -> ConnFuture {
        Box::pin(async move { crate::mux::ssh_connect(&host, &["femmed", "proxy"]).await })
    });
    race(quic, ssh).await
}
