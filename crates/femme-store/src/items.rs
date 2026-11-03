//! Transcript items and the blob store.
//!
//! # Body tiers
//!
//! An [`Item`]'s `body` is JSON text shaped by `femme-agents`. What [`Tx::put_item`] stores
//! depends on its size, and what the read functions return hides the difference:
//!
//! | body size | stored in `items.body` | read back as |
//! |---|---|---|
//! | ≤ 512 B | raw UTF-8 | `body: Some(text)` |
//! | ≤ 4 KiB | zstd level 3 (raw if that is not smaller) | `body: Some(text)` |
//! | > 4 KiB | `NULL`; the text goes to `blobs` | `body: None`, `blob_hash: Some(sha256)` |
//!
//! A reader tells raw from zstd by the zstd magic number: valid UTF-8 can never start with it
//! (`0x28` is ASCII, `0xB5` is a continuation byte), so no marker column is needed.
//!
//! Blobs are content-addressed (`hash` = SHA-256 of the stored text), zstd'd, deduplicated, and
//! capped at 1 MiB: a longer body is cut at a character boundary and ends with `…[truncated]`.
//! The hash covers what is stored, so `sha256(get_blob(h)) == h` always holds. A truncated body
//! is no longer valid JSON; drivers should shorten their text fields before building a body and
//! treat this cap as the backstop.
//!
//! `preview` is at most 512 bytes: the caller's if given (cut at a character boundary), else the
//! first 512 bytes of the body.
//!
//! [`Tx::put_blob_bytes`] stores arbitrary bytes in the same table, for what a client uploads
//! (attachments): never cut, never a transcript body, up to [`BLOB_BYTES_MAX`]. Such a blob is
//! referenced by no item, so retention sweeps it as an orphan: it lives until the next sweep
//! (daemon start, then daily), long enough to be read once by whoever was told its hash.
//!
//! A blob that a re-put replaces is deleted at once (unless another item has the same text); the
//! ones left behind by deletes and purged sessions are swept by
//! [`Store::retain`](crate::Store::retain). Retention may also *evict* the oldest blobs when
//! they pass their size cap: the item then has `blob_hash: None` in the store, but a client
//! that synced earlier still holds `Some(hash)` (an eviction does not bump the item's rev), so
//! [`get_blob`] answering `None` means "the full text is gone, show the preview".

use std::borrow::Cow;

use femme_proto::state::{Entity, Item};
use femme_proto::Sid;
use rusqlite::{params, Connection, Row};

use crate::enums::*;
use crate::{all, blob_id, enum_col, exists, id_blob, one, sha256, wire_size, Error, Result, Tx};

/// Largest body stored raw.
pub const INLINE_RAW_MAX: usize = 512;
/// Largest body kept in the items table (zstd'd above [`INLINE_RAW_MAX`]); anything bigger goes
/// to a blob.
pub const INLINE_MAX: usize = 4096;
/// Largest blob. Longer bodies are truncated and end with [`TRUNCATED`].
pub const BLOB_MAX: usize = 1 << 20;
/// Largest blob [`Tx::put_blob_bytes`] takes (`Error::TooLarge` past it), and the most
/// [`get_blob`] decompresses. Over [`BLOB_MAX`] because an upload is not a transcript body.
pub const BLOB_BYTES_MAX: usize = 8 << 20;
/// Longest `preview`.
pub const PREVIEW_MAX: usize = 512;
/// What a truncated blob ends with.
pub const TRUNCATED: &str = "…[truncated]";

const ZSTD_LEVEL: i32 = 3;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// Upper bound for what an inline body can decompress to (it is ≤ `INLINE_MAX` when written).
const INLINE_DECODE_MAX: usize = 2 * INLINE_MAX;

macro_rules! item_cols {
    () => {
        "session_id, item_id, seq, rev, kind, status, preview, body, blob_hash, ts"
    };
}
pub(crate) use item_cols;

pub(crate) fn item_from_row(r: &Row<'_>) -> Result<Item> {
    let body = match r.get::<_, Option<Vec<u8>>>(7)? {
        Some(b) => Some(decode_body(b)?),
        None => None,
    };
    Ok(Item {
        sid: blob_id(r.get(0)?),
        item_id: r.get(1)?,
        seq: r.get(2)?,
        rev: r.get(3)?,
        kind: enum_col(r, 4, item_kind_parse)?,
        status: enum_col(r, 5, item_status_parse)?,
        preview: r.get(6)?,
        body,
        blob_hash: r.get(8)?,
        ts: r.get(9)?,
    })
}

