use super::*;

/// Soft budget of turns per request. Not a hard stop: crossing it only raises a
/// pacing reminder so the agent converges. Ignored under an active goal, which
/// is long-horizon by design.
const TURN_BUDGET: u64 = 70;

/// The harness note for this step, or `None` when nothing changed. Each section
/// of volatile state (working directory, background processes, delegated work,
/// watches, …) is reported only when it differs from what the model was last
/// told, and one-shot signals ride along when raised. The note is stored in the
/// history, so the model keeps seeing what it was told without it being resent.
pub(super) fn build_reminder(
    state: &HarnessState,
    vars: &mut LoopVars,
    workspace: &std::path::Path,
    cwd: &std::path::Path,
    browser_summary: Option<String>,
) -> Option<String> {
    if vars.reminded_compactions != state.compactions {
        vars.reminded.clear();
        vars.reminded_compactions = state.compactions;
    }
    let mut lines: Vec<String> = Vec::new();
    for (key, current, cleared) in sections(state, workspace, cwd, browser_summary) {
        let previous = vars.reminded.get(key).cloned().unwrap_or_default();
        if current == previous {
            continue;
        }
        if current.is_empty() {
            if let Some(cleared) = cleared {
                lines.push(cleared.to_string());
            }
        } else {
            lines.push(current.clone());
        }
        vars.reminded.insert(key, current);
    }

    let mut signals: Vec<String> = std::mem::take(&mut vars.pending_signals)
        .iter()
        .map(RuntimeSignal::message)
        .collect();
    let goal_active = matches!(&state.goal, Some(g) if g.status == GoalStatus::Active);
    let n = vars.turns_this_request;
    if !goal_active {
        if n + 5 == TURN_BUDGET {
            signals.push(format!(
                "This request has taken {n} steps. Start converging on the result."
            ));
        } else if n >= TURN_BUDGET && (n - TURN_BUDGET) % 10 == 0 {
            signals.push(format!(
                "This request has taken {n} steps. Wrap up: deliver your best current result and say what is left."
            ));
        }
    }
    lines.extend(signals);

    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n\n"))
}

/// (key, current text, what to say once it becomes empty).
fn sections(
    state: &HarnessState,
    workspace: &std::path::Path,
    cwd: &std::path::Path,
    browser_summary: Option<String>,
) -> Vec<(&'static str, String, Option<&'static str>)> {
    let mut out = Vec::new();
    out.push(("cwd", format!("Working directory: {}", compact_path(cwd)), None));

    let untitled = state
        .title
        .as_deref()
        .map(sanitize_one_line)
        .is_none_or(|title| title.is_empty());
    out.push((
        "title",
        if untitled {
            "This session has no title yet.".to_string()
        } else {
            String::new()
        },
        None,
    ));

    if workspace == crate::mission_control::workspace_path() {
        out.push(("autonomy", crate::mission_autonomy::mode_line(), None));
        out.push(("clock", crate::mission_autonomy::clock_line(), None));
    }

    let browser = browser_summary
        .filter(|summary| crate::session::browser_summary_is_connected(summary))
        .unwrap_or_default();
    out.push(("browser", browser, Some("No browser is connected any more.")));

    let vault_names = crate::vault::Vault::load().names();
    out.push((
        "vault",
        if vault_names.is_empty() {
            String::new()
        } else {
            format!(
                "Vault secrets usable as $NAME in bash (values are hidden from you): {}",
                vault_names.join(", ")
            )
        },
        Some("The vault is empty now."),
    ));

    let safety = latest_user_input(state)
        .map(|latest| derive_input_safety_signals(&latest))
        .unwrap_or_default();
    out.push((
        "input_safety",
        if safety.is_empty() {
            String::new()
        } else {
            format!(
                "Heuristic flags on the user's latest message (weigh them, don't quote them): {}",
                safety.join("; ")
            )
        },
        None,
    ));

    out.push((
        "background",
        crate::bg::render_live(workspace)
            .map(|bg| format!("Background processes you started:\n{}", bg.trim_end()))
            .unwrap_or_default(),
        Some("No background processes are running now."),
    ));

    out.push(("plan", render_plan(&state.plan), None));

    out.push(("lanes", render_lanes(state), Some("All delegated work has reported back.")));

    let watches = if state.watches.is_empty() {
        String::new()
    } else {
        let mut text = String::from(
            "Active file watches (end your turn to wait; a matching line wakes you; remove each once it has served its purpose):",
        );
        for w in &state.watches {
            text.push_str(&format!(
                "\n- \"{}\" on {} (filter: \"{}\", id: {})",
                clip(&w.label, 32),
                compact_path(std::path::Path::new(&w.path)),
                w.filter.as_deref().unwrap_or("none"),
                w.id
            ));
        }
        text
    };
    out.push(("watches", watches, Some("No file watches are active now.")));
    out
}

