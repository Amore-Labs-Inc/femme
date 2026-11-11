//! Who may talk to the daemon: enrolled devices, web sessions, API keys; plus the request-dedup
//! table and per-device keybindings.
//!
//! Nothing here is replicated to clients, so none of it touches `rev`.

use femme_proto::{DeviceId, Id, OpId};
use rusqlite::{params, Connection, Row};

use crate::{all, blob_id, id_blob, non_empty, one, Error, Result, Tx};

/// An enrolled desktop device. It authenticates with a client certificate whose SPKI hash is
/// `spki_sha256`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The device's id.
    pub id: DeviceId,
    /// Name given at enrollment (`ssh agentbox femmed enroll --name <dev> ..`).
    pub name: String,
    /// SHA-256 of the certificate's SubjectPublicKeyInfo.
    pub spki_sha256: [u8; 32],
    /// Unix ms.
    pub created: i64,
    /// Unix ms of the last [`Tx::touch_device`].
    pub last_seen: Option<i64>,
    /// Unix ms when it was revoked; `None` while it is allowed in.
    pub revoked: Option<i64>,
}

/// An API key made on the board for another agent (Instinct), optionally carrying the GitHub
/// account its clones push as.
///
/// `Debug` hides the two secrets (`key`, `github_token`), so logging a value cannot leak them.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// `a-z0-9-`, at most 32 characters (validated by the caller); the key's identity.
    pub name: String,
    /// The secret. Stored as given, like the board did; look it up with [`api_key_get`].
    pub key: String,
    /// GitHub token its clones use, if one was attached.
    pub github_token: Option<String>,
    /// GitHub login that token belongs to.
    pub github_login: Option<String>,
    /// `git config user.name` for its commits.
    pub git_name: Option<String>,
    /// `git config user.email` for its commits.
    pub git_email: Option<String>,
    /// Unix ms.
    pub created: i64,
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("name", &self.name)
            .field("key", &"<redacted>")
            .field(
                "github_token",
                &self.github_token.as_ref().map(|_| "<redacted>"),
            )
            .field("github_login", &self.github_login)
            .field("git_name", &self.git_name)
            .field("git_email", &self.git_email)
            .field("created", &self.created)
            .finish()
    }
}

const DEVICE_COLS: &str = "id, name, spki_sha256, created, last_seen, revoked";
const KEY_COLS: &str = "name, key, github_token, github_login, git_name, git_email, created";

fn device_from_row(r: &Row<'_>) -> Result<Device> {
    Ok(Device {
        id: blob_id(r.get(0)?),
        name: r.get(1)?,
        spki_sha256: r.get(2)?,
        created: r.get(3)?,
        last_seen: r.get(4)?,
        revoked: r.get(5)?,
    })
}

pub(crate) fn key_from_row(r: &Row<'_>) -> Result<ApiKey> {
    Ok(ApiKey {
        name: r.get(0)?,
        key: r.get(1)?,
        github_token: r.get(2)?,
        github_login: r.get(3)?,
        git_name: r.get(4)?,
        git_email: r.get(5)?,
        created: r.get(6)?,
    })
}

/// The device with this certificate hash, unless it was revoked (then `None`: it may not connect).
pub fn device_by_spki(conn: &Connection, spki_sha256: &[u8; 32]) -> Result<Option<Device>> {
    one(
        conn,
        &format!("SELECT {DEVICE_COLS} FROM devices WHERE spki_sha256 = ?1 AND revoked IS NULL"),
        [spki_sha256],
        device_from_row,
    )
}

/// Every device, revoked ones included, oldest first (for `femmed devices list`).
pub fn list_devices(conn: &Connection) -> Result<Vec<Device>> {
    all(
        conn,
        &format!("SELECT {DEVICE_COLS} FROM devices ORDER BY created, id"),
        [],
        device_from_row,
    )
}

/// Is this web-session token (given as its SHA-256) known and unexpired at `now` (unix ms)?
pub fn web_session_valid(conn: &Connection, token_sha256: &[u8; 32], now: i64) -> Result<bool> {
    crate::exists(
        conn,
        "SELECT 1 FROM web_sessions WHERE token_sha256 = ?1 AND expires > ?2",
        params![token_sha256, now],
    )
}

/// Every API key, oldest first.
pub fn api_keys_list(conn: &Connection) -> Result<Vec<ApiKey>> {
    all(
        conn,
        &format!("SELECT {KEY_COLS} FROM api_keys ORDER BY created, name"),
        [],
        key_from_row,
    )
}

