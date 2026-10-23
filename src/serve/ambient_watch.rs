use super::*;

use serde_json::{Value, json};

pub(crate) struct Verdict {
    pub kind: String,
    pub addressed: f64,
    pub judge: &'static str,
}

impl Verdict {
    pub fn kept(&self) -> bool {
        !matches!(self.kind.as_str(), "media" | "background" | "noise")
    }
}

fn wake_word(text: &str, phrases: &[String]) -> bool {
    let head: String = text
        .to_lowercase()
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ");
    phrases.iter().any(|p| head.contains(p.as_str()))
}

fn rule(text: &str, phrases: &[String]) -> Verdict {
    Verdict {
        kind: "unjudged".into(),
        addressed: if wake_word(text, phrases) { 1.0 } else { 0.0 },
        judge: "rule",
    }
}

async fn jev(key: &str, text: &str, prior: &[String], active: bool, open: bool) -> Result<Verdict, String> {
    let body = json!({
        "model": "jev-latest",
        "state": {
            "utterance": text,
            "previous_utterances": prior,
            "assistant_mode": if active { "active" } else { "passive" },
            "conversation_open": open,
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
            "addressed": {
                "type": "noul",
                "instructions": "The speaker is talking to the assistant (called Snippet or Mission Control) and expects it to respond."
            }
        }
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
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
    let addressed = value["answers"]["addressed"]["noul"].as_f64().unwrap_or(0.0);
    Ok(Verdict { kind, addressed, judge: "jev" })
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
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(10)).timestamp();
    let prior: Vec<String> = d
        .store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT text FROM ambient_events
                 WHERE status = 'done' AND kept = 1 AND id != ?1 AND created_at >= ?2
                 ORDER BY created_at DESC LIMIT 3",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![id, cutoff], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    let open = crate::ambient::conversation().is_some();
    match jev(&key, text, &prior, mode == "active", open).await {
        Ok(mut verdict) => {
            if wake_word(text, &phrases) && verdict.kind == "conversation" {
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

pub(crate) async fn act(d: &Shared, id: &str, text: &str, verdict: &Verdict) {
    if !verdict.kept() {
        return;
    }
    let Some((holder, _)) = crate::ambient::lease_holder() else {
        return;
    };
    let device: String = d
        .store
        .with_connection(|c| {
            c.query_row("SELECT device FROM ambient_events WHERE id = ?1", [id], |r| r.get(0))
        })
        .unwrap_or_default();
    if !device.is_empty() && device != holder {
        return;
    }
    let (pinned_active, phrases, ttl) = {
        let c = d.config.lock().unwrap();
        (
            c.ambient.mode == "active",
            c.ambient.wake_phrases.clone(),
            c.ambient.conversation_timeout_secs,
        )
    };
    let wake = wake_word(text, &phrases);
    let conversation = crate::ambient::conversation();
    let live = pinned_active || conversation.is_some();
    let opens = verdict.kind != "unjudged" && verdict.addressed >= 0.85 || wake;
    let continues = live && verdict.addressed >= 0.5;
    if !(continues || opens) {
        return;
    }
    let conversation = crate::ambient::open_conversation(&holder, ttl);
    let _ = crate::ambient::record_turn(&conversation.id, "user", text, &holder);
    let at = chrono::Utc::now().to_rfc3339();
    let envelope = format!("<ambient_turn device=\"{holder}\" at=\"{at}\">{text}</ambient_turn>");
    super::autonomy::deliver_to_mission_control(d, envelope).await;
}
