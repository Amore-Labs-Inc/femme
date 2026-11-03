//! What the accounts card shows, as plain data: the web app's JSON (cswap's account list, the box's
//! AWS identity, the API keys, `/api/meta`) turned into view-models by pure functions, so the
//! conversion is tested without a window. Field names are the ones the board's own page reads
//! (`assets/index.html`, `showAccounts` and `showKeys`); cswap's schema is in its `json_output.py`.
//!
//! Nothing here panics on odd input: an entry the page could not act on is dropped, a field that is
//! missing or of another type is the same as one that is absent.

use femme_proto::rpc::AccountCap;
use serde_json::Value;

use super::clock;

/// The lowest per-account rotation cap, in percent.
pub const CAP_MIN: u8 = 50;
/// The highest per-account rotation cap, in percent.
pub const CAP_MAX: u8 = 99;
/// Usage older than this many seconds gets a note saying how old it is.
const STALE_AFTER: i64 = 15 * 60;

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// A JSON object, or `None` for null, absent and anything else.
fn object<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|v| v.is_object())
}

/// JavaScript's `Math.round` for a percentage: halves go up.
pub fn percent(pct: f64) -> i64 {
    (pct + 0.5).floor() as i64
}

/// One usage window: the 5-hour or the 7-day one, or a model's own weekly one.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    /// Share of the window used, in percent (may pass 100).
    pub pct: f64,
    /// When the window resets, seconds since the epoch.
    pub resets_at: Option<i64>,
    /// cswap's own countdown, for a window whose reset time is missing or unreadable.
    pub countdown: Option<String>,
}

impl Usage {
    fn from_json(w: &Value) -> Option<Self> {
        Some(Self {
            pct: w.get("pct")?.as_f64()?,
            resets_at: text(w, "resetsAt").and_then(clock::parse_iso),
            countdown: text(w, "countdown").map(str::to_owned),
        })
    }

    /// How much of the bar is full, 0.0 to 1.0.
    pub fn fill(&self) -> f32 {
        (self.pct / 100.0).clamp(0.0, 1.0) as f32
    }

    /// Whether the usage has reached `cap` percent.
    pub fn reaches(&self, cap: Option<u8>) -> bool {
        cap.is_some_and(|c| self.pct >= f64::from(c))
    }

    /// The text beside the bar, `71% · 2d 5h`: the percentage and the time left to the reset,
    /// counted from `now`.
    pub fn text(&self, now: i64) -> String {
        let left = self
            .resets_at
            .map(|at| clock::countdown(at - now))
            .or_else(|| self.countdown.clone());
        match left {
            Some(left) => format!("{}% · {left}", percent(self.pct)),
            None => format!("{}%", percent(self.pct)),
        }
    }
}

/// A model's own weekly limit (`Fable`), which cswap lists next to the 7-day window.
#[derive(Clone, Debug, PartialEq)]
pub struct Scoped {
    /// The model's name as cswap gives it.
    pub name: String,
    /// Its usage.
    pub usage: Usage,
}

impl Scoped {
    /// The tag on the account's title line, `fable 71% · November 5 at 02:59`: the reset time in the
    /// zone `offset` seconds east of UTC.
    pub fn tag(&self, offset: i64) -> String {
        let name = self.name.to_lowercase();
        let pct = percent(self.usage.pct);
        match self.usage.resets_at {
            Some(at) => format!("{name} {pct}% · {}", clock::long_when(at, offset)),
            None => format!("{name} {pct}%"),
        }
    }
}

/// One Claude account of the box's cswap.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// cswap's account number: what `switch` and the caps name it by.
    pub number: u32,
    /// The alias, else the email.
    pub title: String,
    /// The box runs on this account.
    pub active: bool,
    /// Held out of rotation (`cswap disable`).
    pub disabled: bool,
    /// The 5-hour window.
    pub five_hour: Option<Usage>,
    /// The 7-day window.
    pub seven_day: Option<Usage>,
    /// The per-model weekly windows.
    pub scoped: Vec<Scoped>,
    /// Why there is no (fresh) usage, in words.
    pub reason: Option<String>,
    /// The windows are the last good ones, not the latest reading.
    pub stale: bool,
    /// When the windows were read, seconds since the epoch.
    pub fetched_at: Option<i64>,
}

