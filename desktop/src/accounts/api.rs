//! The requests the card makes. The accounts and keys are the web app's own routes, called by the
//! daemon on the desktop's behalf (`Req::BoardApi`); the per-account caps are the daemon's own
//! (`Req::Caps`, `Req::SetCap`). Bodies are what the board's routes read (compat `accounts.rs` and
//! `keys.rs`): a switch names the account as a *string* of digits, a POST with nothing to say sends
//! `{}`, and keys are told apart by name.

use femme_proto::rpc::Req;
use serde_json::{json, Value};

/// One call of the board's JSON API. `body` is the JSON to send (none for GET and DELETE).
pub fn board(method: &str, path: impl Into<String>, body: Option<Value>) -> Req {
    Req::BoardApi {
        method: method.into(),
        path: path.into(),
        body: body.map_or_else(String::new, |b| b.to_string()),
    }
}

/// `GET /api/accounts`: cswap's list, the AWS identity and the sign-in in progress.
pub fn accounts() -> Req {
    board("GET", "/api/accounts", None)
}

/// `GET /api/keys`.
pub fn keys() -> Req {
    board("GET", "/api/keys", None)
}

/// `GET /api/meta`: the box's default cap and the scheduler's word.
pub fn meta() -> Req {
    board("GET", "/api/meta", None)
}

/// Every account's caps, from the daemon.
pub fn caps() -> Req {
    Req::Caps
}

/// Sets both caps of an account (`None`: the box's default); both `None` clears them.
pub fn set_cap(account: u32, five_hour: Option<u8>, weekly: Option<u8>) -> Req {
    Req::SetCap {
        account,
        five_hour,
        weekly,
    }
}

/// `POST /api/accounts/switch`: the box moves to account `number`.
pub fn switch(number: u32) -> Req {
    board(
        "POST",
        "/api/accounts/switch",
        Some(json!({"account": number.to_string()})),
    )
}

/// `POST /api/accounts/claude/login`: starts a sign-in; the answer carries its link.
pub fn login_start() -> Req {
    board("POST", "/api/accounts/claude/login", Some(json!({})))
}

/// `POST /api/accounts/claude/code`: hands the code the sign-in page showed to the waiting login.
pub fn login_code(code: &str) -> Req {
    board(
        "POST",
        "/api/accounts/claude/code",
        Some(json!({"code": code.trim()})),
    )
}

/// `DELETE /api/accounts/claude/login`: gives the sign-in up.
pub fn login_cancel() -> Req {
    board("DELETE", "/api/accounts/claude/login", None)
}

/// `POST /api/keys`: a new key, with a GitHub token for its clones to push as when one is given.
pub fn key_add(name: &str, github_token: &str) -> Req {
    board(
        "POST",
        "/api/keys",
        Some(json!({"name": name.trim(), "github_token": github_token.trim()})),
    )
}

/// `DELETE /api/keys/{name}`: revokes the key.
pub fn key_revoke(name: &str) -> Req {
    board("DELETE", format!("/api/keys/{}", segment(name)), None)
}

/// `POST /api/keys/{name}/github`: starts a GitHub device sign-in for the key.
pub fn github_start(name: &str) -> Req {
    board(
        "POST",
        format!("/api/keys/{}/github", segment(name)),
        Some(json!({})),
    )
}

/// `DELETE /api/keys/{name}/github`: gives that sign-in up.
pub fn github_cancel(name: &str) -> Req {
    board(
        "DELETE",
        format!("/api/keys/{}/github", segment(name)),
        None,
    )
}

/// A key name as the board takes it: a-z, 0-9 and `-`, up to 32 characters, not starting with `-`.
pub fn valid_key_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What the board says to a bad key name.
pub const NAME_RULE: &str = "name: a-z, 0-9 and -, up to 32 characters";

/// A path segment, percent-encoded: keys are named `[a-z0-9-]`, but a name the board lists is
/// not trusted to stay that way.
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(req: Req) -> (String, String, Value) {
        match req {
            Req::BoardApi { method, path, body } => (
                method,
                path,
                if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&body).unwrap()
                },
            ),
            other => panic!("not a board call: {other:?}"),
        }
    }

    #[test]
    fn reads_are_gets_without_a_body() {
        for (req, path) in [
            (accounts(), "/api/accounts"),
            (keys(), "/api/keys"),
            (meta(), "/api/meta"),
        ] {
            assert_eq!(parts(req), ("GET".into(), path.into(), Value::Null));
        }
        assert_eq!(caps(), Req::Caps);
    }

    #[test]
    fn a_switch_names_the_account_as_a_string_of_digits() {
        // The board answers 400 "account must be a number" to {"account": 2}.
        assert_eq!(
            parts(switch(2)),
            (
                "POST".into(),
                "/api/accounts/switch".into(),
                json!({"account": "2"})
            )
        );
        assert_eq!(parts(switch(12)).2, json!({"account": "12"}));
    }

    #[test]
    fn the_claude_sign_in_is_login_then_code_or_cancel() {
        assert_eq!(
            parts(login_start()),
            (
                "POST".into(),
                "/api/accounts/claude/login".into(),
                json!({})
            )
        );
        assert_eq!(
            parts(login_code("  abc#def \n")),
            (
                "POST".into(),
                "/api/accounts/claude/code".into(),
                json!({"code": "abc#def"})
            )
        );
        assert_eq!(
            parts(login_cancel()),
            (
                "DELETE".into(),
                "/api/accounts/claude/login".into(),
                Value::Null
            )
        );
    }

    #[test]
    fn keys_are_made_revoked_and_connected_by_name() {
        assert_eq!(
            parts(key_add(" ci ", " ghp_x ")),
            (
                "POST".into(),
                "/api/keys".into(),
                json!({"name": "ci", "github_token": "ghp_x"})
            )
        );
        // No token: the field is there and empty, as the page sends it.
        assert_eq!(
            parts(key_add("ci", "")).2,
            json!({"name": "ci", "github_token": ""})
        );
        assert_eq!(
            parts(key_revoke("ci")),
            ("DELETE".into(), "/api/keys/ci".into(), Value::Null)
        );
        assert_eq!(
            parts(github_start("ci")),
            ("POST".into(), "/api/keys/ci/github".into(), json!({}))
        );
        assert_eq!(
            parts(github_cancel("ci")),
            ("DELETE".into(), "/api/keys/ci/github".into(), Value::Null)
        );
    }

    #[test]
    fn a_name_cannot_leave_its_path_segment() {
        assert_eq!(
            parts(key_revoke("../accounts/x y")).1,
            "/api/keys/..%2Faccounts%2Fx%20y"
        );
        assert_eq!(parts(key_revoke("é")).1, "/api/keys/%C3%A9");
        assert_eq!(parts(github_start("a?b#c")).1, "/api/keys/a%3Fb%23c/github");
    }

    #[test]
    fn caps_go_to_the_daemon_whole() {
        assert_eq!(
            set_cap(2, Some(80), None),
            Req::SetCap {
                account: 2,
                five_hour: Some(80),
                weekly: None
            }
        );
    }

    #[test]
    fn key_names_follow_the_boards_rule() {
        for good in ["a", "instinct", "ci-bot", "0", "a-", &"x".repeat(32)] {
            assert!(valid_key_name(good), "{good}");
        }
        for bad in ["", "-a", "A", "a b", "a_b", "é", &"x".repeat(33), "a/b"] {
            assert!(!valid_key_name(bad), "{bad:?}");
        }
    }
}
