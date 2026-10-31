//! Typed agent-to-agent messages: what a message is (`kind`) and how fast the
//! recipient must see it (`urgency`). Both ride in the event payload, so they
//! need no schema of their own and old events without them read as plain,
//! normal-urgency messages.

use serde_json::{Map, Value};

pub const KINDS: &[&str] = &["note", "question", "status", "handoff_note", "blocker", "result", "interrupt"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    Urgent,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::Urgent => "urgent",
        }
    }
}

/// Validated kind and urgency from optional tool arguments. An `interrupt`
/// is urgent by definition; a blocker is only urgent when the sender says so.
pub fn parse(kind: Option<&str>, urgency: Option<&str>) -> Result<(Option<&'static str>, Urgency), String> {
    let kind = match kind.map(str::trim).filter(|k| !k.is_empty()) {
        None => None,
        Some(k) => Some(
            KINDS
                .iter()
                .find(|known| **known == k)
                .copied()
                .ok_or_else(|| format!("unknown kind `{k}`; use one of {}", KINDS.join(", ")))?,
        ),
    };
    let urgency = match urgency.map(str::trim).filter(|u| !u.is_empty()) {
        None | Some("normal") => Urgency::Normal,
        Some("urgent") => Urgency::Urgent,
        Some(other) => return Err(format!("unknown urgency `{other}`; use normal or urgent")),
    };
    let urgency = if kind == Some("interrupt") { Urgency::Urgent } else { urgency };
    Ok((kind, urgency))
}

/// Payload fields for a typed message; empty for an untyped normal one.
pub fn payload_fields(kind: Option<&str>, urgency: Urgency, task_id: Option<&str>) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(kind) = kind {
        map.insert("kind".into(), Value::from(kind));
    }
    if urgency == Urgency::Urgent {
        map.insert("urgency".into(), Value::from("urgent"));
    }
    if let Some(task) = task_id.filter(|t| !t.trim().is_empty()) {
        map.insert("task_id".into(), Value::from(task));
    }
    map
}

pub fn urgency_of(payload: &Value) -> Urgency {
    match payload.get("urgency").and_then(Value::as_str) {
        Some("urgent") => Urgency::Urgent,
        _ => Urgency::Normal,
    }
}

/// Envelope lines for a typed message, empty when it is untyped and normal.
pub fn envelope_lines(payload: &Value) -> String {
    let mut out = String::new();
    if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
        out.push_str(&format!("kind: {kind}\n"));
    }
    if urgency_of(payload) == Urgency::Urgent {
        out.push_str("urgency: urgent (the sender interrupted your run for this; deal with it first)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_is_always_urgent() {
        assert_eq!(parse(Some("interrupt"), None).unwrap(), (Some("interrupt"), Urgency::Urgent));
    }

    #[test]
    fn untyped_is_normal_and_bad_values_are_refused() {
        assert_eq!(parse(None, None).unwrap(), (None, Urgency::Normal));
        assert!(parse(Some("shout"), None).is_err());
        assert!(parse(None, Some("asap")).is_err());
    }

    #[test]
    fn payload_round_trips() {
        let fields = payload_fields(Some("blocker"), Urgency::Urgent, Some("t1"));
        let payload = Value::Object(fields);
        assert_eq!(urgency_of(&payload), Urgency::Urgent);
        assert!(envelope_lines(&payload).contains("kind: blocker"));
        assert_eq!(envelope_lines(&serde_json::json!({"body": "x"})), "");
    }
}
