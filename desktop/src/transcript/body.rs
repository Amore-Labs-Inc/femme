//! What the JSON bodies of items say. The daemon stores a body as JSON text shaped per kind
//! (`femme-agents/src/items.rs`); a body longer than the blob cap is cut mid-string and ends with
//! `…[truncated]`, which is no valid JSON, so the readers fall back to pulling the one field they
//! need out of the raw text.

use femme_proto::state::ItemKind;
use serde_json::Value;

use super::question::{self, QuestionBody};

/// The keys of a tool's input that say what it does, in the daemon's order (`preview_of`).
const HINT_KEYS: [&str; 7] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "url",
    "query",
    "description",
];

/// An item's body, read.
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    /// The text of a user, assistant, thinking, plan or event item.
    Text(String),
    /// A tool the agent called.
    ToolCall(ToolCall),
    /// What a tool answered.
    ToolResult(ToolResult),
    /// A question for the user, and its answer once given.
    Question(QuestionBody),
    /// The end of a turn.
    TurnEnd(TurnEnd),
    /// Nothing to read: there is no body yet (the item is in progress, or its blob has not
    /// arrived), or the kind has none.
    Empty,
}

/// `ItemKind::ToolCall`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The tool's name as the agent calls it.
    pub name: String,
    /// The tool's input; `None` until it is complete, or when it was cut.
    pub input: Option<Value>,
    /// Where the call's task runs, once the agent runs it as one (Claude: a Bash command or a
    /// subagent): `foreground` while the turn waits for it, `background` once it was moved.
    pub task: Option<String>,
    /// Claude's `PushNotification` call that femmed sent to the phone itself (Claude Code only
    /// delivers through Remote Control): [`PUSH_SENT`], or why it did not arrive.
    pub push: Option<String>,
}

/// Claude Code's tool for telling the user's phone something.
pub const PUSH_TOOL: &str = "PushNotification";
/// [`ToolCall::push`] of a push that arrived.
pub const PUSH_SENT: &str = "sent";

impl ToolCall {
    /// The first of the input's usual keys that holds a string: the command, the path, the
    /// pattern.
    pub fn hint(&self) -> String {
        let input = self.input.as_ref();
        HINT_KEYS
            .iter()
            .find_map(|k| input?.get(k)?.as_str())
            .map(|s| s.lines().next().unwrap_or("").to_owned())
            .unwrap_or_default()
    }

    /// The input as text for the expanded row: the command of a shell call as it is, any other
    /// input as indented JSON.
    pub fn input_text(&self) -> Option<String> {
        let input = self.input.as_ref().filter(|v| !v.is_null())?;
        if let Some(cmd) = input.get("command").and_then(Value::as_str) {
            if input.as_object().is_some_and(|o| o.len() == 1) {
                return Some(cmd.to_owned());
            }
        }
        if input.as_object().is_some_and(|o| o.is_empty()) {
            return None;
        }
        serde_json::to_string_pretty(input).ok()
    }
}

/// `ItemKind::ToolResult`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// The output, flattened to text.
    pub output: String,
    /// The tool reported failure.
    pub is_error: bool,
    /// The claude.ai page an `Artifact` call points at (`femme_agents::items::ArtifactLink`).
    pub link: Option<ArtifactLink>,
}

/// A claude.ai artifact page named by an `Artifact` tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactLink {
    /// `https://claude.ai/artifact/<id>`.
    pub url: String,
    /// The page's title (or the file's name).
    pub title: String,
    /// The version number of a publish, when known.
    pub version: Option<u64>,
    /// The publish changed an existing page.
    pub updated: bool,
}

/// Why a turn ended (`femme_agents::StopReason`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The agent finished normally.
    EndTurn,
    /// The output limit was reached.
    MaxTokens,
    /// The user (or the daemon) interrupted it.
    Interrupted,
    /// The turn failed; the agent's own message.
    Error(String),
    /// Anything else the agent reported.
    Other(String),
}