fn decode_body(stored: Vec<u8>) -> Result<String> {
    let raw = if stored.starts_with(&ZSTD_MAGIC) {
        zstd::bulk::decompress(&stored, INLINE_DECODE_MAX)
            .map_err(|e| Error::Corrupt(format!("item body: {e}")))?
    } else {
        stored
    };
    String::from_utf8(raw).map_err(|_| Error::Corrupt("item body is not UTF-8".into()))
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The body as a blob stores it: unchanged if it fits, else cut and marked.
fn cap_blob(body: &str) -> Cow<'_, str> {
    if body.len() <= BLOB_MAX {
        return Cow::Borrowed(body);
    }
    let keep = truncate_utf8(body, BLOB_MAX - TRUNCATED.len());
    Cow::Owned(format!("{keep}{TRUNCATED}"))
}

/// zstd of `raw` when that is smaller, else `raw` itself (tiny or dense text does not shrink).
fn zstd_if_smaller(raw: &[u8]) -> Result<Vec<u8>> {
    let packed = zstd::bulk::compress(raw, ZSTD_LEVEL)?;
    Ok(if packed.len() < raw.len() {
        packed
    } else {
        raw.to_vec()
    })
}

/// The tombstone id of an item: `<session ULID>/<item_id>`.
pub(crate) fn item_key(sid: Sid, item_id: &str) -> String {
    format!("{sid}/{item_id}")
}

/// One item.
pub fn get_item(conn: &Connection, sid: Sid, item_id: &str) -> Result<Option<Item>> {
    one(
        conn,
        concat!(
            "SELECT ",
            item_cols!(),
            " FROM items WHERE session_id = ?1 AND item_id = ?2"
        ),
        params![id_blob(sid), item_id],
        item_from_row,
    )
}

/// A page of a session's transcript, newest first: the `limit` items with the highest `seq`
/// below `before_seq` (`None` = from the end). The next page is `before_seq = last.seq`.
pub fn page_items(
    conn: &Connection,
    sid: Sid,
    before_seq: Option<u64>,
    limit: usize,
) -> Result<Vec<Item>> {
    let before = before_seq.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    all(
        conn,
        concat!(
            "SELECT ",
            item_cols!(),
            " FROM items WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3"
        ),
        params![id_blob(sid), before, limit],
        item_from_row,
    )
}

/// Items of a session with `rev > after_rev`, oldest change first, stopping before the
/// postcard-encoded total would pass `limit_bytes`. The flag says more are waiting: ask again
/// with the last returned `rev`. Always returns at least one item if there is one, however big,
/// so a reader cannot stall on an oversized row.
///
/// Live items only: deleted ones are not reported here. A client that must learn of deletions
/// follows [`changes_since`](crate::sync::changes_since) with `Scope::Session`, which carries
/// the item tombstones.
pub fn items_since(
    conn: &Connection,
    sid: Sid,
    after_rev: u64,
    limit_bytes: usize,
) -> Result<(Vec<Item>, bool)> {
    let mut stmt = conn.prepare_cached(concat!(
        "SELECT ",
        item_cols!(),
        " FROM items WHERE session_id = ?1 AND rev > ?2 ORDER BY rev"
    ))?;
    let mut rows = stmt.query(params![id_blob(sid), after_rev])?;
    let (mut out, mut used) = (Vec::new(), 0usize);
    while let Some(row) = rows.next()? {
        let item = item_from_row(row)?;
        let size = wire_size(&item)?;
        if !out.is_empty() && used + size > limit_bytes {
            return Ok((out, true));
        }
        used += size;
        out.push(item);
    }
    Ok((out, false))
}

/// The decompressed bytes stored under `hash` (see [`Item::blob_hash`], [`Tx::put_blob_bytes`]);
/// `None` if it was never stored or retention has evicted it.
pub fn get_blob(conn: &Connection, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
    one(
        conn,
        "SELECT len, data FROM blobs WHERE hash = ?1",
        [hash],
        |r| {
            let (len, data): (usize, Vec<u8>) = (r.get(0)?, r.get(1)?);
            zstd::bulk::decompress(&data, len.min(BLOB_BYTES_MAX))
                .map_err(|e| Error::Corrupt(format!("blob: {e}")))
        },
    )
}

/// What `put_item` writes into the row, besides the plain columns.
struct Encoded {
    body: Option<Vec<u8>>,
    blob_hash: Option<[u8; 32]>,
    preview: String,
}