/// What cswap's `usageStatus` (and `usageError`) say about a missing reading.
fn reason(status: &str, error: Option<&str>) -> String {
    match status {
        "token_expired" => "token expired".into(),
        "relogin_required" => "re-login needed".into(),
        "api_key" => "api key: no usage quota".into(),
        "keychain_unavailable" => "keychain unavailable".into(),
        "foreign_credential" => "foreign credential".into(),
        "no_credentials" => "no credentials".into(),
        _ => error.map_or_else(
            || "usage unknown".into(),
            |e| format!("usage unavailable ({e})"),
        ),
    }
}

impl Account {
    fn from_json(a: &Value) -> Option<Self> {
        let number = u32::try_from(a.get("number")?.as_u64()?).ok()?;
        let title = text(a, "alias")
            .or_else(|| text(a, "email"))
            .map_or_else(|| format!("account {number}"), str::to_owned);
        let (usage, stale) = match object(a, "usage") {
            Some(u) => (Some(u), false),
            None => (object(a, "lastGoodUsage"), true),
        };
        let window = |key: &str| usage.and_then(|u| u.get(key)).and_then(Usage::from_json);
        let fetched = text(
            a,
            if stale {
                "lastGoodFetchedAt"
            } else {
                "usageFetchedAt"
            },
        );
        let missing = object(a, "usage").is_none();
        Some(Self {
            number,
            title,
            active: a.get("active").and_then(Value::as_bool).unwrap_or(false),
            disabled: a.get("disabled").and_then(Value::as_bool).unwrap_or(false),
            five_hour: window("fiveHour"),
            seven_day: window("sevenDay"),
            scoped: usage
                .and_then(|u| u.get("scoped")?.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| {
                    Some(Scoped {
                        name: text(s, "name")?.to_owned(),
                        usage: Usage::from_json(s)?,
                    })
                })
                .collect(),
            reason: missing.then(|| {
                reason(
                    text(a, "usageStatus").unwrap_or("unavailable"),
                    text(a, "usageError"),
                )
            }),
            stale,
            fetched_at: fetched.and_then(clock::parse_iso),
        })
    }

    /// The windows to draw, labelled as the page labels them.
    pub fn bars(&self) -> Vec<(&'static str, &Usage)> {
        [("5h", &self.five_hour), ("7d", &self.seven_day)]
            .into_iter()
            .filter_map(|(label, w)| Some((label, w.as_ref()?)))
            .collect()
    }

    /// The small line under the bars, when there is something to say: why there is no reading, or
    /// how old the one shown is.
    pub fn note(&self, now: i64) -> Option<String> {
        let age = self.fetched_at.map(|at| clock::ago(now - at));
        if self.bars().is_empty() {
            return Some(
                self.reason
                    .clone()
                    .unwrap_or_else(|| "usage unknown".into()),
            );
        }
        if self.stale {
            let since = age.map_or_else(String::new, |a| format!("last read {a}"));
            return Some(match (&self.reason, since.is_empty()) {
                (Some(r), false) => format!("{since} · {r}"),
                (Some(r), true) => r.clone(),
                (None, _) => since,
            });
        }
        self.fetched_at
            .filter(|at| now - at > STALE_AFTER)
            .map(|at| format!("usage read {}", clock::ago(now - at)))
    }
}

/// The accounts list as the page gets it.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountList {
    /// The box has not read cswap yet.
    Pending,
    /// cswap failed; its words.
    Error(String),
    /// The accounts.
    Rows(Vec<Account>),
}

/// Who the box's AWS CLI runs as.
#[derive(Clone, Debug, PartialEq)]
pub struct Aws {
    /// `123456789012 · assumed-role/femme-box/i-0example`, or why there is none.
    pub line: String,
    /// The full ARN, or the CLI's error, for a second line.
    pub detail: Option<String>,
    /// The CLI has an identity (the green bar).
    pub active: bool,
}