/// The plan as the model is reminded of it (after compaction or at the start
/// of a request); empty when there is none.
pub(super) fn render_plan(plan: &[PlanStep]) -> String {
    if plan.is_empty() {
        return String::new();
    }
    let mut text = String::from("Your current plan (keep it current with update_plan):");
    for step in plan {
        let mark = match step.status {
            PlanStatus::Done => "[x]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
        };
        text.push_str(&format!("\n{mark} {}", step.step));
    }
    text
}

fn render_lanes(state: &HarnessState) -> String {
    let running: Vec<&LaneRecord> = state
        .lanes
        .iter()
        .filter(|l| l.status == LaneStatus::Running)
        .collect();
    if running.is_empty() {
        return String::new();
    }
    let tags = |l: &LaneRecord| {
        let mut tags = String::new();
        if let Some(a) = &l.agent {
            tags.push_str(&format!(" [agent: {a}]"));
        }
        if let Some(p) = &l.profile {
            tags.push_str(&format!(" [profile: {p}]"));
        }
        tags
    };
    let mut text = format!(
        "Delegated work still running ({}); end your turn to wait, each report wakes you:",
        running.len()
    );
    for l in &running {
        text.push_str(&format!("\n- \"{}\"{} ({})", clip(&l.title, 40), tags(l), l.id));
    }
    let finished: Vec<&LaneRecord> = state
        .lanes
        .iter()
        .rev()
        .filter(|l| l.status != LaneStatus::Running)
        .take(6)
        .collect();
    for l in finished {
        let status = match l.status {
            LaneStatus::Completed => "completed",
            LaneStatus::Failed => "failed",
            LaneStatus::Cancelled => "cancelled",
            LaneStatus::Running => unreachable!("filtered above"),
        };
        text.push_str(&format!("\n- \"{}\"{} — {status} ({})", clip(&l.title, 40), tags(l), l.id));
    }
    text
}

/// Flag the latest user message for prompt-injection / exfiltration / secret /
/// destructive phrasing (capped at 6).
pub(super) fn derive_input_safety_signals(input: &str) -> Vec<String> {
    let input_lower = input.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut signals = Vec::new();

    let pattern_checks: [(&str, &str, &[&str]); 5] = [
        (
            "instruction_override",
            "attempt to override system rules detected",
            &[
                "ignore previous instructions",
                "disregard prior instructions",
                "forget all rules",
                "override system prompt",
            ],
        ),
        (
            "prompt_exfiltration",
            "attempt to reveal hidden prompts or internal policy detected",
            &[
                "show system prompt",
                "reveal your prompt",
                "print your instructions",
                "developer instructions",
            ],
        ),
        (
            "safety_bypass",
            "attempt to bypass safety constraints detected",
            &[
                "disable safety",
                "jailbreak",
                "bypass policy",
                "no restrictions",
            ],
        ),
        (
            "secret_exfiltration",
            "request may involve secrets, credentials, or token exfiltration",
            &[
                "api key",
                "access token",
                "password",
                "private key",
                "secret",
            ],
        ),
        (
            "destructive_operations",
            "potential destructive operation request detected",
            &[
                "drop database",
                "delete all",
                "rm -rf",
                "truncate table",
                "wipe",
            ],
        ),
    ];

    for (tag, message, phrases) in pattern_checks {
        if phrases.iter().any(|phrase| input_lower.contains(phrase)) && seen.insert(tag) {
            signals.push(format!("{tag} = \"{message}\""));
        }
        if signals.len() >= 6 {
            break;
        }
    }

    signals
}

pub(super) fn sanitize_one_line(text: &str) -> String {
    let collapsed = text.replace('\n', " ").replace('"', "'");
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most recent user-originated text (a fresh request or a mid-run steer).
pub(super) fn latest_user_input(state: &HarnessState) -> Option<String> {
    state.events.iter().rev().find_map(|event| match event {
        HarnessEvent::UserInput { text } | HarnessEvent::Steer { text } => Some(text.clone()),
        _ => None,
    })
}

pub(super) fn first_question_text(rendered: &Value) -> Option<String> {
    rendered
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| question.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
