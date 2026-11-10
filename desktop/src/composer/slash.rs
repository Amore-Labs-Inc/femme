//! The `/` menu: which of the daemon's commands (`Req::Commands`) to offer while the draft is a
//! command word being typed. Pure, so the view in `composer.rs` only draws and routes keys.

use femme_proto::rpc::PromptHow;

/// The most rows the menu shows.
pub const MAX: usize = 8;

/// What `/model` offers, as menu rows: the names the daemon's `model_switch` knows.
pub const MODELS: &[&str] = &[
    "model opus",
    "model sonnet",
    "model haiku",
    "model fable",
    "model deepseek",
];

/// The id each `/model` name stands for, as the daemon's `model_switch` maps it: the menu names
/// the model in full.
// ponytail: a copy of model_switch's table; move it to femme-proto if they ever drift
const IDS: &[(&str, &str)] = &[
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("fable", "claude-fable-5-2"),
    ("deepseek", "deepseek/deepseek-v4.1-flash-ultrafast"),
];

/// A menu row as shown: `model opus xhigh` is `model opus 5.5 xhigh`; any other row as it is.
pub fn label(row: &str) -> String {
    let Some(rest) = row.strip_prefix("model ") else {
        return row.to_owned();
    };
    let (m, effort) = rest.split_once(' ').unwrap_or((rest, ""));
    match IDS.iter().find(|(name, _)| *name == m) {
        Some((_, id)) => format!("model {} {effort}", super::draft::model_name(id))
            .trim_end()
            .to_owned(),
        None => row.to_owned(),
    }
}

/// How hard a Claude model thinks (`/effort`), offered once its model is picked.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// A picked `model <name> <effort>` row: the two commands it sends, `/model <name>` then
/// `/effort <effort>`.
pub fn model_and_effort(row: &str) -> Option<(String, String)> {
    let (model, effort) = row.strip_prefix("model ")?.split_once(' ')?;
    Some((format!("/model {model}"), format!("/effort {effort}")))
}

/// What follows the `/` while the user is still typing the command word: the text starts with `/`
/// and holds no whitespace yet. `None` for anything else (a prompt, a command with arguments).
pub fn query(text: &str) -> Option<&str> {
    text.strip_prefix('/')
        .filter(|q| !q.contains(char::is_whitespace))
}

/// The names to offer for `text`, at most [`MAX`]: those that start with the query (case aside)
/// alphabetically, then those that hold it elsewhere. An empty query offers the first names.
pub fn matches(text: &str, names: &[String]) -> Vec<String> {
    if let Some(rest) = text.strip_prefix("/model ") {
        let words: Vec<&str> = rest.split(' ').collect();
        return match words[..] {
            // a name being typed: the models
            [m] => {
                let m = m.to_lowercase();
                MODELS
                    .iter()
                    .filter(|row| row["model ".len()..].contains(&m))
                    .map(|row| (*row).to_owned())
                    .collect()
            }
            // a Claude model picked, its effort being typed: the efforts
            [m, e] if m != "deepseek" && MODELS.contains(&format!("model {m}").as_str()) => EFFORTS
                .iter()
                .filter(|x| x.starts_with(&e.to_lowercase()))
                .map(|x| format!("model {m} {x}"))
                .collect(),
            _ => Vec::new(),
        };
    }
    let Some(q) = query(text) else {
        return Vec::new();
    };
    let q = q.to_lowercase();
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let (mut prefix, mut inside) = (Vec::new(), Vec::new());
    for name in sorted {
        let low = name.to_lowercase();
        if low.starts_with(&q) {
            prefix.push(name);
        } else if low.contains(&q) {
            inside.push(name);
        }
    }
    prefix.extend(inside);
    prefix.truncate(MAX);
    prefix.into_iter().map(str::to_owned).collect()
}

/// Whether `text` is exactly `/` and one of the names: Enter then sends it rather than completing.
/// Not `/model`: Enter on it opens the list of models.
pub fn exact(text: &str, names: &[String]) -> bool {
    text.strip_prefix('/')
        .is_some_and(|q| q != "model" && names.iter().any(|n| n == q))
}

/// The input's text once `name` is picked: the command and a space for its arguments.
pub fn complete(name: &str) -> String {
    format!("/{name} ")
}

