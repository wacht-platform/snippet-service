use super::*;

use serde_json::{Value, json};

pub(crate) struct Verdict {
    pub kind: String,
    pub intent: String,
    pub addressed: f64,
    pub judge: &'static str,
}

impl Verdict {
    pub fn kept(&self) -> bool {
        !matches!(self.kind.as_str(), "media" | "background" | "noise") && self.intent != "ignore"
    }

    fn for_assistant(&self) -> bool {
        self.intent == "for_assistant"
    }
}

fn wake_word(text: &str, phrases: &[String]) -> bool {
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(7)
        .map(str::to_string)
        .collect();
    phrases.iter().any(|p| {
        let parts: Vec<&str> = p.split_whitespace().collect();
        !parts.is_empty()
            && words
                .windows(parts.len())
                .any(|w| w.iter().zip(&parts).all(|(a, b)| a == b))
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn rule(text: &str, phrases: &[String]) -> Verdict {
    Verdict {
        kind: "unjudged".into(),
        intent: if wake_word(text, phrases) { "for_assistant".into() } else { "unjudged".into() },
        addressed: if wake_word(text, phrases) { 1.0 } else { 0.0 },
        judge: "rule",
    }
}

fn jev_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_default()
    })
}

async fn jev(key: &str, text: &str, history: &[Value], active: bool, open: bool) -> Result<Verdict, String> {
    let body = json!({
        "model": "jev-latest",
        "state": {
            "utterance": text,
            "recent_history_oldest_first": history,
            "assistant_mode": if active { "active" } else { "passive" },
            "spoken_conversation_open": open,
        },
        "questions": {
            "kind": {
                "type": "choice",
                "instructions": "What kind of speech is the utterance? It comes from a microphone that is always on.",
                "criteria": {
                    "conversation": "People talking to each other or to the assistant: remarks, plans, questions, tasks, decisions, things worth remembering",
                    "media": "Audio from a TV, radio, video, podcast, music or a call playing aloud",
                    "background": "Overheard fragments, filler, or talk clearly unrelated to the user's own life and work",
                    "noise": "Transcription junk, single stray words, or sounds that were not speech"
                }
            },
            "intent": {
                "type": "choice",
                "instructions": "What should the assistant do with this utterance? Use recent_history: if the assistant spoke moments ago and the user is answering or following up, that is for_assistant.",
                "criteria": {
                    "for_assistant": "The user is deliberately calling the assistant (Snippet or Mission Control) or putting a direct question or command to it, and expects a spoken reply",
                    "note": "The user's own remark, plan, idea or commitment that is worth keeping as a note, but is not directed at the assistant",
                    "meeting": "People talking with each other in a meeting, call or discussion that is worth recording as context",
                    "ignore": "Nothing worth keeping"
                }
            },
            "expects_reply": {
                "type": "noul",
                "instructions": "The speaker is talking to the assistant and expects it to answer out loud."
            }
        }
    });
    let response = jev_client()
        .post("https://api.typesafe.ai/v1/systemone")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("typesafe returned {}", response.status()));
    }
    let value: Value = response.json().await.map_err(|e| e.to_string())?;
    let kind = value["answers"]["kind"]["choice"]
        .as_str()
        .ok_or("typesafe answer had no kind")?
        .to_string();
    let intent = value["answers"]["intent"]["choice"].as_str().unwrap_or("note").to_string();
    let addressed = value["answers"]["expects_reply"]["noul"].as_f64().unwrap_or(0.0);
    Ok(Verdict { kind, intent, addressed, judge: "jev" })
}

