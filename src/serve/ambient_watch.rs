use super::*;

use serde_json::{Value, json};

use crate::voiceprint::{VoiceHandle, VoiceScore};

const VOICE_NUDGE: f64 = 0.08;

#[derive(Clone)]
pub(crate) struct Verdict {
    pub kind: String,
    pub intent: String,
    pub addressed: f64,
    pub matches: f64,
    pub suggest: f64,
    pub memorable: f64,
    pub echo: f64,
    pub other: f64,
    pub played: f64,
    pub out_of_context: f64,
    pub judge: &'static str,
    pub voice: Option<std::sync::Arc<VoiceScore>>,
}

impl Verdict {
    pub fn kept(&self) -> bool {
        !matches!(self.kind.as_str(), "media" | "background" | "noise") && self.intent != "ignore" && self.echo < 0.7
    }

    pub fn veto(&self) -> f64 {
        self.echo.max(self.other).max(self.played)
    }

    pub fn user_voice(&self) -> Option<f64> {
        self.voice.as_ref().and_then(|v| v.likelihood)
    }

    pub fn inherited() -> Self {
        Verdict {
            kind: "conversation".into(),
            intent: "for_assistant".into(),
            addressed: 1.0,
            matches: 0.0,
            suggest: 0.0,
            memorable: 0.0,
            echo: 0.0,
            other: 0.0,
            played: 0.0,
            out_of_context: 0.0,
            judge: "continuation",
            voice: None,
        }
    }
}

const POSSESSIVE: &[&str] = &["my", "his", "her", "their", "our", "its"];

const NOUN_LEAD: &[&str] = &[
    "a", "an", "the", "this", "that", "these", "those", "code", "of", "my", "his", "her", "their", "our", "each", "every", "some", "any",
    "another",
];

fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn as_call(words: &[String], at: usize, len: usize) -> bool {
    at == 0
        || (!POSSESSIVE.contains(&words[at - 1].as_str()) && (at + len >= words.len() || !NOUN_LEAD.contains(&words[at - 1].as_str())))
}

fn in_call_position(words: &[String], at: usize) -> bool {
    at < 10 || at + 4 >= words.len()
}

pub(super) fn wake_word(text: &str, phrases: &[String]) -> bool {
    let words = words_of(text);
    let near = words.iter().take(3).any(|w| w == "snipit" || w == "snippit")
        || words.windows(2).take(2).any(|w| w[0] == "snip" && matches!(w[1].as_str(), "it" | "but" | "bit" | "pet"));
    if near && phrases.iter().any(|p| p == "snippet") {
        return true;
    }
    phrases.iter().any(|p| {
        let parts: Vec<&str> = p.split_whitespace().collect();
        !parts.is_empty()
            && words.windows(parts.len()).enumerate().any(|(at, w)| {
                in_call_position(&words, at) && w.iter().zip(&parts).all(|(a, b)| a == b) && as_call(&words, at, parts.len())
            })
    })
}

fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

pub(super) fn near_wake(text: &str, phrases: &[String]) -> bool {
    let words = words_of(text);
    let short = words.len() <= 3;
    phrases.iter().filter(|p| !p.contains(' ') && p.len() >= 5).any(|p| {
        let close = |w: &str| {
            let d = edits(w, p);
            w != p.as_str() && w.len() >= 4 && (d <= 2 || (short && d <= 3))
        };
        (0..words.len()).filter(|&at| in_call_position(&words, at) && as_call(&words, at, 1)).any(|at| {
            close(&words[at]) || words.get(at + 1).is_some_and(|next| close(&format!("{}{}", words[at], next)))
        })
    })
}

fn shared_words(utterance: &str, said: &str) -> &'static str {
    let heard = words_of(utterance);
    if heard.len() <= 2 {
        return "too short to compare";
    }
    let said = words_of(said);
    let share = heard.iter().filter(|w| said.contains(w)).count() as f64 / heard.len() as f64;
    match share {
        s if s >= 0.8 => "almost all",
        s if s >= 0.5 => "about half",
        s if s > 0.0 => "a few",
        _ => "none",
    }
}

fn ended_ago(secs: i64) -> &'static str {
    match secs {
        s if s < 0 => "still speaking",
        s if s < 10 => "seconds ago",
        s if s < 60 => "under a minute ago",
        s if s < 180 => "a few minutes ago",
        _ => "not in the last few minutes",
    }
}