impl Aws {
    fn from_json(a: Option<&Value>) -> Self {
        let Some(a) = a.filter(|a| a.is_object()) else {
            return Self {
                line: "checking".into(),
                detail: None,
                active: false,
            };
        };
        match text(a, "Arn") {
            Some(arn) => Self {
                line: format!(
                    "{} · {}",
                    text(a, "Account").unwrap_or("?"),
                    arn.rsplit(':').next().unwrap_or(arn)
                ),
                detail: Some(arn.to_owned()),
                active: true,
            },
            None => Self {
                line: "no AWS access".into(),
                detail: text(a, "error").map(str::to_owned),
                active: false,
            },
        }
    }
}

/// `GET /api/accounts`.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountsDoc {
    /// cswap's accounts.
    pub list: AccountList,
    /// The AWS identity.
    pub aws: Aws,
    /// The link of the Claude sign-in in progress, if one was started.
    pub login: Option<String>,
}

/// Parses the body of `GET /api/accounts` (and of every account action, which answers the same).
pub fn parse_accounts(body: &str) -> Result<AccountsDoc, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    if !v.is_object() {
        return Err("not an object".into());
    }
    let list = match v.get("cswap") {
        None | Some(Value::Null) => AccountList::Pending,
        Some(c) => match c.get("error") {
            Some(e) => AccountList::Error(
                e.as_str()
                    .or_else(|| e.get("message").and_then(Value::as_str))
                    .unwrap_or("cswap failed")
                    .to_owned(),
            ),
            None => AccountList::Rows(
                c.get("accounts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Account::from_json)
                    .collect(),
            ),
        },
    };
    Ok(AccountsDoc {
        list,
        aws: Aws::from_json(v.get("aws")),
        login: v
            .get("login")
            .and_then(|l| text(l, "claude"))
            .map(str::to_owned),
    })
}

/// A GitHub sign-in in progress for a key: the code to enter and where.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    /// The one-time code.
    pub code: String,
    /// Where it is entered.
    pub url: String,
}

/// One API key, without its secret.
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    /// The key's name.
    pub name: String,
    /// The last characters of the key, to tell keys apart.
    pub hint: String,
    /// Whose GitHub account its clones push as: `the box`, `its own token` or `Name (login)`.
    pub account: String,
    /// When it was made, seconds since the epoch.
    pub created: Option<i64>,
    /// A GitHub sign-in in progress.
    pub github: Option<Device>,
    /// How the last GitHub sign-in failed.
    pub error: Option<String>,
}

impl Key {
    fn from_json(k: &Value) -> Option<Self> {
        Some(Self {
            name: text(k, "name")?.to_owned(),
            hint: text(k, "hint").unwrap_or_default().to_owned(),
            account: text(k, "account").unwrap_or("the box").to_owned(),
            created: k.get("created").and_then(Value::as_i64),
            github: k.get("github").filter(|g| g.is_object()).and_then(|g| {
                Some(Device {
                    code: text(g, "code")?.to_owned(),
                    url: text(g, "url")
                        .unwrap_or("https://github.com/login/device")
                        .to_owned(),
                })
            }),
            error: text(k, "error").map(str::to_owned),
        })
    }

    /// The key's line, `instinct · …a1b2 · pushes as the box · 3d ago`, with its age counted from
    /// `now`.
    pub fn line(&self, now: i64) -> String {
        let mut line = format!(
            "{} · …{} · pushes as {}",
            self.name, self.hint, self.account
        );
        if let Some(at) = self.created {
            line.push_str(&format!(" · {}", clock::ago(now - at)));
        }
        line
    }
}

fn keys_from(v: &Value) -> Vec<Key> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Key::from_json)
        .collect()
}

/// Parses the body of `GET /api/keys` (and of revoke and the GitHub sign-in calls): a list of
/// keys, or an object holding one as `keys`, as the page reads it (`d.keys || d`).
pub fn parse_keys(body: &str) -> Result<Vec<Key>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    let list = v.get("keys").unwrap_or(&v);
    if !list.is_array() {
        return Err("not a list".into());
    }
    Ok(keys_from(list))
}

