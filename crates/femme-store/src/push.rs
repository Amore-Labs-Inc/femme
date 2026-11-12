//! Phones that asked for pushes (`Req::PushRegister`): one row per (device, token), and since a
//! phone has one topic, one row per device in practice. A revoked device's rows go with the
//! revoke ([`Tx::revoke_device`]), and [`push_list`] skips revoked devices besides.
//!
//! Not replicated to clients, so nothing here touches `rev`.

use femme_proto::rpc::{PushPlatform, PushRules};
use femme_proto::DeviceId;
use rusqlite::{params, Connection};

use crate::{all, blob_id, id_blob, Error, Result, Tx};

/// One registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    /// The paired device that registered.
    pub device: DeviceId,
    /// How the pushes are delivered.
    pub platform: PushPlatform,
    /// Where the pushes go: the ntfy topic for [`PushPlatform::Ntfy`].
    pub token: String,
    /// Which attention changes are pushed.
    pub rules: PushRules,
    /// Unix ms.
    pub created: i64,
}

fn platform_text(p: PushPlatform) -> &'static str {
    match p {
        PushPlatform::Ntfy => "Ntfy",
        PushPlatform::Apns => "Apns",
        PushPlatform::Fcm => "Fcm",
    }
}

fn platform_parse(s: &str) -> Option<PushPlatform> {
    Some(match s {
        "Ntfy" => PushPlatform::Ntfy,
        "Apns" => PushPlatform::Apns,
        "Fcm" => PushPlatform::Fcm,
        _ => return None,
    })
}

/// Every registration of a device that is still enrolled, oldest first.
pub fn push_list(conn: &Connection) -> Result<Vec<Push>> {
    all(
        conn,
        "SELECT p.device, p.platform, p.token, p.rules, p.created FROM push p
         JOIN devices d ON d.id = p.device WHERE d.revoked IS NULL ORDER BY p.created, p.token",
        [],
        |r| {
            let rules: String = r.get(3)?;
            Ok(Push {
                device: blob_id(r.get(0)?),
                platform: crate::enum_col(r, 1, platform_parse)?,
                token: r.get(2)?,
                rules: serde_json::from_str(&rules)?,
                created: r.get(4)?,
            })
        },
    )
}

impl Tx<'_> {
    /// Registers `token` for `device`, replacing whatever the device had registered before (one
    /// phone, one topic). `NotFound` when the device is not enrolled.
    pub fn push_register(
        &mut self,
        device: DeviceId,
        platform: PushPlatform,
        token: &str,
        rules: &PushRules,
    ) -> Result<()> {
        let enrolled = crate::exists(
            self,
            "SELECT 1 FROM devices WHERE id = ?1 AND revoked IS NULL",
            [id_blob(device)],
        )?;
        if !enrolled {
            return Err(Error::NotFound);
        }
        let now = self.now();
        self.execute("DELETE FROM push WHERE device = ?1", [id_blob(device)])?;
        self.execute(
            "INSERT INTO push (device, platform, token, rules, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id_blob(device),
                platform_text(platform),
                token,
                serde_json::to_string(rules)?,
                now
            ],
        )?;
        Ok(())
    }

    /// Drops `device`'s registration of `token`; whether there was one.
    pub fn push_unregister(&mut self, device: DeviceId, token: &str) -> Result<bool> {
        let n = self.execute(
            "DELETE FROM push WHERE device = ?1 AND token = ?2",
            params![id_blob(device), token],
        )?;
        Ok(n > 0)
    }
}