/// `ItemKind::TurnEnd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEnd {
    /// Why it ended.
    pub stop: Stop,
    /// How long it ran, as the agent measured it.
    pub duration_ms: Option<u64>,
}

/// Reads `json` as the body of an item of `kind`. `None` (no body) and bodies that say nothing
/// usable read as [`Parsed::Empty`].
pub fn parse(kind: ItemKind, json: Option<&str>) -> Parsed {
    let Some(json) = json else {
        return Parsed::Empty;
    };
    match kind {
        ItemKind::UserMessage
        | ItemKind::PeerMessage
        | ItemKind::AssistantMessage
        | ItemKind::Thinking
        | ItemKind::Plan
        | ItemKind::Event => text(json).map_or(Parsed::Empty, Parsed::Text),
        ItemKind::ToolCall => Parsed::ToolCall(tool_call(json)),
        ItemKind::ToolResult => Parsed::ToolResult(tool_result(json)),
        ItemKind::Question => question::parse(json).map_or(Parsed::Empty, Parsed::Question),
        ItemKind::TurnEnd => Parsed::TurnEnd(turn_end(json)),
        ItemKind::TurnStart => Parsed::Empty,
    }
}

/// The sender's session name in a `PeerMessage` body.
pub fn peer_from(json: &str) -> String {
    string_field(json, "from").unwrap_or_default()
}

/// The `text` of a message-like body (a bare JSON string counts as its own text).
pub fn text(json: &str) -> Option<String> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::String(s)) => Some(s),
        Ok(v) => v.get("text")?.as_str().map(str::to_owned),
        Err(_) => string_field(json, "text"),
    }
}

fn tool_call(json: &str) -> ToolCall {
    match serde_json::from_str::<Value>(json) {
        Ok(v) => ToolCall {
            name: v["name"].as_str().unwrap_or("tool").to_owned(),
            input: v.get("input").cloned(),
            task: v["task"].as_str().map(str::to_owned),
            push: v["push"].as_str().map(str::to_owned),
        },
        // Cut inside the input (keys are sorted, so `input` comes before `name`): the name is gone
        // with the tail; the caller falls back to the item's preview.
        Err(_) => ToolCall {
            name: string_field(json, "name").unwrap_or_default(),
            input: None,
            task: None,
            push: None,
        },
    }
}

fn tool_result(json: &str) -> ToolResult {
    match serde_json::from_str::<Value>(json) {
        Ok(v) => ToolResult {
            output: v["output"].as_str().unwrap_or("").to_owned(),
            is_error: v["is_error"].as_bool().unwrap_or(false),
            link: artifact_link(&v["link"]),
        },
        Err(_) => ToolResult {
            output: string_field(json, "output").unwrap_or_default(),
            is_error: json.contains("\"is_error\":true"),
            // keys are sorted, so a cut body (in `output`) still holds the whole `link`, a
            // flat object
            link: json
                .split_once("\"link\":")
                .and_then(|(_, r)| Some(&r[..=r.find('}')?]))
                .and_then(|o| serde_json::from_str::<Value>(o).ok())
                .and_then(|v| artifact_link(&v)),
        },
    }
}

fn artifact_link(v: &Value) -> Option<ArtifactLink> {
    Some(ArtifactLink {
        url: v["url"].as_str()?.to_owned(),
        title: v["title"].as_str().unwrap_or("page").to_owned(),
        version: v["version"].as_u64(),
        updated: v["updated"].as_bool() == Some(true),
    })
}

fn turn_end(json: &str) -> TurnEnd {
    let v = serde_json::from_str::<Value>(json).unwrap_or(Value::Null);
    let stop = match &v["stop"] {
        Value::String(s) => match s.as_str() {
            "EndTurn" => Stop::EndTurn,
            "MaxTokens" => Stop::MaxTokens,
            "Interrupted" => Stop::Interrupted,
            other => Stop::Other(other.to_owned()),
        },
        Value::Object(o) => match o.iter().next() {
            Some((k, Value::String(m))) if k == "Error" => Stop::Error(m.clone()),
            Some((_, Value::String(m))) => Stop::Other(m.clone()),
            _ => Stop::EndTurn,
        },
        _ => Stop::EndTurn,
    };
    TurnEnd {
        stop,
        duration_ms: v["duration_ms"].as_u64(),
    }
}