pub(super) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn rule(text: &str, phrases: &[String]) -> Verdict {
    let wake = wake_word(text, phrases);
    Verdict {
        kind: "unjudged".into(),
        intent: if wake { "for_assistant".into() } else { "unjudged".into() },
        addressed: if wake { 1.0 } else { 0.0 },
        judge: "rule",
        ..Verdict::inherited()
    }
}

fn classifier_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .pool_idle_timeout(None)
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

fn classifier_questions(with_pending: bool, expecting: bool) -> Value {
    let mut questions = json!({
        "to_assistant": {
            "type": "noul",
            "instructions": "Is `utterance` directed at the assistant: a call, a question, a request or instruction, an answer, a confirmation, a correction, or the rest of something just said to it?",
            "criteria": {
                "true": [
                    "Uses the assistant's name, or `name_in_utterance` is not 'no'",
                    "Asks whether the assistant can hear them or is there, whether or not the conversation is open",
                    "Directly asks the assistant a question or gives it an instruction that a personal AI coding/work assistant would handle (work, tasks, code, reminders, advice, or continuing the active topic in `assistant.last_said` / `mission_control.recent_chat`), unless `user_voice_likelihood` is below 0.1",
                    "Answers, confirms, refuses, thanks or picks an option after `assistant.last_said`",
                    "Continues or corrects a line in `recent` that was handled as sent to the assistant"
                ],
                "false": [
                    "Played media, YouTube/TikTok/Reels videos, dramatic first-person storytelling ('I kissed my boyfriend...', 'last week a street interviewer asked me...', 'before I tell you the last one, drop a comment'), vlogs, or narration",
                    "Thinking aloud, an exclamation, a filler sound, or a remark about the speaker's own life/past that asks nothing of the assistant",
                    "A stray 1-2 word fragment ('Guess what?', 'Hello', 'I mean...') without a wake word when `conversation` is 'closed' or `assistant.speaking_when_utterance_was_heard` is true",
                    "Clearly part of an exchange with someone else shown in `recent`, or third-person chatter ('he asked...', 'she said...', 'they said...')",
                    "When `user_voice_likelihood` is present and below 0.1 (confidently a different speaker)"
                ]
            }
        },
        "assistant_echo": {
            "type": "noul",
            "instructions": "Is `utterance` the assistant's own voice: a repeat, possibly misheard, phonetically similar (e.g. 'Duo' heard as 'Two'), or cut short, of words in `assistant.last_said`?",
            "criteria": {
                "true": "Most words of `utterance` appear in or sound like words in `assistant.last_said` (especially when `assistant.speaking_when_utterance_was_heard` is true) and it adds nothing of its own",
                "false": "Answers, picks an option, confirms or corrects by repeating a few of the assistant's words, adds new words, or `assistant.last_said` is empty"
            }
        },
        "other_party": {
            "type": "noul",
            "instructions": "Is `utterance` spoken by or to someone other than the user talking to the assistant: another person in the room, a narrator/creator in a video or reel, someone on a phone call, third-person remarks, or when `user_voice_likelihood` is below 0.1?"
        },
        "out_of_context": {
            "type": "noul",
            "instructions": "Is `utterance` unrelated to `assistant.last_said`, `mission_control.recent_chat`, `mission_control.recent_tools`, and `recent` turns sent to the assistant (for example played video/reel narration, fiction/storytelling, side chatter with someone else, or an unprompted remark that makes no sense in the ongoing conversation without a wake word)?"
        },
        "played_audio": {
            "type": "noul",
            "instructions": "Is `utterance` played audio, such as TV, film, video, podcast, music, adverts or a game, rather than a person speaking live in the room?"
        },
        "kind": {
            "type": "choice",
            "instructions": "What produced `utterance`?",
            "criteria": {
                "conversation": "Live speech by people in the room: talk to the assistant, to each other, on the phone, or to themselves",
                "media": "Played or broadcast audio: TV, film, video, podcast, music, adverts, games",
                "background": "Distant or overheard fragments unrelated to the people nearby",
                "noise": "Transcription junk, one stray word with no meaning, or sounds that were not speech"
            }
        },
        "intent": {
            "type": "choice",
            "instructions": "What is `utterance` for?",
            "criteria": {
                "for_assistant": "Said to the assistant: a call, request, question, answer or reaction to it",
                "note": "The speaker's own remark, plan, idea or commitment, not said to the assistant",
                "meeting": "People talking with each other in person or on a call",
                "ignore": "Filler, noise, played media, or the assistant's own echoed words"
            }
        },
        "worth_remembering": {
            "type": "noul",
            "instructions": "`utterance` holds information the assistant should still have in future: a commitment, plan, deadline, decision, fact about the user, their health, family, finances, preferences, or work, or anything they would expect to be remembered. Venting, reactions, filler, jokes, commentary on what is on screen, and small talk are false."
        },
        "suggest_conversation": {
            "type": "noul",
            "instructions": "The speaker would be better served by a spoken back and forth with the assistant than by a written reply: a conversational question, thinking aloud with the assistant, or hands-busy talk."
        }
    });
    if with_pending {
        questions["answers_pending"] = json!({
            "type": "noul",
            "instructions": "`utterance` answers `assistant.pending_question`."
        });
    }
    if expecting {
        questions["matches_expectation"] = json!({
            "type": "noul",
            "instructions": "`utterance` is the kind of reply described by `assistant.expecting`: a short answer, confirmation, choice or dictation the assistant asked for."
        });
    }
    questions
}

