//! `Caps` and `SetCap`: the per-account rotation caps, in `<state>/caps.json`.
//!
//! `{"<cswap number>": {"five_hour": 80, "weekly": 90}}`, a percent of that window at which the box
//! moves off the account (`server/fable-cap.sh` reads the file; an account with no entry, or no
//! value for a window, keeps the box's own `CAP` / `FABLE_CAP`). A value is 50 to 99: below that an
//! account would never be worked, and 100 is no cap. The file is written whole to a sibling and
//! renamed over, so a reader never sees half of it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

use femme_proto::rpc::{AccountCap, Err as WireErr, ErrCode, Resp};
use serde::{Deserialize, Serialize};

use super::req::err;
use crate::config::Config;

/// The lowest and highest cap an account may have, in percent.
const RANGE: std::ops::RangeInclusive<u8> = 50..=99;

/// One account's entry in the file; a window without a value is the box's default.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    five_hour: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekly: Option<u8>,
}

/// The file's content, by account number (a string: a JSON key).
type File = BTreeMap<String, Entry>;

/// Held across a read-modify-write of the file: two `SetCap`s must not lose one another's change.
static WRITING: Mutex<()> = Mutex::new(());

/// Every account that has a cap, by account number.
pub(crate) async fn caps(cfg: &Config) -> Result<Resp, WireErr> {
    let path = cfg.state_path("caps.json");
    let file = blocking(move || read(&path)).await?;
    let mut caps = Vec::with_capacity(file.len());
    for (key, e) in file {
        // An account number as `SetCap` writes it; anything else in the file is somebody's own.
        let Some(account) = key.parse::<u32>().ok().filter(|a| a.to_string() == key) else {
            tracing::warn!(%key, "caps.json: not an account number, ignored");
            continue;
        };
        caps.push(AccountCap {
            account,
            five_hour: e.five_hour,
            weekly: e.weekly,
        });
    }
    caps.sort_by_key(|c| c.account);
    Ok(Resp::Caps(caps))
}

/// Sets `account`'s caps, or with neither window given removes its entry. `Bad` for account 0
/// (cswap counts from 1) or a value outside 50 to 99.
pub(crate) async fn set_cap(
    cfg: &Config,
    account: u32,
    five_hour: Option<u8>,
    weekly: Option<u8>,
) -> Result<Resp, WireErr> {
    if account == 0 {
        return Err(err(ErrCode::Bad, "accounts are numbered from 1"));
    }
    if let Some(v) = five_hour
        .into_iter()
        .chain(weekly)
        .find(|v| !RANGE.contains(v))
    {
        let why = format!(
            "a cap is {} to {} percent, not {v}",
            RANGE.start(),
            RANGE.end()
        );
        return Err(err(ErrCode::Bad, why));
    }
    let path = cfg.state_path("caps.json");
    blocking(move || {
        let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = read(&path)?;
        match (five_hour, weekly) {
            (None, None) => file.remove(&account.to_string()),
            _ => file.insert(account.to_string(), Entry { five_hour, weekly }),
        };
        write(&path, &file).map_err(|e| err(ErrCode::Internal, format!("caps.json: {e}")))
    })
    .await?;
    Ok(Resp::Ok)
}

fn unreadable(why: &str) -> WireErr {
    err(ErrCode::Internal, format!("caps.json is not usable: {why}"))
}

/// The file, empty when there is none. One that does not parse is an error and is left as it is:
/// a write over it would drop whatever the person who edited it meant.
fn read(path: &Path) -> Result<File, WireErr> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| unreadable(&e.to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(File::new()),
        Err(e) => Err(err(ErrCode::Internal, format!("caps.json: {e}"))),
    }
}