/// How a send reaches a session that is `working`, as the terminal's keys do: Enter joins the
/// running turn at its next tool boundary (`Steer`), ctrl-Enter (`secondary`) stops the turn and
/// sends now (`InterruptThenSend`), and a `/command` waits for the turn to end (`Queue`), since the
/// terminal holds commands back too. An idle session takes every key as a plain send.
// ponytail: the terminal's newer "send now" moves running shell or subagent work to the background
// instead of interrupting it; ctrl-Enter can do the same once FEMME has a background request.
pub fn how_for(working: bool, secondary: bool, text: &str) -> PromptHow {
    match (working, secondary) {
        (false, _) => PromptHow::Queue,
        (true, _) if text.trim_start().starts_with('/') => PromptHow::Queue,
        (true, false) => PromptHow::Steer,
        (true, true) => PromptHow::InterruptThenSend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn enter_steers_a_working_session_and_commands_wait() {
        use PromptHow::*;
        assert_eq!(how_for(true, false, "also say banana"), Steer);
        assert_eq!(how_for(true, true, "stop, do this"), InterruptThenSend);
        assert_eq!(how_for(true, false, "/compact"), Queue);
        assert_eq!(how_for(true, true, "/compact"), Queue);
        assert_eq!(how_for(false, false, "hi"), Queue);
        assert_eq!(how_for(false, true, "hi"), Queue);
    }

    #[test]
    fn only_a_command_word_being_typed_is_a_query() {
        assert_eq!(query("/"), Some(""));
        assert_eq!(query("/rev"), Some("rev"));
        assert_eq!(query("/review "), None);
        assert_eq!(query("/review\n"), None);
        assert_eq!(query("hi /rev"), None);
        assert_eq!(query(""), None);
    }

    #[test]
    fn prefix_matches_come_first_then_the_rest() {
        let n = names(&["context", "compact", "btw", "review", "pr-comments", "cost"]);
        assert_eq!(
            matches("/co", &n),
            ["compact", "context", "cost", "pr-comments"]
        );
        assert_eq!(
            matches("/CO", &n),
            ["compact", "context", "cost", "pr-comments"]
        );
        assert_eq!(matches("/view", &n), ["review"]);
        assert!(matches("/zzz", &n).is_empty());
        assert!(matches("/co x", &n).is_empty());
        assert!(matches("co", &n).is_empty());
    }

    #[test]
    fn an_empty_query_offers_the_first_eight_alphabetically() {
        let n = names(&["j", "i", "h", "g", "f", "e", "d", "c", "b", "a", "a"]);
        assert_eq!(matches("/", &n), ["a", "b", "c", "d", "e", "f", "g", "h"]);
        assert!(matches("/", &[]).is_empty());
    }

    #[test]
    fn model_and_a_space_offers_the_models() {
        let n = names(&["model", "review"]);
        assert_eq!(matches("/model ", &n), MODELS);
        assert_eq!(matches("/model so", &n), ["model sonnet"]);
        assert_eq!(
            matches("/model opus ", &n),
            [
                "model opus low",
                "model opus medium",
                "model opus high",
                "model opus xhigh",
                "model opus max"
            ]
        );
        assert_eq!(matches("/model opus x", &n), ["model opus xhigh"]);
        assert!(
            matches("/model deepseek ", &n).is_empty(),
            "no effort off Claude"
        );
        assert!(matches("/model opus high ", &n).is_empty());
        assert_eq!(
            model_and_effort("model opus high"),
            Some(("/model opus".into(), "/effort high".into()))
        );
        assert_eq!(model_and_effort("model opus"), None);
        assert!(!exact("/model", &n), "Enter on /model opens the list");
        assert_eq!(label("model opus"), "model opus 5.5");
        assert_eq!(label("model fable xhigh"), "model fable 5.2 xhigh");
        assert_eq!(
            label("model deepseek"),
            "model deepseek-v4.1-flash-ultrafast"
        );
        assert_eq!(label("compact"), "compact");
    }

    #[test]
    fn exact_names_a_command_and_nothing_more() {
        let n = names(&["review", "compact"]);
        assert!(exact("/review", &n));
        assert!(!exact("/rev", &n));
        assert!(!exact("/review ", &n));
        assert!(!exact("review", &n));
        assert_eq!(complete("review"), "/review ");
    }
}