async fn ask_classifier(key: &str, state: Value, with_pending: bool, expecting: bool) -> Result<Verdict, String> {
    let body = json!({
        "model": "jev-latest",
        "state": state,
        "questions": classifier_questions(with_pending, expecting),
    });
    let response = classifier_client()
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
    verdict_from(&value)
}

pub(super) fn inner_turn(content: &str) -> Option<String> {
    let start = content.find("<ambient_turn")?;
    let open_end = content[start..].find('>')? + start + 1;
    let close = content.rfind("</ambient_turn>")?;
    if close < open_end {
        return None;
    }
    let mut inner = content[open_end..close].to_string();
    if let (Some(a), Some(b)) = (inner.find("<recent_speech>"), inner.find("</recent_speech>")) {
        inner.replace_range(a..b + "</recent_speech>".len(), "");
    }
    Some(
        inner
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

pub(super) struct Heard {
    pub text: String,
    pub ago: i64,
    pub kind: String,
    pub intent: String,
    pub delivered: bool,
}

pub(super) struct Said {
    pub text: String,
    pub ago: i64,
    pub heard: Option<String>,
}

pub(super) struct Moment {
    pub text: String,
    pub duration_ms: i64,
    pub users: Vec<Heard>,
    pub assistant: Vec<Said>,
    pub open: bool,
    pub phrases: Vec<String>,
    pub expecting: Option<(String, String)>,
    pub pending: Option<String>,
    pub mc_chat: Vec<Value>,
    pub mc_tools: Vec<Value>,
    pub media_playing: bool,
    pub during_playback: bool,
    pub user_voice: Option<f64>,
}

fn handled_as(h: &Heard) -> &'static str {
    if h.delivered {
        "sent to the assistant"
    } else if matches!(h.kind.as_str(), "media" | "background") {
        "played audio"
    } else if h.intent == "meeting" {
        "people talking"
    } else {
        "not sent to the assistant"
    }
}

pub(super) fn classifier_state(m: &Moment) -> Value {
    let mut recent: Vec<(i64, Value)> = Vec::new();
    for u in &m.users {
        recent.push((-u.ago, json!({"from": "microphone", "when": ended_ago(u.ago), "text": u.text, "handled_as": handled_as(u)})));
    }
    for a in &m.assistant {
        recent.push((-a.ago, json!({"from": "assistant", "when": ended_ago(a.ago), "text": a.text})));
    }
    recent.sort_by_key(|(at, _)| *at);
    let last = m.assistant.iter().min_by_key(|a| a.ago);
    let last_text = last.map(|a| a.text.clone());
    let shared = m
        .assistant
        .iter()
        .map(|a| shared_words(&m.text, &a.text))
        .min_by_key(|s| ["almost all", "about half", "a few", "none", "too short to compare"].iter().position(|x| x == s))
        .unwrap_or("none");
    let length = match m.duration_ms {
        ms if ms < 2500 => "short",
        ms if ms < 12000 => "medium",
        _ => "long",
    };
    let name = if wake_word(&m.text, &m.phrases) {
        "yes"
    } else if near_wake(&m.text, &m.phrases) {
        "likely, misheard"
    } else {
        "no"
    };
    let mut assistant = json!({
        "names": m.phrases,
        "speaking_when_utterance_was_heard": m.during_playback,
        "last_said": last_text,
        "last_said_ended": last.map(|a| ended_ago(a.ago)).unwrap_or("not in the last few minutes"),
        "last_said_was_cut_off": last.is_some_and(|a| a.heard.is_some()),
        "last_said_was_question": last.is_some_and(|a| a.ago < 120 && a.text.trim_end().ends_with('?')),
        "words_shared_with_utterance": shared,
    });
    if let Some((kind, about)) = &m.expecting {
        assistant["expecting"] = json!({"kind": kind, "about": about});
    }
    if let Some(q) = &m.pending {
        assistant["pending_question"] = json!(q);
    }
    let mut state = json!({
        "utterance": m.text,
        "utterance_length": length,
        "name_in_utterance": name,
        "conversation": if m.open { "open: the user and the assistant are talking back and forth" } else { "closed" },
        "assistant": assistant,
        "mission_control": {
            "recent_chat": m.mc_chat,
            "recent_tools": m.mc_tools,
        },
        "recent": recent.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "phone_playing_media": m.media_playing,
    });
    if let Some(uv) = m.user_voice {
        state["user_voice_likelihood"] = json!((uv * 100.0).round() / 100.0);
    }
    state
}

pub(super) fn verdict_from(value: &Value) -> Result<Verdict, String> {
    let a = &value["answers"];
    let kind = a["kind"]["choice"].as_str().ok_or("typesafe answer had no kind")?.to_string();
    let noul = |k: &str| a[k]["noul"].as_f64().unwrap_or(0.0);
    Ok(Verdict {
        kind,
        intent: a["intent"]["choice"].as_str().unwrap_or("note").to_string(),
        addressed: noul("to_assistant"),
        matches: noul("matches_expectation").max(noul("answers_pending")),
        suggest: noul("suggest_conversation"),
        memorable: noul("worth_remembering"),
        echo: noul("assistant_echo"),
        other: noul("other_party"),
        played: noul("played_audio").max(a["kind"]["probabilities"]["media"].as_f64().unwrap_or(0.0)),
        out_of_context: noul("out_of_context"),
        judge: "classifier",
        voice: None,
    })
}

const FILLERS: &[&str] = &[
    "hmm", "hm", "mm", "mmm", "uh", "uhh", "um", "umm", "ah", "oh", "er", "erm", "so", "huh", "eh", "ooh", "aah", "and", "but", "the", "a",
];

const OTHER_ASSISTANTS: &[&str] = &["alexa", "siri", "google", "gemini", "cortana", "bixby"];

fn other_assistant(text: &str) -> bool {
    let words = words_of(text);
    words.iter().take(3).any(|w| OTHER_ASSISTANTS.contains(&w.as_str()))
        && !words.iter().take(3).any(|w| matches!(w.as_str(), "is" | "was" | "the" | "my" | "use" | "using"))
}

pub(super) fn filler(text: &str) -> bool {
    words_of(text).iter().all(|w| FILLERS.contains(&w.as_str()))
}

/// Strong cap only for a confident different speaker on enough neural audio; otherwise a bounded nudge.
pub(super) fn apply_voice(verdict: &mut Verdict, voice: &VoiceScore, wake: bool) {
    let Some(likelihood) = voice.likelihood else { return };
    if wake {
        return;
    }
    if voice.confident_other() {
        verdict.other = verdict.other.max(0.72);
        verdict.addressed = verdict.addressed.min(0.38);
        return;
    }
    let weight = if voice.neural { VOICE_NUDGE } else { VOICE_NUDGE / 2.0 };
    let delta = (likelihood - 0.5) * 2.0 * weight;
    let clean = verdict.echo < 0.45 && verdict.played < 0.45 && verdict.other < 0.45 && verdict.out_of_context < 0.60;
    if delta < 0.0 || clean {
        verdict.addressed = (verdict.addressed + delta).clamp(0.0, 1.0);
    }
}

pub(super) fn settle(mut verdict: Verdict, text: &str, phrases: &[String], voice: Option<std::sync::Arc<VoiceScore>>) -> Verdict {
    if let Some(v) = &voice {
        apply_voice(&mut verdict, v, wake_word(text, phrases));
    }
    verdict.voice = voice;
    if filler(text) {
        verdict.intent = "ignore".into();
        verdict.addressed = verdict.addressed.min(0.2);
        return verdict;
    }
    if other_assistant(text) {
        verdict.other = verdict.other.max(0.95);
    }
    if near_wake(text, phrases) && verdict.addressed >= 0.35 && verdict.veto() < 0.5 {
        verdict.addressed = verdict.addressed.max(0.75);
        if verdict.kind == "noise" {
            verdict.kind = "conversation".into();
            verdict.intent = "for_assistant".into();
        }
    }
    if wake_word(text, phrases) && verdict.kind == "conversation" && verdict.played < 0.7 && verdict.other < 0.8 {
        verdict.intent = "for_assistant".into();
        verdict.addressed = verdict.addressed.max(0.9);
    }
    if verdict.matches >= 0.7 && verdict.kind != "media" {
        verdict.intent = "for_assistant".into();
        verdict.addressed = verdict.addressed.max(0.85);
    }
    if matches!(verdict.intent.as_str(), "meeting" | "note") || matches!(verdict.kind.as_str(), "background" | "media") {
        verdict.other = verdict.other.max(0.75);
    }
    if !wake_word(text, phrases) && verdict.out_of_context >= 0.65 && verdict.matches < 0.7 {
        verdict.addressed = verdict.addressed.min(0.38);
        if verdict.intent == "for_assistant" {
            verdict.intent = "note".into();
        }
    }
    if verdict.addressed >= 0.5 && verdict.veto() < 0.5 && verdict.out_of_context < 0.65 && verdict.kind == "conversation" {
        verdict.intent = "for_assistant".into();
    }
    verdict
}

fn ready_voice(voices: &[VoiceHandle]) -> Option<std::sync::Arc<VoiceScore>> {
    best_voice(voices.iter().filter_map(VoiceHandle::ready))
}

fn best_voice(scores: impl Iterator<Item = std::sync::Arc<VoiceScore>>) -> Option<std::sync::Arc<VoiceScore>> {
    scores.filter(|s| s.likelihood.is_some()).max_by(|a, b| a.voiced_secs.total_cmp(&b.voiced_secs))
}

async fn wait_voice(id: &str, voices: &[VoiceHandle]) -> Option<std::sync::Arc<VoiceScore>> {
    let mut got = Vec::with_capacity(voices.len());
    for v in voices {
        got.extend(v.wait().await);
    }
    if !voices.is_empty() && got.len() < voices.len() {
        eprintln!("[voiceprint] {} timed out", &id[..id.len().min(8)]);
    }
    best_voice(got.into_iter())
}

pub(crate) async fn classify(d: &Shared, ids: &[String], text: &str, voices: Vec<VoiceHandle>) -> Verdict {
    let id = ids.last().map(String::as_str).unwrap_or("");
    let excluded = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let (key, watcher, phrases, chat_context) = {
        let c = d.config.lock().unwrap();
        (
            c.typesafe_api_key.clone().filter(|k| !k.trim().is_empty()),
            c.ambient.watcher.clone(),
            c.ambient.wake_phrases.clone(),
            c.ambient.watcher_chat_context,
        )
    };
    let Some(key) = key.filter(|_| watcher == "classifier" || watcher == "jev") else {
        return Verdict { voice: ready_voice(&voices), ..rule(text, &phrases) };
    };
    let now = chrono::Utc::now().timestamp();
    let (duration_ms, users, assistant): (i64, Vec<Heard>, Vec<Said>) = d
        .store
        .with_connection(|c| {
            let duration_ms: i64 =
                c.query_row("SELECT duration_ms FROM ambient_events WHERE id = ?1", [id], |r| r.get(0)).unwrap_or(0);
            let mut users = Vec::new();
            let mut stmt = c.prepare(
                "SELECT text, created_at, COALESCE(verdict, ''), COALESCE(intent, ''), delivered FROM ambient_events
                 WHERE status = 'done' AND text != '' AND id NOT IN (SELECT value FROM json_each(?1)) AND created_at >= ?2
                   AND device = (SELECT device FROM ambient_events WHERE id = ?3)
                 ORDER BY started_at DESC LIMIT 4",
            )?;
            for row in stmt.query_map(rusqlite::params![excluded, now - 180, id], |r| {
                Ok(Heard {
                    text: r.get(0)?,
                    ago: now - r.get::<_, i64>(1)?,
                    kind: r.get(2)?,
                    intent: r.get(3)?,
                    delivered: r.get::<_, i64>(4)? != 0,
                })
            })? {
                users.push(row?);
            }
            let mut assistant = Vec::new();
            let mut stmt = c.prepare(
                "SELECT t.text, t.created_at + COALESCE((SELECT SUM(ms) FROM ambient_chunks WHERE turn_id = t.id), 0) / 1000,
                        t.status, COALESCE(t.heard_text, '')
                 FROM ambient_turns t
                 WHERE t.role = 'assistant' AND t.created_at >= ?1 AND t.text != '' AND t.status != 'cancelled'
                   AND t.device = (SELECT device FROM ambient_events WHERE id = ?2)
                 ORDER BY t.id DESC LIMIT 2",
            )?;
            for row in stmt.query_map(rusqlite::params![now - 300, id], |r| {
                let status: String = r.get(2)?;
                let heard: String = r.get(3)?;
                Ok(Said {
                    text: r.get(0)?,
                    ago: now - r.get::<_, i64>(1)?,
                    heard: (status == "interrupted" && !heard.trim().is_empty()).then_some(heard),
                })
            })? {
                assistant.push(row?);
            }
            Ok((duration_ms, users, assistant))
        })
        .unwrap_or_default();
    let (pending, mc_chat, mc_tools) = if chat_context {
        let mc = super::ambient_context::mission_control(d);
        (
            mc["pending_question"].as_str().map(str::to_string),
            mc["recent_chat"].as_array().cloned().unwrap_or_default(),
            mc["recent_tools"].as_array().cloned().unwrap_or_default(),
        )
    } else {
        (None, Vec::new(), Vec::new())
    };
    let with_pending = pending.is_some();
    let (media_playing, during_playback): (bool, bool) = d
        .store
        .with_connection(|c| {
            c.query_row(
                "SELECT COALESCE(MAX(media), 0), COALESCE(MAX(during_playback), 0) FROM ambient_events WHERE id IN (SELECT value FROM json_each(?1))",
                [&excluded],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)? != 0)),
            )
        })
        .unwrap_or((false, false));
    if media_playing && !wake_word(text, &phrases) {
        return Verdict { kind: "media".into(), intent: "ignore".into(), judge: "rule", voice: ready_voice(&voices), ..Verdict::inherited() };
    }
    let expectation = crate::ambient::expectation();
    let expecting = expectation.is_some();
    let moment = Moment {
        text: text.to_string(),
        duration_ms,
        users,
        assistant,
        open: crate::ambient::conversation().is_some(),
        phrases: phrases.clone(),
        expecting: expectation.map(|e| (e.kind, e.about)),
        pending,
        mc_chat,
        mc_tools,
        media_playing,
        during_playback,
        user_voice: ready_voice(&voices).filter(|v| v.neural).and_then(|v| v.likelihood),
    };
    let classifier = ask_classifier(&key, classifier_state(&moment), with_pending, expecting);
    let (classified, voice) = tokio::join!(classifier, wait_voice(id, &voices));
    match classified {
        Ok(verdict) => settle(verdict, text, &phrases, voice),
        Err(e) => {
            eprintln!("[ambient] voice classifier failed: {e}");
            Verdict { voice, ..rule(text, &phrases) }
        }
    }
}

