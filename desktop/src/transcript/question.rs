//! A question an agent asks the user, as the item body carries it (`femme-agents` `QuestionItem`:
//! the `Question` fields flattened, plus the `answered` answer and a `reason` when it ended
//! unanswered), and how its answer reads.

use femme_proto::rpc::Answer;
use femme_proto::state::Question;
use serde_json::Value;

/// The body of a `Question` item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionBody {
    /// The question: what to ask and which answers it takes.
    pub q: Question,
    /// The answer, once the user gave one.
    pub answered: Option<Answer>,
    /// Why the question ended without an answer (the turn was interrupted, the agent exited).
    pub reason: Option<String>,
}

/// Reads the body of a `Question` item; `None` when it carries no usable question.
pub fn parse(json: &str) -> Option<QuestionBody> {
    let v: Value = serde_json::from_str(json).ok()?;
    let q: Question = serde_json::from_value(v.clone()).ok()?;
    let answered = v
        .get("answered")
        .filter(|a| !a.is_null())
        .and_then(|a| serde_json::from_value::<Answer>(a.clone()).ok());
    let reason = v.get("reason").and_then(Value::as_str).map(str::to_owned);
    Some(QuestionBody {
        q,
        answered,
        reason,
    })
}

impl QuestionBody {
    /// The answer in words: the option's label (or the labels, joined), the free text, or
    /// "cancelled". Indices past the options are shown as their number.
    pub fn answer_text(&self) -> Option<String> {
        let label = |i: u16| {
            self.q
                .options
                .get(usize::from(i))
                .cloned()
                .unwrap_or_else(|| format!("#{}", u32::from(i) + 1))
        };
        Some(match self.answered.as_ref()? {
            Answer::Option(i) => label(*i),
            Answer::Options(v) => v.iter().map(|i| label(*i)).collect::<Vec<_>>().join(", "),
            Answer::Text(t) => t.clone(),
            Answer::Cancel => "cancelled".to_owned(),
        })
    }

    /// Whether the user can still answer: no answer is recorded and the question did not end
    /// without one.
    pub fn open(&self) -> bool {
        self.answered.is_none() && self.reason.is_none()
    }
}

/// The picks of a multiple-choice question as an answer, in option order.
pub fn picks(selected: &[bool]) -> Answer {
    Answer::Options(
        selected
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(i, _)| u16::try_from(i).unwrap_or(u16::MAX))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(extra: Value) -> String {
        let mut v = json!({
            "request_id": "r1",
            "prompt": "Which?",
            "options": ["alpha", "beta", "gamma"],
            "multi": false,
            "free_text": true,
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v.to_string()
    }

    #[test]
    fn a_pending_question_is_open() {
        let q = parse(&body(json!({"answered": null}))).unwrap();
        assert_eq!(q.q.request_id, "r1");
        assert_eq!(q.q.prompt, "Which?");
        assert_eq!(q.q.options, ["alpha", "beta", "gamma"]);
        assert!(q.q.free_text && !q.q.multi);
        assert!(q.open());
        assert_eq!(q.answer_text(), None);
        // `answered` and `reason` are additive: an old body has neither.
        assert!(parse(&body(json!({}))).unwrap().open());
    }

    #[test]
    fn answers_read_as_labels_text_or_cancel() {
        let answered = |a: Value| parse(&body(json!({ "answered": a }))).unwrap();
        let q = answered(json!({"Option": 1}));
        assert!(!q.open());
        assert_eq!(q.answer_text().as_deref(), Some("beta"));
        assert_eq!(
            answered(json!({"Options": [0, 2]}))
                .answer_text()
                .as_deref(),
            Some("alpha, gamma")
        );
        assert_eq!(
            answered(json!({"Text": "something else"}))
                .answer_text()
                .as_deref(),
            Some("something else")
        );
        assert_eq!(
            answered(json!("Cancel")).answer_text().as_deref(),
            Some("cancelled")
        );
        // An index the options do not have (the question changed under us) is shown by number.
        assert_eq!(
            answered(json!({"Option": 7})).answer_text().as_deref(),
            Some("#8")
        );
    }

    #[test]
    fn a_question_that_failed_unanswered_is_closed_with_its_reason() {
        let q = parse(&body(json!({"answered": null, "reason": "interrupted"}))).unwrap();
        assert!(!q.open());
        assert_eq!(q.reason.as_deref(), Some("interrupted"));
        assert_eq!(q.answer_text(), None);
    }

    #[test]
    fn garbage_is_no_question() {
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("{\"prompt\":\"no id\"}"), None);
        assert_eq!(parse("{\"request_id\":\"r\",\"prompt\":\"p\"}"), None);
    }

    #[test]
    fn picks_are_the_ticked_options_in_order() {
        assert_eq!(picks(&[true, false, true]), Answer::Options(vec![0, 2]));
        assert_eq!(picks(&[false, false]), Answer::Options(vec![]));
    }
}