/// The string value of `key` in `json`, read without a JSON parser so a text cut anywhere still
/// gives what there is: up to the closing quote, or to the end of the input.
fn string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(i) = json[from..].find(&needle) {
        let after = from + i + needle.len();
        if let Some(value) = json[after..]
            .trim_start()
            .strip_prefix(':')
            .and_then(|r| r.trim_start().strip_prefix('"'))
        {
            return Some(unescape(value));
        }
        from = after;
    }
    None
}

/// Decodes a JSON string body up to its closing quote (or the end of the input, when cut).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(1 << 16));
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let hex = |chars: &mut std::str::Chars| -> Option<u32> {
                        let h: String = chars.take(4).collect();
                        u32::from_str_radix(&h, 16).ok().filter(|_| h.len() == 4)
                    };
                    let Some(hi) = hex(&mut chars) else { break };
                    let c = if (0xD800..0xDC00).contains(&hi) {
                        // A surrogate pair: the low half follows as `\uXXXX`.
                        let mut rest = chars.clone();
                        let low = (rest.next() == Some('\\') && rest.next() == Some('u'))
                            .then(|| hex(&mut rest))
                            .flatten()
                            .filter(|l| (0xDC00..0xE000).contains(l));
                        match low {
                            Some(lo) => {
                                chars = rest;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            }
                            None => None,
                        }
                    } else {
                        char::from_u32(hi)
                    };
                    out.push(c.unwrap_or('\u{FFFD}'));
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_bodies_read_their_text() {
        let body = json!({"text": "héllo\n\"world\""}).to_string();
        assert_eq!(
            parse(ItemKind::AssistantMessage, Some(&body)),
            Parsed::Text("héllo\n\"world\"".into())
        );
        assert_eq!(parse(ItemKind::Thinking, None), Parsed::Empty);
        assert_eq!(
            parse(ItemKind::Event, Some("\"bare string\"")),
            Parsed::Text("bare string".into())
        );
        assert_eq!(parse(ItemKind::Plan, Some("{}")), Parsed::Empty);
    }

    #[test]
    fn a_cut_body_still_gives_its_text() {
        let full = json!({"text": format!("a\\b \"q\" é 😀 {}", "x".repeat(50))}).to_string();
        // Cut inside the string, ending with the daemon's marker as a blob over the cap does.
        let cut = format!("{}…[truncated]", &full[..full.len() - 20]);
        assert!(serde_json::from_str::<Value>(&cut).is_err());
        let Parsed::Text(t) = parse(ItemKind::AssistantMessage, Some(&cut)) else {
            panic!("no text")
        };
        assert!(t.starts_with("a\\b \"q\" é 😀 xxx"), "{t}");
        assert!(t.ends_with("…[truncated]"), "{t}");
    }

    #[test]
    fn unescape_handles_every_escape_and_stops_at_the_quote() {
        assert_eq!(unescape(r#"a\n\t\r\\\/\"é😀" tail"#), "a\n\t\r\\/\"é😀");
        // Cut in the middle of an escape: what came before stays.
        assert_eq!(unescape(r"abc\u00"), "abc");
        assert_eq!(unescape(r"abc\"), "abc");
        // A lone surrogate is replaced, not a panic.
        assert_eq!(unescape(r"x\ud83dy"), "x\u{FFFD}y");
    }

    #[test]
    fn tool_calls_name_what_they_run() {
        let b =
            json!({"name":"Bash","input":{"command":"ls -la\nsecond"},"call_id":"t1"}).to_string();
        let Parsed::ToolCall(c) = parse(ItemKind::ToolCall, Some(&b)) else {
            panic!()
        };
        assert_eq!(c.name, "Bash");
        assert_eq!(c.hint(), "ls -la");
        assert_eq!(c.input_text().as_deref(), Some("ls -la\nsecond"));

        let b =
            json!({"name":"Read","input":{"file_path":"/etc/hostname","limit":5},"call_id":"t2"})
                .to_string();
        let Parsed::ToolCall(c) = parse(ItemKind::ToolCall, Some(&b)) else {
            panic!()
        };
        assert_eq!(c.hint(), "/etc/hostname");
        assert!(c.input_text().unwrap().contains("\"limit\": 5"));

        // No input yet (`Null`) or an empty object: nothing to show.
        let b = json!({"name":"Bash","input":null,"call_id":"t3"}).to_string();
        let Parsed::ToolCall(c) = parse(ItemKind::ToolCall, Some(&b)) else {
            panic!()
        };
        assert_eq!((c.hint().as_str(), c.input_text()), ("", None));
    }

    #[test]
    fn tool_results_keep_error_and_output_even_when_cut() {
        let b = json!({"call_id":"t1","output":"hi\n","is_error":true}).to_string();
        assert_eq!(
            parse(ItemKind::ToolResult, Some(&b)),
            Parsed::ToolResult(ToolResult {
                output: "hi\n".into(),
                is_error: true,
                link: None,
            })
        );
        // an Artifact result's link survives a cut output (keys sorted: link before output)
        let b = json!({"call_id":"t1","is_error":false,
            "link":{"url":"https://claude.ai/artifact/AbC","title":"T","version":2,"updated":true},
            "output":"Published …"})
        .to_string();
        let cut = &b[..b.find("Published").unwrap() + 4];
        for j in [b.as_str(), cut] {
            let Parsed::ToolResult(r) = parse(ItemKind::ToolResult, Some(j)) else {
                panic!()
            };
            let l = r.link.unwrap();
            assert_eq!(
                (l.url.as_str(), l.title.as_str(), l.version, l.updated),
                ("https://claude.ai/artifact/AbC", "T", Some(2), true)
            );
        }
        // Keys are sorted: call_id, is_error, output. A cut lands in the output.
        let cut = r#"{"call_id":"t1","is_error":true,"output":"line 1\nline 2\nli…[truncated]"#;
        let Parsed::ToolResult(r) = parse(ItemKind::ToolResult, Some(cut)) else {
            panic!()
        };
        assert!(r.is_error);
        assert_eq!(r.output, "line 1\nline 2\nli…[truncated]");
        // A call cut inside its input has lost its name: the caller uses the preview.
        let cut = r#"{"call_id":"t1","input":{"content":"aaaa…[truncated]"#;
        let Parsed::ToolCall(c) = parse(ItemKind::ToolCall, Some(cut)) else {
            panic!()
        };
        assert_eq!((c.name.as_str(), c.input), ("", None));
    }

    #[test]
    fn turn_ends_say_why_and_how_long() {
        let end = |v: Value| match parse(ItemKind::TurnEnd, Some(&v.to_string())) {
            Parsed::TurnEnd(t) => t,
            p => panic!("{p:?}"),
        };
        assert_eq!(
            end(json!({"stop":"EndTurn","usage":null,"duration_ms":1500})),
            TurnEnd {
                stop: Stop::EndTurn,
                duration_ms: Some(1500)
            }
        );
        assert_eq!(end(json!({"stop":"Interrupted"})).stop, Stop::Interrupted);
        assert_eq!(end(json!({"stop":"MaxTokens"})).stop, Stop::MaxTokens);
        assert_eq!(
            end(json!({"stop":{"Error":"rate limited"},"duration_ms":null})),
            TurnEnd {
                stop: Stop::Error("rate limited".into()),
                duration_ms: None
            }
        );
        assert_eq!(
            end(json!({"stop":{"Other":"refusal"}})).stop,
            Stop::Other("refusal".into())
        );
        // Garbage reads as a normal end rather than hiding the row.
        assert_eq!(end(json!(null)).stop, Stop::EndTurn);
    }
}
