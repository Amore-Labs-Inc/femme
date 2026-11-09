//! JSON bodies of transcript items, one struct per [`femme_proto::state::ItemKind`]. The
//! drivers put these (via [`body`]) into `ItemCompleted`/`ItemUpdated`; the daemon stores the
//! JSON as `Item.body`; the desktop and the phone board render from it. Keep fields additive:
//! new ones are optional with `#[serde(default)]`.

use femme_proto::rpc::Answer;
use femme_proto::state::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StopReason;

/// Serialize a body struct to the `Value` the events carry.
pub fn body<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// `ItemKind::AssistantMessage`: the agent's text (markdown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssistantMessage {
    /// The whole message so far (final on completion).
    pub text: String,
}

/// `ItemKind::Thinking`: reasoning text or a summary of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Thinking {
    /// The reasoning text.
    pub text: String,
}

/// `ItemKind::ToolCall`: the agent invoked a tool. The result is a separate `ToolResult` item
/// whose `call_id` equals this one's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name as the agent calls it (`Bash`, `Edit`, `commandExecution`, ...).
    pub name: String,
    /// The tool's input, as the agent sent it. `Null` until the streamed input is complete.
    pub input: Value,
    /// The agent's id for this call (Claude `tool_use.id`, Codex item id, Pi toolCallId).
    pub call_id: String,
}

/// `ItemKind::ToolResult`: what a tool returned. Id of the item is `<call_id>:result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the `ToolCall` this answers.
    pub call_id: String,
    /// Output flattened to text (non-text blocks become `[image]` style placeholders).
    pub output: String,
    /// The tool reported failure.
    pub is_error: bool,
    /// The page an `Artifact` call published or read, when the result names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<ArtifactLink>,
}

/// A claude.ai artifact page an `Artifact` tool result points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArtifactLink {
    /// `https://claude.ai/artifact/<id>`.
    pub url: String,
    /// The page's title; the file's name or the call's action when the result does not say.
    pub title: String,
    /// The version number of a publish (its `seq`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The publish changed an existing page rather than making a new one.
    #[serde(default)]
    pub updated: bool,
}

/// `ItemKind::Question`: a question for the user, plus its answer once given.
///
/// The item ends `Answered` with the user's own `answered` (a deliberate Cancel or Deny is an
/// answer too), or `Failed` with `answered: None` and a `reason` when nobody answered it: the
/// turn was interrupted, the process exited, or the agent withdrew the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionItem {
    /// The question (`request_id`, `prompt`, `options`, `multi`, `free_text`), flattened.
    #[serde(flatten)]
    pub q: femme_proto::state::Question,
    /// The answer, once the user gave one.
    pub answered: Option<Answer>,
    /// Why the question ended `Failed` without an answer; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `ItemKind::Plan`: a plan the agent proposes (Claude `ExitPlanMode`, Codex plan item).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    /// The plan, markdown.
    pub text: String,
}

/// `ItemKind::Event`: a status line shown inline (model change, interrupt, rate limit, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Event {
    /// The line.
    pub text: String,
    /// Set when the line says a background task stopped: which call, and how.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskEnd>,
}

/// A background task's end (Claude's `task_notification`): an `Agent` run in the background or a
/// shell command. A client that shows the agent as working while its items arrive takes this
/// as the sign that it stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TaskEnd {
    /// The id of the tool call that started it.
    pub call: String,
    /// `completed`, `failed`, `killed`, `stopped`, ... as the agent said.
    pub status: String,
}

/// `ItemKind::UserMessage`: something the user (or the daemon on their behalf) sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserMessage {
    /// The text.
    pub text: String,
}

/// `ItemKind::TurnEnd`: built by the daemon from `AgentEvent::TurnEnded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    /// Why the turn ended.
    pub stop: StopReason,
    /// Token usage of the turn, if the agent reported it.
    pub usage: Option<Usage>,
    /// Duration as the agent measured it.
    pub duration_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::Question;
    use serde_json::json;

    #[test]
    fn question_item_flattens() {
        let q = QuestionItem {
            q: Question {
                request_id: "r1".into(),
                prompt: "A or B?".into(),
                options: vec!["A".into(), "B".into()],
                multi: false,
                free_text: true,
            },
            answered: Some(Answer::Option(1)),
            reason: None,
        };
        assert_eq!(
            body(&q),
            json!({"request_id":"r1","prompt":"A or B?","options":["A","B"],"multi":false,"free_text":true,"answered":{"Option":1}})
        );
        let back: QuestionItem = serde_json::from_value(body(&q)).unwrap();
        assert_eq!(back, q);
    }

    /// `reason` is additive: absent when there is none (old bodies and answered questions are
    /// unchanged), present on a question that failed unanswered, and old bodies still parse.
    #[test]
    fn question_item_reason_is_additive() {
        let q = femme_proto::state::Question {
            request_id: "r1".into(),
            prompt: "p".into(),
            options: vec![],
            multi: false,
            free_text: true,
        };
        let failed = QuestionItem {
            q,
            answered: None,
            reason: Some("interrupted".into()),
        };
        let v = body(&failed);
        assert_eq!(v["reason"], "interrupted");
        assert_eq!(serde_json::from_value::<QuestionItem>(v).unwrap(), failed);
        let old = json!({"request_id":"r1","prompt":"p","options":[],"multi":false,"free_text":true,"answered":null});
        assert_eq!(
            serde_json::from_value::<QuestionItem>(old).unwrap().reason,
            None
        );
    }

    #[test]
    fn tool_bodies_link_by_call_id() {
        let c = body(&ToolCall {
            name: "Bash".into(),
            input: json!({"command":"echo hi"}),
            call_id: "t1".into(),
        });
        let r = body(&ToolResult {
            call_id: "t1".into(),
            output: "hi\n".into(),
            is_error: false,
            link: None,
        });
        assert_eq!(c["call_id"], r["call_id"]);
    }
}
