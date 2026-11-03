//! Where the card's answers come from: the daemon (`Net::run`), or, for a screenshot or a try-out
//! without a daemon, a JSON file played back by a tiny fake board (the debug aid below).
//!
//! # `FEMME_ACCOUNTS_FIXTURE=<path>`
//!
//! With this variable set the overlay renders that JSON file instead of calling the daemon:
//! `{"accounts": <what GET /api/accounts answers>, "keys": <GET /api/keys>, "caps": [{"account": 1,
//! "five_hour": 80, "weekly": null}], "meta": <GET /api/meta>}`; every part but `accounts` may be
//! left out. `desktop/src/accounts/fixture.json` is the file the tests use. The fake answers the
//! board's account and key routes against the in-memory copy (switching, signing in, keys, GitHub
//! codes) and the caps requests, so every action of the card can be tried and photographed; nothing
//! is stored, nothing is sent anywhere, and a file that cannot be read shows its error on the card.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::Path;

use femme_proto::rpc::{AccountCap, Req, Resp};
use gpui::{Context, Entity, Task};
use serde_json::{json, Value};

use super::{api, clock};
use crate::net::Net;

/// The environment variable that points the overlay at a fixture file.
pub const FIXTURE_ENV: &str = "FEMME_ACCOUNTS_FIXTURE";

/// The link the fake board's Claude sign-in hands out.
const SIGN_IN: &str = "https://claude.example.test/oauth/authorize?code=true&state=fixture";

/// Who answers the card's requests.
pub enum Source {
    /// Nobody: the desktop runs on synthetic panes, with no daemon.
    Detached,
    /// The daemon.
    Daemon(Entity<Net>),
    /// The fake board over a fixture file.
    Fixture(Box<Fixture>),
    /// The fixture file named in the environment could not be used: its error.
    Broken(String),
}

impl Source {
    /// The fixture named by [`FIXTURE_ENV`], else the daemon `net` talks to, else nobody.
    pub fn pick(net: Option<Entity<Net>>) -> Self {
        Self::pick_with(std::env::var_os(FIXTURE_ENV).as_deref(), net)
    }

    fn pick_with(fixture: Option<&OsStr>, net: Option<Entity<Net>>) -> Self {
        match fixture.filter(|p| !p.is_empty()) {
            Some(path) => match Fixture::load(Path::new(path)) {
                Ok(f) => Self::Fixture(Box::new(f)),
                Err(e) => Self::Broken(e),
            },
            None => net.map_or(Self::Detached, Self::Daemon),
        }
    }

    /// Sends `req`; the answer, or the reason it failed (for the daemon: what [`Net::error`] said).
    pub fn send<T: 'static>(&self, req: Req, cx: &mut Context<T>) -> Task<Result<Resp, String>> {
        match self {
            Self::Daemon(net) => {
                let sent = net.update(cx, |n, cx| n.run(req, cx));
                let net = net.downgrade();
                cx.spawn(async move |_, cx| match sent.await {
                    Some(resp) => Ok(resp),
                    None => Err(net
                        .read_with(cx, |n, _| n.error.clone())
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "the request failed".into())),
                })
            }
            Self::Fixture(f) => Task::ready(f.answer(req)),
            Self::Broken(e) => Task::ready(Err(e.clone())),
            Self::Detached => Task::ready(Err(
                "no daemon: the desktop is running on synthetic panes".into(),
            )),
        }
    }
}

/// The fake board: the accounts, keys, caps and meta of a fixture file, changed in memory by the
/// requests the card makes.
#[derive(Debug)]
pub struct Fixture {
    state: RefCell<State>,
}

#[derive(Debug)]
struct State {
    accounts: Value,
    keys: Vec<Value>,
    caps: Vec<AccountCap>,
    meta: Value,
}

