use serde::Serialize;

const LADDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Effort,
    Model,
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSpec {
    pub control: Control,
    pub label: &'static str,
    pub options: Vec<&'static str>,
    pub can_disable: bool,
    pub visible: bool,
    pub note: String,
}

impl ReasoningSpec {
    fn effort(label: &'static str, options: &[&'static str], can_disable: bool, visible: bool, note: impl Into<String>) -> Self {
        Self {
            control: Control::Effort,
            label,
            options: options.to_vec(),
            can_disable,
            visible,
            note: note.into(),
        }
    }

    fn fixed(control: Control, label: &'static str, visible: bool, note: impl Into<String>) -> Self {
        Self {
            control,
            label,
            options: Vec::new(),
            can_disable: false,
            visible,
            note: note.into(),
        }
    }
}

fn gpt_version(model: &str) -> Option<f32> {
    let rest = model.split("gpt-").nth(1)?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.trim_end_matches('.').parse().ok()
}

fn openai_reasoning_model(model: &str) -> bool {
    let bytes = model.as_bytes();
    (bytes.first() == Some(&b'o') && bytes.get(1).is_some_and(u8::is_ascii_digit))
        || gpt_version(model).is_some_and(|v| v >= 5.0)
        || model.contains("codex")
}

pub fn spec(provider: &str, model: &str) -> ReasoningSpec {
    let m = model.trim().to_ascii_lowercase();
    match provider {
        "claude-code" => ReasoningSpec::effort(
            "Thinking",
            &LADDER,
            false,
            true,
            "Sent to Claude Code as --effort. Default lets Claude Code decide.",
        ),
        "antigravity" => ReasoningSpec::fixed(
            Control::Model,
            "Thinking",
            false,
            "Antigravity sets effort through the model: pick a -low, -medium or -high variant. It doesn't stream its thinking.",
        ),
        "anthropic" | "anthropic-compatible" => {
            let legacy = m.starts_with("claude-3-") && !m.starts_with("claude-3-7");
            if legacy {
                ReasoningSpec::fixed(Control::None, "Thinking", false, "This model has no extended thinking.")
            } else {
                ReasoningSpec::effort(
                    "Thinking",
                    &LADDER,
                    true,
                    true,
                    "Extended thinking budget, from about 2k tokens (Low) to 64k (Max). Off turns thinking off.",
                )
            }
        }
        "gemini" => {
            if m.contains("gemini-3") || m.contains("gemini3") {
                ReasoningSpec::effort(
                    "Thinking",
                    &["low", "high"],
                    false,
                    true,
                    "Gemini 3 thinks at a low or high level and can't be switched off. Default is dynamic.",
                )
            } else if m.contains("2.5") || m.contains("2-5") {
                ReasoningSpec::effort(
                    "Thinking",
                    &["low", "medium", "high", "max"],
                    m.contains("flash"),
                    true,
                    "Gemini 2.5 thinking budget, from 2k tokens (Low) to 32k (Max). Default is dynamic.",
                )
            } else {
                ReasoningSpec::fixed(Control::None, "Thinking", false, "This model doesn't think.")
            }
        }
        "chatgpt" => {
            let version = gpt_version(&m).unwrap_or(0.0);
            let options: &[&'static str] = if version >= 5.6 {
                &LADDER
            } else if m.contains("codex-max") || version >= 5.2 {
                &LADDER[..4]
            } else {
                &LADDER[..3]
            };
            ReasoningSpec::effort(
                "Reasoning",
                options,
                false,
                true,
                "Reasoning effort for the Codex backend; its reasoning summary streams as thinking.",
            )
        }
        "openai" => {
            if openai_reasoning_model(&m) {
                ReasoningSpec::effort(
                    "Reasoning",
                    &LADDER[..4],
                    false,
                    false,
                    "OpenAI reasoning effort. The API keeps the reasoning itself private, so nothing streams.",
                )
            } else {
                ReasoningSpec::fixed(Control::None, "Reasoning", false, "This model doesn't reason.")
            }
        }
        "xai" | "grok" => {
            if m.contains("mini") {
                ReasoningSpec::effort("Reasoning", &["low", "high"], false, true, "Grok mini reasons at a low or high level.")
            } else {
                ReasoningSpec::effort(
                    "Reasoning",
                    &LADDER[..3],
                    false,
                    true,
                    "Grok reasoning effort. If the model fixes its own, snippet steps back to the default.",
                )
            }
        }
        _ => ReasoningSpec::effort(
            "Reasoning",
            &LADDER,
            false,
            true,
            "Passed through to the endpoint. If the model rejects a tier, snippet steps down until it's accepted.",
        ),
    }
}

pub fn effective(provider: &str, model: &str, stored: Option<&str>) -> Option<String> {
    let stored = stored.map(str::trim).filter(|s| !s.is_empty())?.to_ascii_lowercase();
    let spec = spec(provider, model);
    if spec.control != Control::Effort {
        return None;
    }
    if stored == "off" || stored == "none" || stored == "minimal" {
        return spec.can_disable.then(|| "off".to_string());
    }
    if spec.options.contains(&stored.as_str()) {
        return Some(stored);
    }
    let rank = LADDER.iter().position(|t| *t == stored)?;
    LADDER[..=rank]
        .iter()
        .rev()
        .find(|t| spec.options.contains(t))
        .or(spec.options.first())
        .map(|t| t.to_string())
}