/// The answer to `POST /api/keys`: the key, shown once, and the list as it is now.
#[derive(Clone, Debug, PartialEq)]
pub struct NewKey {
    /// The name it was made under.
    pub name: String,
    /// The secret, which the box never shows again.
    pub key: String,
    /// Every key now.
    pub keys: Vec<Key>,
}

/// Parses the body of `POST /api/keys`.
pub fn parse_new_key(body: &str) -> Result<NewKey, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    Ok(NewKey {
        name: text(&v, "name").ok_or("no name in the answer")?.to_owned(),
        key: text(&v, "key").ok_or("no key in the answer")?.to_owned(),
        keys: v.get("keys").map(keys_from).unwrap_or_default(),
    })
}

/// What `/api/meta` says that the card uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// The box's default rotation cap (`CAP`), in percent.
    pub cap: Option<u8>,
    /// The scheduler's word, `working (31% used, pause at 60%)`.
    pub state: Option<String>,
}

/// Parses the body of `GET /api/meta`; what is not there is `None`.
pub fn parse_meta(body: &str) -> Meta {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    Meta {
        cap: v
            .get("cap")
            .and_then(Value::as_u64)
            .and_then(|c| u8::try_from(c).ok()),
        state: text(&v, "state").map(str::to_owned),
    }
}

/// One account's caps as the box has them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// Percent of the 5-hour window at which the box moves off the account; `None`: the default.
    pub five_hour: Option<u8>,
    /// The same for the 7-day window.
    pub weekly: Option<u8>,
}

impl From<&AccountCap> for Caps {
    fn from(c: &AccountCap) -> Self {
        Self {
            five_hour: c.five_hour,
            weekly: c.weekly,
        }
    }
}

/// Reads what a cap box holds: empty is the default (`None`), else a whole percent from
/// [`CAP_MIN`] to [`CAP_MAX`].
pub fn parse_cap(text: &str) -> Result<Option<u8>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<u8>()
        .ok()
        .filter(|n| (CAP_MIN..=CAP_MAX).contains(n))
        .map(Some)
        .ok_or_else(|| format!("use {CAP_MIN}–{CAP_MAX}"))
}

/// The error text of a failed board call: the `error` of its JSON body, else the body's own words,
/// else the status.
pub fn error_text(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let said = v.get("error").and_then(|e| {
        e.as_str()
            .or_else(|| e.get("message").and_then(Value::as_str))
    });
    let said = said.map_or_else(|| body.trim(), str::trim);
    if said.is_empty() {
        return format!("HTTP {status}");
    }
    match said.char_indices().nth(240) {
        Some((end, _)) => format!("{}…", &said[..end]),
        None => said.to_owned(),
    }
}