impl Fixture {
    /// Reads the fixture file at `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("{FIXTURE_ENV}: cannot read {}: {e}", path.display()))?;
        Self::parse(&json).map_err(|e| format!("{FIXTURE_ENV}: {}: {e}", path.display()))
    }

    /// A fixture from its JSON text.
    pub fn parse(json: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let part = |key: &str| v.get(key).cloned().unwrap_or(Value::Null);
        let accounts = match part("accounts") {
            Value::Null => json!({}),
            a if a.is_object() => a,
            _ => return Err("\"accounts\" must be an object".into()),
        };
        let keys = match part("keys") {
            Value::Null => vec![],
            Value::Array(k) => k,
            _ => return Err("\"keys\" must be a list".into()),
        };
        let caps = match part("caps") {
            Value::Null => vec![],
            c => serde_json::from_value(c).map_err(|e| format!("\"caps\": {e}"))?,
        };
        Ok(Self {
            state: RefCell::new(State {
                accounts,
                keys,
                caps,
                meta: part("meta"),
            }),
        })
    }

    /// The answer to one request: the caps and the board's account and key routes; anything else
    /// is `Unsupported`, as it is from a daemon that does not know it.
    pub fn answer(&self, req: Req) -> Result<Resp, String> {
        let mut st = self.state.borrow_mut();
        match req {
            Req::Caps => Ok(Resp::Caps(st.caps.clone())),
            Req::SetCap {
                account,
                five_hour,
                weekly,
            } => {
                st.caps.retain(|c| c.account != account);
                if five_hour.is_some() || weekly.is_some() {
                    st.caps.push(AccountCap {
                        account,
                        five_hour,
                        weekly,
                    });
                    st.caps.sort_by_key(|c| c.account);
                }
                Ok(Resp::Ok)
            }
            Req::BoardApi { method, path, body } => {
                let input: Value = serde_json::from_str(&body).unwrap_or_default();
                let (status, v) = st.board(&method, &path, &input);
                Ok(Resp::BoardApi {
                    status,
                    body: v.to_string(),
                })
            }
            _ => Err("Unsupported: the fixture answers the accounts requests only".into()),
        }
    }
}

fn error(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({"error": msg}))
}

