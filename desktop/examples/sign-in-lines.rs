//! Prints what the accounts card says about each account's claude.ai sign-in, from a JSON file:
//! `cswap list --json` output, a `/api/accounts` body, or `src/accounts/fixture.json`.
//!
//!     cswap list --json > /tmp/list.json && cargo run --example sign-in-lines -- /tmp/list.json
//!
//! Shows no emails: an account is named by its number only.

use femme_desktop::accounts::clock;
use femme_desktop::accounts::model::{parse_accounts, AccountList, Level};
use serde_json::{json, Value};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: sign-in-lines <json file>");
    let text = std::fs::read_to_string(&path).expect("cannot read the file");
    let v: Value = serde_json::from_str(&text).expect("not JSON");
    // The fixture wraps the body as `accounts`; cswap's own list needs wrapping as `cswap`.
    let body = if v.get("accounts").is_some_and(|a| a.get("cswap").is_some()) {
        v["accounts"].clone()
    } else if v.get("cswap").is_some() {
        v
    } else {
        json!({ "cswap": v })
    };
    let doc = parse_accounts(&body.to_string()).expect("not an accounts body");
    let AccountList::Rows(rows) = doc.list else {
        panic!("no account rows: {:?}", doc.list);
    };
    let now = clock::now();
    let mut alert = false;
    for a in &rows {
        let state = if a.active {
            "active"
        } else if a.disabled {
            "disabled"
        } else {
            ""
        };
        match a.sign_in(now) {
            None => println!("account {:<2} {:<9} (quiet)", a.number, state),
            Some((line, Level::Warn)) => {
                println!("account {:<2} {:<9} ● {line}  [amber]", a.number, state)
            }
            Some((line, Level::Bad)) => {
                alert = true;
                println!(
                    "account {:<2} {:<9} ● {line}  [red]  [sign in again]",
                    a.number, state
                );
            }
        }
    }
    println!(
        "menu ▸ Accounts: {}",
        if alert { "● red dot" } else { "no dot" }
    );
}