/// Real-time parallel classification on partial streaming transcripts while the user
/// is still speaking, enabling fast classifier-driven barge-in/interruption.
pub(crate) async fn classify_live(d: &Shared, device: &str, text: &str, during_playback: bool, user_voice: Option<f64>) -> Verdict {
    let (key, watcher, phrases, chat_context) = {
        let c = d.config.lock().unwrap();
        (
            c.typesafe_api_key.clone().filter(|k| !k.trim().is_empty()),
            c.ambient.watcher.clone(),
            c.ambient.wake_phrases.clone(),
            c.ambient.watcher_chat_context,
        )
    };
    let Some(key) = key.filter(|_| watcher == "classifier" || watcher == "jev") else {
        return rule(text, &phrases);
    };
    let now = chrono::Utc::now().timestamp();
    let (users, mut assistant): (Vec<Heard>, Vec<Said>) = d
        .store
        .with_connection(|c| {
            let mut users = Vec::new();
            let mut stmt = c.prepare(
                "SELECT text, created_at, COALESCE(verdict, ''), COALESCE(intent, ''), delivered FROM ambient_events
                 WHERE status = 'done' AND text != '' AND created_at >= ?1 AND device = ?2
                 ORDER BY started_at DESC LIMIT 4",
            )?;
            for row in stmt.query_map(rusqlite::params![now - 180, device], |r| {
                Ok(Heard {
                    text: r.get(0)?,
                    ago: now - r.get::<_, i64>(1)?,
                    kind: r.get(2)?,
                    intent: r.get(3)?,
                    delivered: r.get::<_, i64>(4)? != 0,
                })
            })? {
                users.push(row?);
            }
            let mut assistant = Vec::new();
            let mut stmt = c.prepare(
                "SELECT t.text, t.created_at + COALESCE((SELECT SUM(ms) FROM ambient_chunks WHERE turn_id = t.id), 0) / 1000,
                        t.status, COALESCE(t.heard_text, '')
                 FROM ambient_turns t
                 WHERE t.role = 'assistant' AND t.created_at >= ?1 AND t.text != '' AND t.status != 'cancelled'
                   AND t.device = ?2
                 ORDER BY t.id DESC LIMIT 2",
            )?;
            for row in stmt.query_map(rusqlite::params![now - 300, device], |r| {
                let status: String = r.get(2)?;
                let heard: String = r.get(3)?;
                Ok(Said {
                    text: r.get(0)?,
                    ago: (now - r.get::<_, i64>(1)?).max(0),
                    heard: (status == "interrupted" && !heard.trim().is_empty()).then_some(heard),
                })
            })? {
                assistant.push(row?);
            }
            Ok((users, assistant))
        })
        .unwrap_or_default();
    for live_txt in crate::tts_stream::live_texts(device) {
        if !assistant.iter().any(|a| a.text.trim() == live_txt.trim()) {
            assistant.insert(0, Said { text: live_txt, ago: 0, heard: None });
        }
    }
    let (pending, mc_chat, mc_tools) = if chat_context {
        let mc = super::ambient_context::mission_control(d);
        (
            mc["pending_question"].as_str().map(str::to_string),
            mc["recent_chat"].as_array().cloned().unwrap_or_default(),
            mc["recent_tools"].as_array().cloned().unwrap_or_default(),
        )
    } else {
        (None, Vec::new(), Vec::new())
    };
    let with_pending = pending.is_some();
    let expectation = crate::ambient::expectation();
    let expecting = expectation.is_some();
    let moment = Moment {
        text: text.to_string(),
        duration_ms: 800,
        users,
        assistant,
        open: crate::ambient::conversation().is_some() || during_playback,
        phrases: phrases.clone(),
        expecting: expectation.map(|e| (e.kind, e.about)),
        pending,
        mc_chat,
        mc_tools,
        media_playing: false,
        during_playback,
        user_voice: None,
    };
    let legacy = user_voice.map(|l| {
        std::sync::Arc::new(VoiceScore { neural: false, similarity: None, likelihood: Some(l), voiced_secs: 0.0, embedding: None, ms: 0 })
    });
    match ask_classifier(&key, classifier_state(&moment), with_pending, expecting).await {
        Ok(verdict) => settle(verdict, text, &phrases, legacy),
        Err(_) => rule(text, &phrases),
    }
}

