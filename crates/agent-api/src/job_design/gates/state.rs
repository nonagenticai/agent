//! The `state` Laya reads at each gate point (plan T9 step 2), at most [`STATE_MAX_CHARS`] characters of text:
//! - P1 (request): the last [`P1_LOG_LINES`] log lines and the latest customer text, one per line (a string).
//! - P2 (tool): `{utterance, op: {key, args}}`.
//! - R1 (response): `{reply, operations: [{op, outcome}]}`.
//!
//! Text is cut from the front (the newest words are kept). The state is customer data: never logged.
use serde_json::{Value, json};

/// Laya's `state` budget, in characters.
pub const STATE_MAX_CHARS: usize = 20_000;
/// P1 reads this many log lines before the latest customer text.
pub const P1_LOG_LINES: usize = 4;
/// R1 lists at most this many operations of the turn (the newest).
pub const R1_MAX_OPERATIONS: usize = 20;

/// The last `max` characters of `s`.
pub fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    s.chars().skip(n - max).collect()
}

/// P1: the last [`P1_LOG_LINES`] of `log` (oldest first), then the latest customer text.
pub fn request_state<S: AsRef<str>>(log: &[S], latest: &str) -> Value {
    let from = log.len().saturating_sub(P1_LOG_LINES);
    let mut lines: Vec<&str> = log[from..].iter().map(AsRef::as_ref).collect();
    lines.push(latest);
    Value::String(tail(&lines.join("\n"), STATE_MAX_CHARS))
}

/// P2: the utterance the tool call answers and the call (`op` key and its arguments). Arguments that would take
/// more than half the budget are left out (`null`); the utterance fills what is left.
pub fn tool_state(utterance: &str, op_key: &str, args: Option<&Value>) -> Value {
    let mut op = json!({"key": tail(op_key, 200), "args": args.cloned().unwrap_or(Value::Null)});
    if op.to_string().chars().count() > STATE_MAX_CHARS / 2 {
        op["args"] = Value::Null;
    }
    let left = STATE_MAX_CHARS.saturating_sub(op.to_string().chars().count());
    json!({"utterance": tail(utterance, left), "op": op})
}

/// R1: the draft reply and the operations of the turn with their outcome (`ok`, `failed`, `refused`, …), newest
/// [`R1_MAX_OPERATIONS`] only.
pub fn response_state<S: AsRef<str>>(reply: &str, operations: &[(S, S)]) -> Value {
    let from = operations.len().saturating_sub(R1_MAX_OPERATIONS);
    let ops: Vec<Value> = operations[from..]
        .iter()
        .map(|(op, outcome)| json!({"op": tail(op.as_ref(), 200), "outcome": tail(outcome.as_ref(), 40)}))
        .collect();
    let ops = Value::Array(ops);
    let left = STATE_MAX_CHARS.saturating_sub(ops.to_string().chars().count());
    json!({"reply": tail(reply, left), "operations": ops})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p1_keeps_the_last_lines_and_the_latest_text() {
        let log = ["a", "b", "c", "d", "e"];
        assert_eq!(request_state(&log, "now"), json!("b\nc\nd\ne\nnow"));
        let empty: [&str; 0] = [];
        assert_eq!(request_state(&empty, "only"), json!("only"));
        let long = "x".repeat(STATE_MAX_CHARS + 10);
        let s = request_state(&["old"], &format!("{long}END"));
        let s = s.as_str().unwrap();
        assert_eq!(s.chars().count(), STATE_MAX_CHARS);
        assert!(s.ends_with("END"));
    }

    #[test]
    fn p2_and_r1_shapes_fit_the_budget() {
        let args = json!({"order": "A-1"});
        let v = tool_state("refund it", "refund.issue", Some(&args));
        assert_eq!(
            v,
            json!({"utterance": "refund it", "op": {"key": "refund.issue", "args": {"order": "A-1"}}})
        );
        let huge = json!({"blob": "y".repeat(STATE_MAX_CHARS)});
        let v = tool_state(&"u".repeat(STATE_MAX_CHARS * 2), "a.b", Some(&huge));
        assert_eq!(v["op"]["args"], Value::Null);
        assert!(v.to_string().chars().count() <= STATE_MAX_CHARS + 64);
        let v = response_state("Your refund is on its way.", &[("refund.issue", "ok")]);
        assert_eq!(
            v,
            json!({"reply": "Your refund is on its way.", "operations": [{"op": "refund.issue", "outcome": "ok"}]})
        );
    }
}