impl State {
    fn board(&mut self, method: &str, path: &str, input: &Value) -> (u16, Value) {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("GET", ["api", "accounts"]) => (200, self.accounts.clone()),
            ("GET", ["api", "keys"]) => (200, Value::Array(self.keys.clone())),
            ("GET", ["api", "meta"]) => (200, self.meta.clone()),
            ("POST", ["api", "accounts", "switch"]) => self.switch(input),
            ("POST", ["api", "accounts", "claude", "login"]) => {
                self.accounts["login"] = json!({"claude": SIGN_IN});
                (200, self.accounts.clone())
            }
            ("POST", ["api", "accounts", "claude", "code"]) => self.sign_in_code(input),
            ("DELETE", ["api", "accounts", _, "login"]) => {
                self.accounts["login"] = json!({});
                (200, self.accounts.clone())
            }
            ("POST", ["api", "keys"]) => self.key_add(input),
            ("DELETE", ["api", "keys", name]) => match self.key(name) {
                Some(at) => {
                    self.keys.remove(at);
                    (200, Value::Array(self.keys.clone()))
                }
                None => error(404, "not found"),
            },
            ("POST", ["api", "keys", name, "github"]) => self.github(name, true),
            ("DELETE", ["api", "keys", name, "github"]) => self.github(name, false),
            _ => error(404, "not found"),
        }
    }

    fn key(&self, name: &str) -> Option<usize> {
        self.keys.iter().position(|k| k["name"] == name)
    }

    fn list(&mut self) -> Option<&mut Vec<Value>> {
        self.accounts.pointer_mut("/cswap/accounts")?.as_array_mut()
    }

    fn switch(&mut self, input: &Value) -> (u16, Value) {
        let n = input.get("account").and_then(Value::as_str).unwrap_or("");
        if n.is_empty() || n.len() > 3 || !n.bytes().all(|b| b.is_ascii_digit()) {
            return error(400, "account must be a number");
        }
        let n: u64 = n.parse().unwrap_or_default();
        let Some(list) = self.list() else {
            return error(502, "cswap: no accounts");
        };
        if !list.iter().any(|a| a["number"] == n) {
            return error(502, "no such account");
        }
        for a in list.iter_mut() {
            a["active"] = json!(a["number"] == n);
        }
        self.accounts["cswap"]["activeAccountNumber"] = json!(n);
        (200, self.accounts.clone())
    }

    fn sign_in_code(&mut self, input: &Value) -> (u16, Value) {
        let code: String = input
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if code.is_empty() {
            return error(400, "paste the code exactly as the sign-in page shows it");
        }
        if self.accounts["login"].get("claude").is_none() {
            return error(409, "that sign-in ended; start again");
        }
        self.accounts["login"] = json!({});
        let Some(list) = self.list() else {
            return error(502, "cswap: no accounts");
        };
        let next = list
            .iter()
            .filter_map(|a| a["number"].as_u64())
            .max()
            .unwrap_or(0)
            + 1;
        list.push(json!({
            "number": next,
            "email": format!("account{next}@example.test"),
            "active": false,
            "usageStatus": "unavailable",
            "usage": null,
        }));
        (200, self.accounts.clone())
    }

    fn key_add(&mut self, input: &Value) -> (u16, Value) {
        let text = |k: &str| {
            input
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        let (name, token) = (text("name"), text("github_token"));
        if !api::valid_key_name(&name) {
            return error(400, api::NAME_RULE);
        }
        if self.key(&name).is_some() {
            return error(409, "a key with that name exists: revoke it first");
        }
        let account = if token.is_empty() {
            "the box"
        } else {
            "Fixture User (fixture-user)"
        };
        self.keys.push(json!({
            "name": name, "created": clock::now(), "hint": "f1x7",
            "account": account, "github": null, "error": null,
        }));
        (
            201,
            json!({"name": name, "key": "fixture-key-not-a-secret-f1x7", "keys": self.keys}),
        )
    }

    fn github(&mut self, name: &str, start: bool) -> (u16, Value) {
        let Some(at) = self.key(name) else {
            return error(404, "not found");
        };
        self.keys[at]["error"] = Value::Null;
        self.keys[at]["github"] = if start {
            json!({"code": "FIXT-0001", "url": "https://github.com/login/device"})
        } else {
            Value::Null
        };
        (200, Value::Array(self.keys.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::model::{self, AccountList};

    fn fake() -> Fixture {
        Fixture::parse(include_str!("fixture.json")).unwrap()
    }

    /// Sends a board call and returns its status and parsed body.
    fn call(f: &Fixture, req: Req) -> (u16, Value) {
        match f.answer(req).unwrap() {
            Resp::BoardApi { status, body } => (status, serde_json::from_str(&body).unwrap()),
            other => panic!("not a board answer: {other:?}"),
        }
    }

    fn numbers(f: &Fixture) -> Vec<(u32, bool)> {
        let (_, v) = call(f, api::accounts());
        match model::parse_accounts(&v.to_string()).unwrap().list {
            AccountList::Rows(rows) => rows.iter().map(|a| (a.number, a.active)).collect(),
            other => panic!("{other:?}"),
        }
    }

    fn names(f: &Fixture) -> Vec<String> {
        let (_, v) = call(f, api::keys());
        model::parse_keys(&v.to_string())
            .unwrap()
            .into_iter()
            .map(|k| k.name)
            .collect()
    }

    #[test]
    fn a_fixture_answers_the_reads() {
        let f = fake();
        assert_eq!(numbers(&f), [(1, true), (2, false), (3, false), (4, false)]);
        assert_eq!(names(&f), ["instinct", "ci-bot", "review", "old-bot"]);
        assert_eq!(
            model::parse_meta(&call(&f, api::meta()).1.to_string()).cap,
            Some(60)
        );
        assert_eq!(
            f.answer(api::caps()),
            Ok(Resp::Caps(vec![
                AccountCap {
                    account: 1,
                    five_hour: Some(80),
                    weekly: None
                },
                AccountCap {
                    account: 2,
                    five_hour: None,
                    weekly: Some(90)
                },
            ]))
        );
    }

    #[test]
    fn caps_are_kept_changed_and_cleared() {
        let f = fake();
        let caps = |f: &Fixture| match f.answer(api::caps()) {
            Ok(Resp::Caps(c)) => c
                .iter()
                .map(|c| (c.account, c.five_hour, c.weekly))
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(f.answer(api::set_cap(3, Some(70), Some(95))), Ok(Resp::Ok));
        assert_eq!(f.answer(api::set_cap(1, None, Some(75))), Ok(Resp::Ok));
        assert_eq!(
            caps(&f),
            [
                (1, None, Some(75)),
                (2, None, Some(90)),
                (3, Some(70), Some(95))
            ]
        );
        // Both None clears the account's caps.
        assert_eq!(f.answer(api::set_cap(2, None, None)), Ok(Resp::Ok));
        assert_eq!(caps(&f), [(1, None, Some(75)), (3, Some(70), Some(95))]);
    }

    #[test]
    fn a_switch_moves_the_active_mark_and_refuses_what_the_board_refuses() {
        let f = fake();
        let (status, v) = call(&f, api::switch(3));
        assert_eq!(status, 200);
        assert_eq!(numbers(&f), [(1, false), (2, false), (3, true), (4, false)]);
        assert_eq!(v["cswap"]["activeAccountNumber"], 3);
        assert_eq!(
            call(&f, api::switch(9)),
            (502, json!({"error": "no such account"}))
        );
        // A number rather than a string of digits is what the board answers 400 to.
        let (status, v) = call(
            &f,
            api::board("POST", "/api/accounts/switch", Some(json!({"account": 2}))),
        );
        assert_eq!(
            (status, v["error"].as_str()),
            (400, Some("account must be a number"))
        );
        assert_eq!(numbers(&f)[2], (3, true), "nothing changed");
    }

    #[test]
    fn the_sign_in_hands_out_a_link_and_takes_a_code() {
        let f = fake();
        let (_, v) = call(&f, api::login_start());
        let doc = model::parse_accounts(&v.to_string()).unwrap();
        assert_eq!(doc.login.as_deref(), Some(SIGN_IN));
        let (status, v) = call(&f, api::login_code("  \n"));
        assert_eq!(
            (status, v["error"].as_str()),
            (
                400,
                Some("paste the code exactly as the sign-in page shows it")
            )
        );
        let (status, v) = call(&f, api::login_code("abc#def"));
        assert_eq!(status, 200);
        let doc = model::parse_accounts(&v.to_string()).unwrap();
        assert_eq!(doc.login, None);
        assert_eq!(numbers(&f).len(), 5, "the new account joined the list");
        // The code needs a sign-in to hand it to.
        let (status, _) = call(&f, api::login_code("abc#def"));
        assert_eq!(status, 409);
        // Cancelling clears a sign-in.
        call(&f, api::login_start());
        let (_, v) = call(&f, api::login_cancel());
        assert_eq!(model::parse_accounts(&v.to_string()).unwrap().login, None);
    }

    #[test]
    fn keys_are_made_connected_and_revoked() {
        let f = fake();
        let (status, v) = call(&f, api::key_add("ci", ""));
        assert_eq!(status, 201);
        let made = model::parse_new_key(&v.to_string()).unwrap();
        assert_eq!(made.name, "ci");
        assert_eq!(made.keys.len(), 5);
        assert_eq!(made.keys[4].account, "the box");
        let (status, v) = call(&f, api::key_add("ci", ""));
        assert_eq!(
            (status, v["error"].as_str()),
            (409, Some("a key with that name exists: revoke it first"))
        );
        let (status, v) = call(&f, api::key_add("Bad Name", ""));
        assert_eq!((status, v["error"].as_str()), (400, Some(api::NAME_RULE)));
        let (_, v) = call(&f, api::key_add("with-token", "ghp_fixturefixturefixture"));
        assert_eq!(
            model::parse_new_key(&v.to_string()).unwrap().keys[5].account,
            "Fixture User (fixture-user)"
        );

        let (_, v) = call(&f, api::github_start("ci"));
        let keys = model::parse_keys(&v.to_string()).unwrap();
        assert_eq!(
            keys[4].github.as_ref().map(|d| d.code.as_str()),
            Some("FIXT-0001")
        );
        let (_, v) = call(&f, api::github_cancel("ci"));
        assert_eq!(model::parse_keys(&v.to_string()).unwrap()[4].github, None);
        assert_eq!(call(&f, api::github_start("nope")).0, 404);

        let (status, v) = call(&f, api::key_revoke("instinct"));
        assert_eq!(status, 200);
        assert_eq!(model::parse_keys(&v.to_string()).unwrap().len(), 5);
        assert_eq!(call(&f, api::key_revoke("instinct")).0, 404);
    }

    #[test]
    fn what_a_fixture_does_not_know_is_unsupported_or_not_found() {
        let f = fake();
        assert_eq!(call(&f, api::board("GET", "/api/nope", None)).0, 404);
        let err = f.answer(Req::ServerInfo).unwrap_err();
        assert!(err.starts_with("Unsupported"), "{err}");
        // An old daemon says the same, and the card words it plainly.
        assert!(model::explain(&err).starts_with("daemon too old for accounts"));
    }

    #[test]
    fn a_fixture_needs_an_object_of_accounts_and_sane_parts() {
        assert!(Fixture::parse("nope").is_err());
        assert!(Fixture::parse(r#"{"accounts": []}"#)
            .unwrap_err()
            .contains("accounts"));
        assert!(Fixture::parse(r#"{"keys": {}}"#)
            .unwrap_err()
            .contains("keys"));
        assert!(Fixture::parse(r#"{"caps": [{"account": "x"}]}"#)
            .unwrap_err()
            .contains("caps"));
        // Every part but the accounts may be left out; those are then empty.
        let bare = Fixture::parse("{}").unwrap();
        assert_eq!(
            model::parse_accounts(&call(&bare, api::accounts()).1.to_string())
                .unwrap()
                .list,
            AccountList::Pending
        );
        assert_eq!(bare.answer(api::caps()), Ok(Resp::Caps(vec![])));
    }

    #[test]
    fn the_source_is_the_fixture_when_the_variable_names_one() {
        let dir =
            std::env::temp_dir().join(format!("femme-accounts-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.json");
        std::fs::write(&ok, include_str!("fixture.json")).unwrap();
        assert!(matches!(
            Source::pick_with(Some(ok.as_os_str()), None),
            Source::Fixture(_)
        ));
        let bad = dir.join("missing.json");
        match Source::pick_with(Some(bad.as_os_str()), None) {
            Source::Broken(e) => {
                assert!(e.contains(FIXTURE_ENV) && e.contains("missing.json"), "{e}")
            }
            _ => panic!("a missing file is Broken"),
        }
        std::fs::write(&bad, "{").unwrap();
        assert!(matches!(
            Source::pick_with(Some(bad.as_os_str()), None),
            Source::Broken(_)
        ));
        // Empty or unset: no fixture; with no daemon either, nobody answers.
        assert!(matches!(
            Source::pick_with(Some(OsStr::new("")), None),
            Source::Detached
        ));
        assert!(matches!(Source::pick_with(None, None), Source::Detached));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