impl Tx<'_> {
    /// Inserts or updates the item `(item.sid, item.item_id)` and returns it as stored.
    ///
    /// - `rev` is a fresh rev; `item.rev` is ignored.
    /// - `seq` is the item's place in the transcript and unique per session. A new item gets
    ///   `item.seq` if non-zero (`Exists("seq")` if another item of the session has it), else one
    ///   past the session's highest. An existing item keeps its `seq`; `item.seq` is ignored.
    /// - The body is tiered as described in the [module docs](crate::items).
    /// - **The row is replaced as a whole**: whatever `item` lacks is gone afterwards. With
    ///   `body: None` the stored body is dropped, and only `preview` and the `blob_hash` given
    ///   in `item` survive. To change one field (a status, say), read the item first
    ///   ([`get_item`] returns the decoded body) and write the modified copy back.
    /// - `NotFound` if the session does not exist.
    pub fn put_item(&mut self, item: &Item) -> Result<Item> {
        let enc = self.encode(item)?;
        let prev: Option<(u64, Option<[u8; 32]>)> = one(
            self,
            "SELECT seq, blob_hash FROM items WHERE session_id = ?1 AND item_id = ?2",
            params![id_blob(item.sid), item.item_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let seq: u64 = match prev {
            Some((seq, _)) => seq,
            None if item.seq != 0 => {
                if exists(
                    self,
                    "SELECT 1 FROM items WHERE session_id = ?1 AND seq = ?2",
                    params![id_blob(item.sid), item.seq],
                )? {
                    return Err(Error::Exists("seq"));
                }
                item.seq
            }
            None => self.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM items WHERE session_id = ?1",
                [id_blob(item.sid)],
                |r| r.get(0),
            )?,
        };
        let replaced = prev.and_then(|(_, hash)| hash);
        let rev = self.next_rev();
        self.prepare_cached(concat!(
            "INSERT INTO items (", item_cols!(), ")
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_id, item_id) DO UPDATE SET
                 rev = excluded.rev, kind = excluded.kind, status = excluded.status, preview = excluded.preview,
                 body = excluded.body, blob_hash = excluded.blob_hash, ts = excluded.ts"
        ))?
        .execute(params![
            id_blob(item.sid),
            item.item_id,
            seq,
            rev,
            item_kind_str(item.kind),
            item_status_str(item.status),
            enc.preview,
            enc.body,
            enc.blob_hash,
            item.ts,
        ])?;
        self.clear_tombstone(Entity::Item, &item_key(item.sid, &item.item_id))?; // ids can come back
        if let Some(old) = replaced.filter(|h| Some(*h) != enc.blob_hash) {
            // A streaming body is re-put with every delta; its earlier versions must not pile up
            // as blobs until the daily sweep.
            self.execute(
                "DELETE FROM blobs WHERE hash = ?1 AND NOT EXISTS (SELECT 1 FROM items WHERE blob_hash = ?1)",
                [old],
            )?;
        }
        Ok(Item {
            sid: item.sid,
            item_id: item.item_id.clone(),
            seq,
            rev,
            kind: item.kind,
            status: item.status,
            preview: enc.preview,
            body: if enc.body.is_some() {
                item.body.clone()
            } else {
                None
            },
            blob_hash: enc.blob_hash,
            ts: item.ts,
        })
    }

    /// Deletes one item and leaves a tombstone. `NotFound` if it does not exist.
    pub fn delete_item(&mut self, sid: Sid, item_id: &str) -> Result<()> {
        let n = self.execute(
            "DELETE FROM items WHERE session_id = ?1 AND item_id = ?2",
            params![id_blob(sid), item_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.tombstone(Entity::Item, &item_key(sid, item_id))?;
        Ok(())
    }

    /// Deletes every item of a session, with a tombstone per item (so a client that has the
    /// session open drops them). Returns how many. The blobs they referenced stay until the next
    /// retention sweep. ponytail: one tombstone per item makes this O(items); if clearing big
    /// transcripts becomes routine, replace it with a single "items cleared at rev R" marker.
    pub fn delete_items(&mut self, sid: Sid) -> Result<usize> {
        let ids = all(
            self,
            "SELECT item_id FROM items WHERE session_id = ?1 ORDER BY seq",
            [id_blob(sid)],
            |r| Ok(r.get::<_, String>(0)?),
        )?;
        self.execute("DELETE FROM items WHERE session_id = ?1", [id_blob(sid)])?;
        for id in &ids {
            self.tombstone(Entity::Item, &item_key(sid, id))?;
        }
        Ok(ids.len())
    }

    fn encode(&mut self, item: &Item) -> Result<Encoded> {
        let preview_of = |body: &str| {
            let text = if item.preview.is_empty() {
                body
            } else {
                item.preview.as_str()
            };
            truncate_utf8(text, PREVIEW_MAX).to_owned()
        };
        let Some(body) = item.body.as_deref() else {
            let preview = truncate_utf8(&item.preview, PREVIEW_MAX).to_owned();
            return Ok(Encoded {
                body: None,
                blob_hash: item.blob_hash,
                preview,
            });
        };
        let preview = preview_of(body);
        if body.len() <= INLINE_RAW_MAX {
            return Ok(Encoded {
                body: Some(body.as_bytes().to_vec()),
                blob_hash: None,
                preview,
            });
        }
        if body.len() <= INLINE_MAX {
            return Ok(Encoded {
                body: Some(zstd_if_smaller(body.as_bytes())?),
                blob_hash: None,
                preview,
            });
        }
        let hash = self.put_blob(body)?;
        Ok(Encoded {
            body: None,
            blob_hash: Some(hash),
            preview,
        })
    }

    /// Stores (or refreshes) a transcript body as a blob and returns its hash: cut to
    /// [`BLOB_MAX`] first (see the [module docs](crate::items)).
    fn put_blob(&mut self, body: &str) -> Result<[u8; 32]> {
        self.put_blob_raw(cap_blob(body).as_bytes())
    }

    /// Stores (or refreshes) `data` as a blob, whole, and returns its SHA-256, which
    /// [`get_blob`] answers to. For what a client uploads. The bytes are kept as they are (no
    /// UTF-8 needed, nothing cut); `Error::TooLarge` past [`BLOB_BYTES_MAX`]. Idempotent: the
    /// same bytes are one row, and putting them again only refreshes it. No item refers to the
    /// blob, so retention sweeps it as an orphan (see the [module docs](crate::items)).
    pub fn put_blob_bytes(&mut self, data: &[u8]) -> Result<[u8; 32]> {
        if data.len() > BLOB_BYTES_MAX {
            return Err(Error::TooLarge {
                max: BLOB_BYTES_MAX,
            });
        }
        self.put_blob_raw(data)
    }

    /// An identical body is stored once; its `ts` moves forward so retention evicts the least
    /// recently *written* blob first.
    fn put_blob_raw(&mut self, body: &[u8]) -> Result<[u8; 32]> {
        let hash = sha256(body);
        let now = self.now();
        let known = self
            .prepare_cached("UPDATE blobs SET ts = ?2 WHERE hash = ?1")?
            .execute(params![hash, now])?;
        if known == 0 {
            let data = zstd::bulk::compress(body, ZSTD_LEVEL)?;
            self.prepare_cached("INSERT INTO blobs (hash, len, data, ts) VALUES (?1, ?2, ?3, ?4)")?
                .execute(params![hash, body.len(), data, now])?;
        }
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("abc", 2), "ab");
        // "é" is 2 bytes: cutting at 1 must back off to 0, cutting at 3 keeps "aé".
        assert_eq!(truncate_utf8("éa", 1), "");
        assert_eq!(truncate_utf8("aéb", 3), "aé");
        assert_eq!(truncate_utf8("aéb", 2), "a");
        // a 4-byte emoji
        assert_eq!(truncate_utf8("😀x", 3), "");
        assert_eq!(truncate_utf8("😀x", 4), "😀");
    }

    #[test]
    fn cap_blob_marks_what_it_cuts() {
        let small = "x".repeat(BLOB_MAX);
        assert!(
            matches!(cap_blob(&small), Cow::Borrowed(_)),
            "exactly the cap is not cut"
        );
        let big = "é".repeat(BLOB_MAX); // 2 MiB of 2-byte chars
        let capped = cap_blob(&big);
        assert!(capped.len() <= BLOB_MAX);
        assert!(capped.ends_with(TRUNCATED));
        assert!(
            capped.len() > BLOB_MAX - 8,
            "keeps almost everything it may"
        );
    }

    #[test]
    fn zstd_is_used_only_when_it_is_smaller() {
        let squishy = b"abcabcabc".repeat(100);
        let packed = zstd_if_smaller(&squishy).unwrap();
        assert!(packed.starts_with(&ZSTD_MAGIC) && packed.len() < squishy.len());
        // all 256 byte values in LCG order: no zstd frame can be smaller than this
        let mut x = 1u64;
        let dense: Vec<u8> = (0..600)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (x >> 56) as u8
            })
            .collect();
        assert_eq!(zstd_if_smaller(&dense).unwrap(), dense);
        assert_eq!(zstd_if_smaller(b"").unwrap(), b"");
    }

    #[test]
    fn zstd_magic_cannot_start_a_utf8_string() {
        assert!(String::from_utf8(ZSTD_MAGIC.to_vec()).is_err());
        assert!(String::from_utf8(ZSTD_MAGIC[..2].to_vec()).is_err());
    }
}