/// Writes `file` to `<path>.tmp` and renames it over `path`.
fn write(path: &Path, file: &File) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut out = fs::File::create(&tmp)?;
    let mut text = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
    text.push('\n');
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, WireErr> + Send + 'static,
) -> Result<T, WireErr> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(err(ErrCode::Internal, format!("caps.json: {e}"))))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(dir: &Path) -> Config {
        let mut cfg = Config::from_vars([("HOME".to_string(), dir.display().to_string())]);
        cfg.state_dir = dir.to_path_buf();
        cfg
    }

    fn cap(account: u32, five_hour: Option<u8>, weekly: Option<u8>) -> AccountCap {
        AccountCap {
            account,
            five_hour,
            weekly,
        }
    }

    fn file(cfg: &Config) -> serde_json::Value {
        let text = fs::read_to_string(cfg.state_path("caps.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn caps_start_empty_and_come_back_by_account_number() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        assert_eq!(caps(&cfg).await, Ok(Resp::Caps(vec![])));
        set_cap(&cfg, 10, Some(80), None).await.unwrap();
        set_cap(&cfg, 2, Some(60), Some(90)).await.unwrap();
        // numerically, not as the file's keys sort ("10" before "2")
        let want = vec![cap(2, Some(60), Some(90)), cap(10, Some(80), None)];
        assert_eq!(caps(&cfg).await, Ok(Resp::Caps(want)));
    }

    #[tokio::test]
    async fn the_file_is_the_shape_fable_cap_sh_reads_and_nothing_else_is_left() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        set_cap(&cfg, 2, Some(60), Some(90)).await.unwrap();
        set_cap(&cfg, 3, None, Some(70)).await.unwrap();
        let want = json!({"2": {"five_hour": 60, "weekly": 90}, "3": {"weekly": 70}});
        assert_eq!(file(&cfg), want, "no null for a window without a cap");
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["caps.json"], "the temporary file was renamed away");
    }

    #[tokio::test]
    async fn a_set_replaces_the_entry_and_none_for_both_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        set_cap(&cfg, 2, Some(60), Some(90)).await.unwrap();
        set_cap(&cfg, 4, Some(99), None).await.unwrap();
        set_cap(&cfg, 2, None, Some(70)).await.unwrap();
        assert_eq!(
            file(&cfg),
            json!({"2": {"weekly": 70}, "4": {"five_hour": 99}})
        );
        set_cap(&cfg, 2, None, None).await.unwrap();
        assert_eq!(file(&cfg), json!({"4": {"five_hour": 99}}));
        // clearing what is not there, or in a state folder with no file at all, is fine
        set_cap(&cfg, 2, None, None).await.unwrap();
        set_cap(&cfg, 4, None, None).await.unwrap();
        assert_eq!(file(&cfg), json!({}));
        let other = tempfile::tempdir().unwrap();
        set_cap(&config(other.path()), 1, None, None).await.unwrap();
    }

    #[tokio::test]
    async fn a_cap_outside_fifty_to_ninety_nine_or_for_account_zero_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        for (account, five, weekly) in [
            (1, Some(49), None),
            (1, None, Some(100)),
            (1, Some(60), Some(0)),
            (1, Some(255), None),
            (0, Some(60), None),
            (0, None, None),
        ] {
            let r = set_cap(&cfg, account, five, weekly).await;
            assert_eq!(
                r.unwrap_err().code,
                ErrCode::Bad,
                "{account} {five:?} {weekly:?}"
            );
        }
        assert!(!cfg.state_path("caps.json").exists(), "nothing was written");
        // the ends are fine
        set_cap(&cfg, 1, Some(50), Some(99)).await.unwrap();
    }

    #[tokio::test]
    async fn a_file_that_is_not_usable_is_an_error_and_is_left_as_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        for text in [
            "{\"2\": {\"five_hour\": 8",
            "[1]",
            "{\"2\": {\"weekly\": 300}}",
        ] {
            fs::write(cfg.state_path("caps.json"), text).unwrap();
            assert_eq!(
                caps(&cfg).await.unwrap_err().code,
                ErrCode::Internal,
                "{text}"
            );
            let r = set_cap(&cfg, 3, Some(60), None).await;
            assert_eq!(r.unwrap_err().code, ErrCode::Internal, "{text}");
            assert_eq!(
                fs::read_to_string(cfg.state_path("caps.json")).unwrap(),
                text
            );
        }
    }

    #[tokio::test]
    async fn an_entry_that_is_no_account_is_not_listed_and_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        let text = r#"{"x": {"weekly": 60}, "02": {"weekly": 70}, "5": {"weekly": 80}}"#;
        fs::write(cfg.state_path("caps.json"), text).unwrap();
        let want = vec![cap(5, None, Some(80))];
        assert_eq!(caps(&cfg).await, Ok(Resp::Caps(want)));
        set_cap(&cfg, 3, Some(60), None).await.unwrap();
        let want = json!({"x": {"weekly": 60}, "02": {"weekly": 70}, "3": {"five_hour": 60}, "5": {"weekly": 80}});
        assert_eq!(file(&cfg), want);
    }

    #[tokio::test]
    async fn sets_made_at_once_all_land() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        let sets = (1..=24).map(|a| set_cap(&cfg, a, Some(60), None));
        for r in futures::future::join_all(sets).await {
            r.unwrap();
        }
        let Ok(Resp::Caps(all)) = caps(&cfg).await else {
            panic!("no caps")
        };
        assert_eq!(
            all.iter().map(|c| c.account).collect::<Vec<_>>(),
            (1..=24).collect::<Vec<_>>()
        );
    }
}