#[cfg(test)]
mod voice_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn score(neural: bool, similarity: f32, voiced_secs: f32) -> Arc<VoiceScore> {
        Arc::new(VoiceScore {
            neural,
            similarity: neural.then_some(similarity),
            likelihood: Some(crate::voiceprint::likelihood(similarity)),
            voiced_secs,
            embedding: None,
            ms: 0,
        })
    }

    fn classified(addressed: f64) -> Verdict {
        Verdict { kind: "conversation".into(), intent: "for_assistant".into(), addressed, judge: "classifier", ..Verdict::inherited() }
    }

    fn phrases() -> Vec<String> {
        vec!["snippet".into()]
    }

    #[test]
    fn confident_other_speaker_is_capped() {
        let v = settle(classified(0.8), "what time is the meeting tomorrow", &phrases(), Some(score(true, 0.05, 2.5)));
        assert!(v.addressed <= 0.38 && v.other >= 0.72);
        assert_eq!(v.user_voice(), Some(crate::voiceprint::likelihood(0.05)));
    }

    #[test]
    fn short_or_uncertain_audio_only_nudges() {
        let v = settle(classified(0.8), "how are you this beautiful evening", &phrases(), Some(score(true, 0.05, 1.5)));
        assert!(v.addressed >= 0.70 && v.addressed < 0.8, "{}", v.addressed);
        assert_eq!(v.intent, "for_assistant");
        let v = settle(classified(0.8), "how are you this beautiful evening", &phrases(), Some(score(true, 0.30, 4.0)));
        assert!(v.addressed > 0.7 && v.other < 0.5);
    }

    #[test]
    fn legacy_score_never_vetoes() {
        let v = settle(classified(0.8), "listened to my last two messages", &phrases(), Some(score(false, 0.0, 0.0)));
        assert!(v.addressed >= 0.75, "{}", v.addressed);
        assert_eq!(v.intent, "for_assistant");
    }

    #[test]
    fn wake_word_ignores_voice() {
        let v = settle(classified(0.6), "snippet do you hear me", &phrases(), Some(score(true, 0.0, 3.0)));
        assert!(v.addressed >= 0.9 && v.other < 0.5);
    }

    #[test]
    fn matching_voice_boosts_only_clean_verdicts() {
        let v = settle(classified(0.55), "can you check the build", &phrases(), Some(score(true, 0.8, 3.0)));
        assert!((v.addressed - 0.63).abs() < 0.01, "{}", v.addressed);
        let mut dirty = classified(0.55);
        dirty.echo = 0.5;
        let v = settle(dirty, "can you check the build", &phrases(), Some(score(true, 0.8, 3.0)));
        assert!((v.addressed - 0.55).abs() < 1e-9);
    }

    #[test]
    fn no_score_leaves_verdict_alone() {
        let none = Arc::new(VoiceScore { neural: false, similarity: None, likelihood: None, voiced_secs: 0.4, embedding: None, ms: 0 });
        let v = settle(classified(0.8), "thank you for that", &phrases(), Some(none));
        assert!((v.addressed - 0.8).abs() < 1e-9 && v.user_voice().is_none());
        let v = settle(classified(0.8), "thank you for that", &phrases(), None);
        assert!((v.addressed - 0.8).abs() < 1e-9);
    }

    #[tokio::test]
    async fn unresolved_voice_times_out_to_none() {
        let pending = crate::voiceprint::VoiceHandle::pending_for_test(Duration::from_millis(50));
        let t = Instant::now();
        assert!(wait_voice("t", &[pending]).await.is_none());
        assert!(t.elapsed() < Duration::from_millis(200));
    }

    /// VP_BENCH_WAV=<wav> cargo test --release sim_end_to_deliver -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn sim_end_to_deliver() {
        let Ok(path) = std::env::var("VP_BENCH_WAV") else { return };
        let pcm = crate::voiceprint::pcm_from_audio(&std::fs::read(path).unwrap()).unwrap();
        let (enrol, rest) = pcm.split_at(pcm.len() / 2);
        let (vector, _) = crate::voiceprint::embed_enrolment(enrol).await.unwrap();
        let samples = vec![crate::voiceprint::VoiceprintSample {
            id: "s".into(),
            label: "s".into(),
            created_at: 0,
            model: crate::voiceprint::NEURAL_MODEL.into(),
            audio_path: None,
            vector,
        }];
        let utterance = &rest[..rest.len().min(6 * 32000)];
        for classifier_ms in [50u64, 350] {
            for with_voice in [false, true, false, true] {
                crate::ambient::mark("sim-utt", "end");
                let t0 = Instant::now();
                let voices: Vec<_> = if with_voice { crate::voiceprint::start("sim-utt", "sim", utterance, &samples).into_iter().collect() } else { Vec::new() };
                tokio::time::sleep(Duration::from_millis(150)).await;
                let classifier = async {
                    tokio::time::sleep(Duration::from_millis(classifier_ms)).await;
                    classified(0.7)
                };
                let (verdict, voice) = tokio::join!(classifier, wait_voice("sim-utt", &voices));
                let v = settle(verdict, "can you check the build", &phrases(), voice.clone());
                println!(
                    "classifier {classifier_ms}ms voice={with_voice}: end->deliver {}ms (voice {:?}ms, user_voice {:?}, addressed {:.2})",
                    t0.elapsed().as_millis(),
                    voice.as_ref().map(|s| s.ms),
                    v.user_voice(),
                    v.addressed
                );
            }
        }
    }
}
