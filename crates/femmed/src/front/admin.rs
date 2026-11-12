//! `femmed enroll`, `femmed devices` and `femmed devices revoke`: the SSH login is the root of
//! trust, so these edit the database directly (the same file the running daemon has open: SQLite
//! serialises the two) and the daemon sees the change on the next connection.

use anyhow::{bail, Context};
use femme_proto::DeviceId;
use femme_store::{auth, Device, Store};
use femme_transport::{fp_from_hex, fp_to_hex};

use crate::config::Config;
use crate::daemon::now_ms;

fn store(cfg: &Config) -> anyhow::Result<Store> {
    let path = cfg.state_dir.join("femme.db");
    Store::open(&path).with_context(|| format!("cannot open {}", path.display()))
}

/// Registers (or re-registers: same key keeps its id, takes the new name and is let back in) a
/// device by the SHA-256 of its certificate's public key. The line it returns is what the desktop
/// pins: the daemon's fingerprint, the UDP port, and the ssh host.
pub(crate) fn enroll(cfg: &Config, name: &str, spki: &str) -> anyhow::Result<String> {
    let spki = fp_from_hex(spki)
        .context("--spki is 64 hex digits: the SHA-256 of the device certificate's public key")?;
    // The identity first: no point recording a device the daemon cannot be reached by.
    let fingerprint = super::identity(cfg)?.fingerprint();
    let id = store(cfg)?.write(|tx| tx.enroll_device(name, &spki, now_ms()))?;
    tracing::debug!(%id, "device enrolled");
    Ok(format!(
        "fingerprint={} quic_port={} host={}",
        fp_to_hex(&fingerprint),
        cfg.quic_port,
        cfg.host
    ))
}

/// Enrolls a phone: a fresh token is its credential (`Authorization: Bearer` on `/femme/ws`), and
/// the device row's key is the token's SHA-256, so the daemon finds it the way it finds a desktop
/// by its certificate. Prints the `femme://pair` link, as a QR code and as text (for typing it).
pub(crate) fn pair(cfg: &Config, name: &str) -> anyhow::Result<String> {
    use base64::Engine;
    let mut raw = [0u8; 32];
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").context("random bytes")?,
        &mut raw,
    )?;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let id = store(cfg)?
        .write(|tx| tx.enroll_device(name, &femme_store::sha256(token.as_bytes()), now_ms()))?;
    tracing::debug!(%id, "phone enrolled");
    // Where the phone connects: the board's public host (Caddy, HTTPS) when there is one, else the
    // HTTP layer itself (a LAN, a tunnel).
    let (host, tls) = match cfg.board_url.split_once("://") {
        Some(("https", host)) => (host.to_owned(), true),
        Some((_, host)) => (host.to_owned(), false),
        None => (cfg.http_bind.clone(), false),
    };
    let link = format!(
        "femme://pair?host={host}&tls={}&name={}&token={token}",
        u8::from(tls),
        cfg.host
    );
    let qr = qrcode::QrCode::new(link.as_bytes()).context("the link fits a QR code")?;
    let picture = qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    Ok(format!(
        "{picture}\n{link}\n\nenrolled {name} ({id}): scan with the FEMME app, or paste the link into it. \
         Revoke with `femmed devices revoke {name}`."
    ))
}

/// One line per device, oldest first: id, name, key, when it was made and last seen, and whether
/// it was revoked.
pub(crate) fn devices(cfg: &Config) -> anyhow::Result<String> {
    let all = store(cfg)?.read(auth::list_devices)?;
    let now = now_ms();
    let ago = |ms: i64| format!("{} ago", span((now - ms).max(0) / 1000));
    let lines: Vec<String> = all
        .iter()
        .map(|d| {
            format!(
                "{} {} {} created {} last seen {}{}",
                d.id,
                d.name,
                fp_to_hex(&d.spki_sha256),
                ago(d.created),
                d.last_seen.map_or_else(|| "never".into(), ago),
                d.revoked
                    .map_or_else(String::new, |r| format!(" REVOKED {}", ago(r))),
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "no devices enrolled".into()
    } else {
        lines.join("\n")
    })
}

/// Revokes the device with this id, or this name when it is the only one called that. It is
/// refused on its next connection (TLS 1.3 lets a client think it connected for a round trip,
/// then the daemon says no), and the daemon closes the ones it has open at its next check.
pub(crate) fn revoke(cfg: &Config, device: &str) -> anyhow::Result<String> {
    let store = store(cfg)?;
    let all = store.read(auth::list_devices)?;
    let target = pick(&all, device)?;
    store.write(|tx| tx.revoke_device(target.id))?;
    Ok(format!(
        "revoked {} ({}); a running femmed closes its connections within {}s",
        target.name,
        target.id,
        super::RECHECK.as_secs()
    ))
}

/// The device `which` names: an id, else a name nothing else shares.
fn pick<'a>(all: &'a [Device], which: &str) -> anyhow::Result<&'a Device> {
    if let Ok(id) = which.parse::<DeviceId>() {
        if let Some(d) = all.iter().find(|d| d.id == id) {
            return Ok(d);
        }
    }
    let named: Vec<&Device> = all.iter().filter(|d| d.name == which).collect();
    match named.as_slice() {
        [d] => Ok(d),
        [] => bail!("no device with the id or name {which:?} (femmed devices lists them)"),
        many => bail!(
            "{} devices are named {which:?}: use an id ({})",
            many.len(),
            many.iter()
                .map(|d| d.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `90` seconds is `1m`, 3 hours `3h`: the biggest unit that fits, whole.
fn span(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_use_the_biggest_whole_unit() {
        let got: Vec<String> = [0, 59, 60, 3599, 3600, 86_399, 86_400, 400_000]
            .into_iter()
            .map(span)
            .collect();
        assert_eq!(got, ["0s", "59s", "1m", "59m", "1h", "23h", "1d", "4d"]);
    }
}