pub(crate) async fn classify(d: &Shared, id: &str, text: &str) -> Verdict {
    let (key, watcher, mode, phrases) = {
        let c = d.config.lock().unwrap();
        (
            c.typesafe_api_key.clone().filter(|k| !k.trim().is_empty()),
            c.ambient.watcher.clone(),
            c.ambient.mode.clone(),
            c.ambient.wake_phrases.clone(),
        )
    };
    let Some(key) = key.filter(|_| watcher == "jev") else {
        return rule(text, &phrases);
    };
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - 15 * 60;
    let mut history: Vec<(i64, Value)> = d
        .store
        .with_connection(|c| {
            let mut out = Vec::new();
            let mut stmt = c.prepare(
                "SELECT text, created_at FROM ambient_events
                 WHERE status = 'done' AND kept = 1 AND id != ?1 AND created_at >= ?2
                 ORDER BY created_at DESC LIMIT 8",
            )?;
            for row in stmt.query_map(rusqlite::params![id, cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (text, at) = row?;
                out.push((at, json!({"who": "user", "seconds_ago": now - at, "text": text})));
            }
            let mut stmt = c.prepare(
                "SELECT role, text, created_at FROM ambient_turns
                 WHERE role = 'assistant' AND created_at >= ?1 ORDER BY id DESC LIMIT 3",
            )?;
            for row in stmt.query_map([cutoff], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })? {
                let (text, at) = row?;
                out.push((at, json!({"who": "assistant", "seconds_ago": now - at, "text": text})));
            }
            Ok(out)
        })
        .unwrap_or_default();
    history.sort_by_key(|(at, _)| *at);
    let prior: Vec<Value> = history.into_iter().map(|(_, v)| v).collect();
    let open = crate::ambient::conversation().is_some();
    match jev(&key, text, &prior, mode == "active", open).await {
        Ok(mut verdict) => {
            if wake_word(text, &phrases) && verdict.kind == "conversation" {
                verdict.intent = "for_assistant".into();
                verdict.addressed = verdict.addressed.max(0.9);
            }
            verdict
        }
        Err(e) => {
            eprintln!("[ambient] watcher failed: {e}");
            rule(text, &phrases)
        }
    }
}

struct Route {
    id: String,
    device: String,
    started_at: String,
    holder: String,
    phrases: Vec<String>,
    ttl: u32,
    pinned_active: bool,
}

fn route(d: &Shared, id: &str) -> Option<Route> {
    let (holder, _) = crate::ambient::lease_holder()?;
    let (device, started_at): (String, String) = d
        .store
        .with_connection(|c| {
            c.query_row("SELECT device, started_at FROM ambient_events WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })
        .unwrap_or_default();
    if device != holder {
        return None;
    }
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(&started_at) {
        if (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds() > 45 {
            return None;
        }
    }
    let c = d.config.lock().unwrap();
    Some(Route {
        id: id.to_string(),
        device,
        started_at,
        holder,
        phrases: c.ambient.wake_phrases.clone(),
        ttl: c.ambient.conversation_timeout_secs,
        pinned_active: c.ambient.mode == "active",
    })
}

async fn deliver(d: &Shared, route: &Route, text: &str) {
    for _ in 0..30 {
        let earlier: i64 = d
            .store
            .with_connection(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM ambient_events
                     WHERE device = ?1 AND status = 'pending' AND started_at < ?2 AND id != ?3",
                    rusqlite::params![route.device, route.started_at, route.id],
                    |r| r.get(0),
                )
            })
            .unwrap_or(0);
        if earlier == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::ambient::note_user_turn();
    crate::ambient::note_pinned(route.pinned_active);
    let conversation = crate::ambient::open_conversation(&route.holder, route.ttl);
    let _ = crate::ambient::record_turn(&conversation.id, "user", text, &route.holder);
    let device = escape(&route.holder);
    let at = escape(&route.started_at);
    let body = escape(text);
    let envelope = format!("<ambient_turn device=\"{device}\" at=\"{at}\">{body}</ambient_turn>");
    super::autonomy::deliver_to_mission_control(d, envelope).await;
}

pub(crate) async fn process(d: &Shared, id: &str, text: &str) -> (Verdict, bool) {
    let (d2, id2, text2) = (d.clone(), id.to_string(), text.to_string());
    let mut task = tokio::spawn(async move { classify(&d2, &id2, &text2).await });
    let route = route(d, id);
    let wake = route.as_ref().is_some_and(|r| wake_word(text, &r.phrases));
    let mut early = None;
    let mut delivered = false;
    if wake {
        early = tokio::time::timeout(Duration::from_millis(700), &mut task)
            .await
            .ok()
            .and_then(|r| r.ok());
        let vetoed = early.as_ref().is_some_and(|v| !v.kept());
        if !vetoed {
            if let Some(route) = &route {
                deliver(d, route, text).await;
                delivered = true;
            }
        }
    }
    let verdict = match early {
        Some(v) => v,
        None => match task.await {
            Ok(v) => v,
            Err(_) => {
                let phrases = d.config.lock().unwrap().ambient.wake_phrases.clone();
                rule(text, &phrases)
            }
        },
    };
    (verdict, delivered)
}

pub(crate) async fn act(d: &Shared, id: &str, text: &str, verdict: &Verdict, delivered: bool) {
    if delivered || !verdict.kept() {
        return;
    }
    let Some(route) = route(d, id) else {
        return;
    };
    let live = route.pinned_active || crate::ambient::conversation().is_some();
    let opens = verdict.for_assistant() && verdict.addressed >= 0.8;
    let continues = live && verdict.for_assistant() && verdict.addressed >= 0.5;
    if opens || continues {
        deliver(d, &route, text).await;
    }
}