/// The API key whose secret is `key` (a bearer token). The comparison is constant-time over
/// every stored key, like the board's `same()`: how long the lookup takes says nothing about how
/// much of a guess was right. An empty `key` matches nothing.
pub fn api_key_get(conn: &Connection, key: &str) -> Result<Option<ApiKey>> {
    if key.is_empty() {
        return Ok(None);
    }
    let mut found = None;
    for k in api_keys_list(conn)? {
        if same(k.key.as_bytes(), key.as_bytes()) {
            found = Some(k);
        }
    }
    Ok(found)
}

/// Compares in the same time wherever the first difference is.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |acc, (x, y)| std::hint::black_box(acc | (x ^ y)))
            == 0
}

/// The API key with this name.
pub fn api_key_by_name(conn: &Connection, name: &str) -> Result<Option<ApiKey>> {
    one(
        conn,
        &format!("SELECT {KEY_COLS} FROM api_keys WHERE name = ?1"),
        [name],
        key_from_row,
    )
}

/// The cached response of an operation this device already ran (`None` if it is new). Look up
/// inside the same write transaction that will record the new one, so two racing retries cannot
/// both run.
pub fn op_lookup(conn: &Connection, device: DeviceId, op: OpId) -> Result<Option<Vec<u8>>> {
    one(
        conn,
        "SELECT resp FROM ops WHERE device = ?1 AND op_id = ?2",
        params![id_blob(device), id_blob(op)],
        |r| Ok(r.get(0)?),
    )
}

/// The device's keybinding rules (JSON `[{key, command, when}]`), if it saved any.
pub fn keybindings_get(conn: &Connection, device: DeviceId) -> Result<Option<String>> {
    one(
        conn,
        "SELECT json FROM keybindings WHERE device = ?1",
        [id_blob(device)],
        |r| Ok(r.get(0)?),
    )
}

