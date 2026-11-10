//! `/femme/ws`: the phone's way in, on the HTTP layer (behind Caddy's HTTPS when the board is
//! public). The same connection the desktop makes over QUIC, carried as JSON over one WebSocket
//! (`femme_transport::ws`), served by the same [`serve_conn`](super::serve_conn): `Hello`,
//! subscriptions, requests, terminals, all of it.
//!
//! Who may connect: a device `femmed pair` enrolled. Its token travels as `Authorization: Bearer
//! <token>` (or `?token=`, for clients that cannot set headers), and the device row's key is the
//! SHA-256 of the token, so `femmed devices` lists and revokes phones like any desktop, and a
//! revoke ends the connection at the next check as it does for QUIC.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use femme_proto::DeviceId;
use femme_store::{auth, sha256};
use femme_transport::{Conn, WsConn};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use super::{serve_conn, WsGate};

/// The biggest carrier message either way: a `BlobPut` of `MAX_BLOB_PUT` bytes written as a JSON
/// array is about four times its size.
const MAX_MESSAGE: usize = 40 << 20;

/// The device a request speaks for, by its token.
fn device(
    gate: &WsGate,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<(Arc<super::Front>, DeviceId), (StatusCode, &'static str)> {
    let Some(front) = gate.front() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the front is not up"));
    };
    let Some(daemon) = front.daemon.upgrade() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping"));
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .or(query.get("token").map(String::as_str))
        .unwrap_or_default();
    if bearer.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "no token"));
    }
    // ponytail: nothing slows a guesser down; the token is 256 random bits, so a guess is not a
    // risk, only a log line. A per-IP delay on misses if the log ever fills with them.
    let found = daemon
        .store
        .read(|c| auth::device_by_spki(c, &sha256(bearer.as_bytes())))
        .unwrap_or_else(|e| {
            tracing::warn!(%e, "device lookup failed; refusing the phone");
            None
        });
    match found {
        Some(d) => Ok((front, d.id)),
        None => Err((StatusCode::UNAUTHORIZED, "unknown or revoked device")),
    }
}

/// `GET /femme/ws`: upgrades, then serves the connection until it ends.
pub(crate) async fn upgrade(
    State(gate): State<Arc<WsGate>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let (front, device) = match device(&gate, &headers, &query) {
        Ok(d) => d,
        Err(refused) => return refused.into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| serve(front, device, socket))
}

/// Pumps the socket into a [`WsConn`] and serves it as any connection.
async fn serve(front: Arc<super::Front>, device: DeviceId, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let conn = WsConn::start(out_tx, in_rx);
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                    Ok(t) => t,
                    Err(_) => break,
                },
                Message::Close(_) => break,
                // pings and pongs are axum's
                _ => continue,
            };
            if in_tx.send(text).await.is_err() {
                break;
            }
        }
        // dropping `in_tx` ends the carrier: the connection is dead
    });
    let conn: Arc<dyn Conn> = conn;
    serve_conn(front, device, conn).await;
    writer.abort();
    reader.abort();
}