/// A failed request, in words for the card: a daemon that does not serve the accounts requests yet
/// (`Unsupported`) is said to be too old, plainly; anything else is the daemon's own message.
pub fn explain(err: &str) -> String {
    if err.starts_with("Unsupported") {
        format!("daemon too old for accounts ({err})")
    } else {
        err.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture file as the debug aid reads it: `accounts`, `keys`, `caps`, `meta`.
    fn fixture() -> Value {
        serde_json::from_str(include_str!("fixture.json")).unwrap()
    }

    fn body(key: &str) -> String {
        fixture()[key].to_string()
    }

    /// 2026-11-03 10:00:00 UTC, a little after the fixture's readings.
    fn now() -> i64 {
        clock::parse_iso("2026-11-03T10:00:00Z").unwrap()
    }

    fn rows(doc: AccountsDoc) -> Vec<Account> {
        match doc.list {
            AccountList::Rows(r) => r,
            other => panic!("not rows: {other:?}"),
        }
    }

    #[test]
    fn the_fixture_has_four_accounts_with_the_fields_the_page_reads() {
        let rows = rows(parse_accounts(&body("accounts")).unwrap());
        assert_eq!(
            rows.iter().map(|a| a.number).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let ada = &rows[0];
        assert_eq!(ada.title, "ada@example.test");
        assert!(ada.active && !ada.disabled && !ada.stale);
        assert_eq!(ada.five_hour.as_ref().unwrap().pct, 31.0);
        assert_eq!(ada.seven_day.as_ref().unwrap().pct, 48.0);
        assert_eq!(ada.scoped.len(), 1);
        assert_eq!(ada.scoped[0].name, "Fable");
        assert_eq!(ada.reason, None);
        assert_eq!(ada.fetched_at, clock::parse_iso("2026-11-03T09:59:30Z"));
        // The alias wins over the email.
        assert_eq!(rows[1].title, "grace (lab)");
        assert!(rows[2].disabled && !rows[2].active);
    }

    #[test]
    fn bars_and_tags_read_like_the_page() {
        let rows = rows(parse_accounts(&body("accounts")).unwrap());
        let ada = &rows[0];
        let bars = ada.bars();
        assert_eq!(bars.len(), 2);
        assert_eq!((bars[0].0, bars[1].0), ("5h", "7d"));
        // The countdown is counted from now, not taken from what cswap said at its last read.
        assert_eq!(bars[0].1.text(now()), "31% · 3h 0m");
        assert_eq!(bars[1].1.text(now()), "48% · 4d 20h");
        assert_eq!(ada.scoped[0].tag(0), "fable 71% · November 5 at 02:59");
        assert_eq!(
            ada.scoped[0].tag(3 * 3600),
            "fable 71% · November 5 at 05:59"
        );
        assert!((bars[0].1.fill() - 0.31).abs() < 1e-6);
        // Past the reset: nothing left to wait for.
        assert_eq!(bars[0].1.text(now() + 4 * 3600), "31% · 0m");
    }

    #[test]
    fn a_window_without_a_reset_time_keeps_cswaps_countdown() {
        let w = Usage::from_json(&serde_json::json!({"pct": 49.5, "countdown": "2h 1m"})).unwrap();
        assert_eq!(w.resets_at, None);
        assert_eq!(w.text(now()), "50% · 2h 1m");
        let bare = Usage::from_json(&serde_json::json!({"pct": 7})).unwrap();
        assert_eq!(bare.text(now()), "7%");
        assert_eq!(Usage::from_json(&serde_json::json!({"pct": "7"})), None);
        let over = Usage::from_json(&serde_json::json!({"pct": 130.0})).unwrap();
        assert_eq!(over.fill(), 1.0);
        let scoped = Scoped {
            name: "Fable".into(),
            usage: bare,
        };
        assert_eq!(scoped.tag(0), "fable 7%", "no reset time, no separator");
    }

    #[test]
    fn percent_rounds_halves_up_like_javascript() {
        assert_eq!(percent(0.4), 0);
        assert_eq!(percent(0.5), 1);
        assert_eq!(percent(71.0), 71);
        assert_eq!(percent(86.49), 86);
        assert_eq!(percent(99.5), 100);
    }

    #[test]
    fn caps_are_reached_at_or_past_the_percentage() {
        let w = Usage::from_json(&serde_json::json!({"pct": 60.0})).unwrap();
        assert!(w.reaches(Some(60)));
        assert!(w.reaches(Some(50)));
        assert!(!w.reaches(Some(61)));
        assert!(!w.reaches(None));
    }

    #[test]
    fn a_missing_reading_says_why_and_a_stale_one_says_how_old() {
        let rows = rows(parse_accounts(&body("accounts")).unwrap());
        // Fresh bars say nothing.
        assert_eq!(rows[0].note(now()), None);
        // Margaret's usage call failed (rate limited): the last good bars, flagged.
        let margaret = &rows[3];
        assert!(margaret.stale);
        assert_eq!(margaret.bars().len(), 2);
        assert_eq!(margaret.bars()[0].1.pct, 5.0);
        assert_eq!(
            margaret.note(now()).as_deref(),
            Some("last read 3h ago · usage unavailable (http-429)")
        );
        // Fresh bars that have aged past the page's refresh say so.
        assert_eq!(
            rows[0].note(now() + 3600).as_deref(),
            Some("usage read 1h ago")
        );
    }

    #[test]
    fn cswap_statuses_become_words() {
        let one = |extra: Value| {
            let mut a = serde_json::json!({"number": 7, "email": "x@example.test", "usage": null});
            a.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Account::from_json(&a).unwrap()
        };
        let expired = one(serde_json::json!({"usageStatus": "token_expired"}));
        assert_eq!(expired.note(0).as_deref(), Some("token expired"));
        assert!(expired.bars().is_empty());
        for (status, words) in [
            ("relogin_required", "re-login needed"),
            ("api_key", "api key: no usage quota"),
            ("keychain_unavailable", "keychain unavailable"),
            ("foreign_credential", "foreign credential"),
            ("no_credentials", "no credentials"),
            ("unavailable", "usage unknown"),
        ] {
            let a = one(serde_json::json!({"usageStatus": status}));
            assert_eq!(a.note(0).as_deref(), Some(words), "{status}");
        }
        let failed =
            one(serde_json::json!({"usageStatus": "unavailable", "usageError": "timeout"}));
        assert_eq!(
            failed.note(0).as_deref(),
            Some("usage unavailable (timeout)")
        );
        // No status at all: the same as unknown.
        let bare = one(serde_json::json!({}));
        assert_eq!(bare.note(0).as_deref(), Some("usage unknown"));
    }

    #[test]
    fn an_entry_without_a_number_cannot_be_acted_on_and_is_dropped() {
        let doc = parse_accounts(
            r#"{"cswap": {"accounts": [
                {"email": "no-number@example.test"},
                {"number": "2", "email": "string-number@example.test"},
                {"number": 99999999999, "email": "huge@example.test"},
                {"number": 3},
                7, null, "x"
            ]}}"#,
        )
        .unwrap();
        let rows = rows(doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 3);
        assert_eq!(rows[0].title, "account 3");
    }

    #[test]
    fn the_list_can_be_pending_failed_or_empty() {
        assert_eq!(
            parse_accounts(r#"{"login": {}}"#).unwrap().list,
            AccountList::Pending
        );
        assert_eq!(
            parse_accounts(r#"{"cswap": null, "aws": null}"#)
                .unwrap()
                .list,
            AccountList::Pending
        );
        let failed = parse_accounts(r#"{"cswap": {"error": "cswap: no config"}}"#).unwrap();
        assert_eq!(failed.list, AccountList::Error("cswap: no config".into()));
        let structured = parse_accounts(r#"{"cswap": {"error": {"message": "locked"}}}"#).unwrap();
        assert_eq!(structured.list, AccountList::Error("locked".into()));
        let empty = parse_accounts(r#"{"cswap": {"accounts": []}}"#).unwrap();
        assert_eq!(empty.list, AccountList::Rows(vec![]));
        assert!(parse_accounts("nope").is_err());
        assert!(parse_accounts("[1]").is_err());
    }

    #[test]
    fn aws_is_an_account_and_a_role_or_why_not() {
        let doc = parse_accounts(&body("accounts")).unwrap();
        assert_eq!(
            doc.aws.line,
            "123456789012 · assumed-role/femme-box/i-0example"
        );
        assert!(doc.aws.active);
        assert_eq!(
            doc.aws.detail.as_deref(),
            Some("arn:aws:sts::123456789012:assumed-role/femme-box/i-0example")
        );
        let none = parse_accounts(r#"{"aws": {"error": "Unable to locate credentials"}}"#).unwrap();
        assert_eq!(none.aws.line, "no AWS access");
        assert!(!none.aws.active);
        assert_eq!(
            none.aws.detail.as_deref(),
            Some("Unable to locate credentials")
        );
        let unread = parse_accounts("{}").unwrap();
        assert_eq!(
            (unread.aws.line.as_str(), unread.aws.active),
            ("checking", false)
        );
    }

    #[test]
    fn a_sign_in_in_progress_is_its_link() {
        let doc = parse_accounts(
            r#"{"cswap": {"accounts": []}, "login": {"claude": "https://claude.example.test/authorize?code=true"}}"#,
        )
        .unwrap();
        assert_eq!(
            doc.login.as_deref(),
            Some("https://claude.example.test/authorize?code=true")
        );
        assert_eq!(parse_accounts(&body("accounts")).unwrap().login, None);
    }

    #[test]
    fn keys_read_like_the_pages_list() {
        let keys = parse_keys(&body("keys")).unwrap();
        assert_eq!(keys.len(), 4);
        assert_eq!(
            keys[0].line(now()),
            "instinct · …a1b2 · pushes as the box · 2d ago"
        );
        assert_eq!(keys[1].account, "Ada Lovelace (ada-l)");
        assert_eq!(
            keys[2].github,
            Some(Device {
                code: "ABCD-1234".into(),
                url: "https://github.com/login/device".into()
            })
        );
        assert_eq!(
            keys[3].error.as_deref(),
            Some("GitHub did not accept that token")
        );
        assert_eq!(keys[0].github, None);
        assert!(parse_keys("{}").is_err());
        assert!(parse_keys("").is_err());
        assert_eq!(parse_keys("[]").unwrap(), vec![]);
        // An object holding the list is read as well, as the page does.
        let wrapped = format!(r#"{{"keys": {}}}"#, body("keys"));
        assert_eq!(parse_keys(&wrapped).unwrap(), keys);
        assert!(parse_keys(r#"{"keys": 3}"#).is_err());
    }

    #[test]
    fn a_new_key_comes_with_its_secret_and_the_list() {
        let made = parse_new_key(
            r#"{"name": "ci", "key": "0123456789abcdef", "keys": [{"name": "ci", "hint": "cdef", "created": 5, "account": "the box"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (made.name.as_str(), made.key.as_str()),
            ("ci", "0123456789abcdef")
        );
        assert_eq!(made.keys.len(), 1);
        assert!(parse_new_key(r#"{"name": "ci"}"#).is_err());
        assert!(parse_new_key("[]").is_err());
    }

    #[test]
    fn meta_gives_the_default_cap() {
        let meta = parse_meta(&body("meta"));
        assert_eq!(meta.cap, Some(60));
        assert_eq!(
            meta.state.as_deref(),
            Some("working (31% used, pause at 60%)")
        );
        assert_eq!(parse_meta("{}"), Meta::default());
        assert_eq!(parse_meta("not json"), Meta::default());
        assert_eq!(parse_meta(r#"{"cap": 4000}"#).cap, None);
    }

    #[test]
    fn the_fixtures_caps_are_the_wire_type() {
        let caps: Vec<AccountCap> = serde_json::from_value(fixture()["caps"].clone()).unwrap();
        assert_eq!(caps.len(), 2);
        assert_eq!(
            Caps::from(&caps[0]),
            Caps {
                five_hour: Some(80),
                weekly: None
            }
        );
    }

    #[test]
    fn a_cap_box_holds_nothing_or_fifty_to_ninety_nine() {
        assert_eq!(parse_cap(""), Ok(None));
        assert_eq!(parse_cap("  "), Ok(None));
        assert_eq!(parse_cap("50"), Ok(Some(50)));
        assert_eq!(parse_cap(" 99 "), Ok(Some(99)));
        for bad in [
            "5",
            "49",
            "100",
            "-60",
            "6o",
            "60%",
            "6 0",
            "256",
            "999999999999",
        ] {
            assert_eq!(parse_cap(bad), Err("use 50–99".to_string()), "{bad:?}");
        }
    }

    #[test]
    fn a_failed_call_says_what_the_board_said() {
        assert_eq!(
            error_text(400, r#"{"error": "account must be a number"}"#),
            "account must be a number"
        );
        assert_eq!(
            error_text(502, r#"{"error": {"message": "locked"}}"#),
            "locked"
        );
        assert_eq!(error_text(503, "board is down\n"), "board is down");
        assert_eq!(error_text(500, ""), "HTTP 500");
        assert_eq!(error_text(500, r#"{"error": ""}"#), "HTTP 500");
        let long = "é".repeat(500);
        let cut = error_text(500, &long);
        assert_eq!(cut.chars().count(), 241);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn an_old_daemon_is_called_old() {
        assert_eq!(
            explain("Unsupported: not served yet"),
            "daemon too old for accounts (Unsupported: not served yet)"
        );
        assert_eq!(explain("Timeout"), "Timeout");
        assert_eq!(explain("the client was stopped"), "the client was stopped");
    }
}