impl Tx<'_> {
    /// Enrolls a device, or re-enrolls it: a known `spki_sha256` keeps its id, takes the new
    /// `name` and is un-revoked (enrolling is an explicit act over SSH, the root of trust).
    pub fn enroll_device(
        &mut self,
        name: &str,
        spki_sha256: &[u8; 32],
        now: i64,
    ) -> Result<DeviceId> {
        let known: Option<[u8; 16]> = one(
            self,
            "SELECT id FROM devices WHERE spki_sha256 = ?1",
            [spki_sha256],
            |r| Ok(r.get(0)?),
        )?;
        if let Some(id) = known {
            self.execute(
                "UPDATE devices SET name = ?2, revoked = NULL WHERE id = ?1",
                params![id, name],
            )?;
            return Ok(blob_id(id));
        }
        let id = Id::new();
        self.execute(
            "INSERT INTO devices (id, name, spki_sha256, created) VALUES (?1, ?2, ?3, ?4)",
            params![id_blob(id), name, spki_sha256, now],
        )?;
        Ok(id)
    }

    /// Revokes a device (at [`Tx::now`]); it can no longer connect, and its push registrations
    /// are dropped. `NotFound` if unknown.
    pub fn revoke_device(&mut self, id: DeviceId) -> Result<()> {
        let n = self.execute(
            "UPDATE devices SET revoked = COALESCE(revoked, ?2) WHERE id = ?1",
            params![id_blob(id), self.now()],
        )?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.execute("DELETE FROM push WHERE device = ?1", [id_blob(id)])?;
        Ok(())
    }

    /// Notes that the device connected at `now`. Cheap, but it is a write transaction:
    /// `femmed` should throttle it (once a minute is plenty).
    pub fn touch_device(&mut self, id: DeviceId, now: i64) -> Result<()> {
        self.execute(
            "UPDATE devices SET last_seen = ?2 WHERE id = ?1",
            params![id_blob(id), now],
        )?;
        Ok(())
    }

    /// Remembers a web session: the SHA-256 of its cookie token and its expiry (unix ms).
    /// Only the hash is stored; use [`sha256`](crate::sha256) on the cookie value.
    pub fn web_session_insert(&mut self, token_sha256: &[u8; 32], expires: i64) -> Result<()> {
        self.execute(
            "INSERT INTO web_sessions (token_sha256, expires) VALUES (?1, ?2)
             ON CONFLICT(token_sha256) DO UPDATE SET expires = excluded.expires",
            params![token_sha256, expires],
        )?;
        Ok(())
    }

    /// Forgets a web session (logout). Deleting an unknown one is fine.
    pub fn web_session_delete(&mut self, token_sha256: &[u8; 32]) -> Result<()> {
        self.execute(
            "DELETE FROM web_sessions WHERE token_sha256 = ?1",
            [token_sha256],
        )?;
        Ok(())
    }

    /// Deletes web sessions that expired at or before `now`; returns how many.
    pub fn web_sessions_purge(&mut self, now: i64) -> Result<usize> {
        Ok(self.execute("DELETE FROM web_sessions WHERE expires <= ?1", [now])?)
    }

    /// Adds an API key. `Exists("name")` if the name is taken or `Exists("key")` if the secret
    /// is (revoke the old one first, as the board says).
    pub fn api_key_insert(&mut self, name: &str, key: &str, now: i64) -> Result<ApiKey> {
        if crate::exists(self, "SELECT 1 FROM api_keys WHERE name = ?1", [name])? {
            return Err(Error::Exists("name"));
        }
        if crate::exists(self, "SELECT 1 FROM api_keys WHERE key = ?1", [key])? {
            return Err(Error::Exists("key"));
        }
        self.execute(
            "INSERT INTO api_keys (name, key, created) VALUES (?1, ?2, ?3)",
            params![name, key, now],
        )?;
        Ok(ApiKey {
            name: name.into(),
            key: key.into(),
            github_token: None,
            github_login: None,
            git_name: None,
            git_email: None,
            created: now,
        })
    }

    /// Sets (or, with `None`, clears) the GitHub account of a key: token, login, and the name and
    /// email its commits carry. Returns the key. `NotFound` if there is none called `name`.
    pub fn api_key_update_github(
        &mut self,
        name: &str,
        github_token: Option<&str>,
        github_login: Option<&str>,
        git_name: Option<&str>,
        git_email: Option<&str>,
    ) -> Result<ApiKey> {
        let n = self.execute(
            "UPDATE api_keys SET github_token = ?2, github_login = ?3, git_name = ?4, git_email = ?5 WHERE name = ?1",
            params![name, github_token, github_login, git_name, git_email],
        )?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        api_key_by_name(self, name)?.ok_or(Error::NotFound)
    }

    /// Deletes an API key. `NotFound` if there is none called `name`.
    pub fn api_key_delete(&mut self, name: &str) -> Result<()> {
        if self.execute("DELETE FROM api_keys WHERE name = ?1", [name])? == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Inserts a key with every field (the JSON import); `false` if its name or secret is already
    /// taken (nothing is written then). Empty optional strings become `None`.
    pub(crate) fn api_key_import(&mut self, k: &ApiKey) -> Result<bool> {
        let n = self.execute(
            "INSERT OR IGNORE INTO api_keys (name, key, github_token, github_login, git_name, git_email, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                k.name,
                k.key,
                non_empty(k.github_token.clone()),
                non_empty(k.github_login.clone()),
                non_empty(k.git_name.clone()),
                non_empty(k.git_email.clone()),
                k.created
            ],
        )?;
        Ok(n == 1)
    }

    /// Records the response of an operation so a retry gets the same answer (first write wins).
    /// Call it in the transaction that performed the operation, so both happen or neither.
    pub fn op_record(&mut self, device: DeviceId, op: OpId, resp: &[u8], now: i64) -> Result<()> {
        self.execute(
            "INSERT OR IGNORE INTO ops (device, op_id, resp, ts) VALUES (?1, ?2, ?3, ?4)",
            params![id_blob(device), id_blob(op), resp, now],
        )?;
        Ok(())
    }

    /// Saves a device's keybinding rules. `json` must be valid JSON (`Json` error otherwise).
    pub fn keybindings_set(&mut self, device: DeviceId, json: &str) -> Result<()> {
        serde_json::from_str::<serde::de::IgnoredAny>(json)?;
        self.execute(
            "INSERT INTO keybindings (device, json) VALUES (?1, ?2) ON CONFLICT(device) DO UPDATE SET json = excluded.json",
            params![id_blob(device), json],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::same;

    #[test]
    fn same_compares_whole_values() {
        assert!(same(b"abc", b"abc") && same(b"", b""));
        assert!(!same(b"abc", b"abd") && !same(b"abc", b"ab") && !same(b"", b"a"));
    }
}
