use super::*;

use serde_json::{Value, json};

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
}

impl Verdict {
    pub fn kept(&self) -> bool {
        !matches!(self.kind.as_str(), "media" | "background" | "noise") && self.intent != "ignore" && self.echo < 0.7
    }

    pub fn veto(&self) -> f64 {
        self.echo.max(self.other).max(self.played)
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
                    "Asks a question or gives an instruction that a personal AI assistant would handle, such as the status of work, tasks, reminders, information, advice, chat or jokes, unless `recent` shows the speaker talking with someone else",
                    "When it is the user's own voice (`user_voice_likelihood` >= 0.55 or unset) and there is no conversation with another person in the room, be friendly and generous: any question, request, follow-up, topic change, or remark that could potentially be directed to the assistant is true, even without a wake word",
                    "Answers, confirms, refuses, thanks or picks an option after `assistant.last_said`",
                    "Continues or corrects a line in `recent` that was handled as sent to the assistant, even a fragment cut off by a pause"
                ],
                "false": [
                    "A filler sound or an exclamation ('hmm', 'ouch') that asks nothing of anyone",
                    "A stray 1-2 word fragment heard while `assistant.speaking_when_utterance_was_heard` is true that is not a stop/wait command or a direct answer to a question in `assistant.last_said`",
                    "Clearly part of an exchange with someone else shown in `recent`, or third-person chatter ('he asked...', 'they said...', 'you guys')",
                    "Uses the word snippet for a thing, such as a code snippet, instead of calling the assistant"
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
            "instructions": "Is `utterance` part of a conversation with someone other than the assistant: a person or background actors in the room, someone on a phone call, third-person remarks about the user ('he asked...', 'tell him...'), a caller or automated phone agent, or another voice assistant?"
        },
        "out_of_context": {
            "type": "noul",
            "instructions": "Is `utterance` side chatter with someone else or a meaningless cut-off fragment? Note: a new question, topic change, or request from the user themselves is NOT out of context even without a wake word."
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

fn looks_directed_or_question(text: &str) -> bool {
    if text.contains('?') {
        return true;
    }
    let ws = words_of(text);
    if ws.len() < 2 {
        return false;
    }
    const LEADS: &[&str] = &[
        "what", "whats", "how", "why", "when", "where", "who", "which", "can", "could", "would", "will", "should", "do", "does",
        "did", "is", "are", "was", "were", "tell", "give", "show", "check", "make", "fix", "help", "explain", "find", "search",
        "open", "run", "build", "please", "pls", "hey", "yo", "listen", "look", "let", "lets",
    ];
    if LEADS.contains(&ws[0].as_str()) || (ws.len() >= 3 && LEADS.contains(&ws[1].as_str())) {
        return true;
    }
    let joined = ws.join(" ");
    joined.contains("i want")
        || joined.contains("i need")
        || joined.contains("can you")
        || joined.contains("could you")
        || joined.contains("for me")
        || joined.contains("about me")
        || joined.contains("about myself")
        || joined.contains("do you")
        || joined.contains("what do you")
}

pub(super) fn settle(mut verdict: Verdict, text: &str, phrases: &[String]) -> Verdict {
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
    // When it is a live person (not media/echo) and not clearly a meeting with another party,
    // be friendly toward questions or requests that could be directed to Snippet.
    if verdict.echo < 0.5 && verdict.played < 0.5 && verdict.other < 0.55 && looks_directed_or_question(text) && verdict.addressed >= 0.25 {
        verdict.addressed = verdict.addressed.max(0.68);
        verdict.out_of_context = verdict.out_of_context.min(0.45);
        verdict.kind = "conversation".into();
        verdict.intent = "for_assistant".into();
    }
    if matches!(verdict.intent.as_str(), "meeting") || matches!(verdict.kind.as_str(), "background" | "media") {
        verdict.other = verdict.other.max(0.75);
    }
    if !wake_word(text, phrases) && verdict.out_of_context >= 0.78 && verdict.other >= 0.50 && verdict.matches < 0.7 {
        verdict.addressed = verdict.addressed.min(0.40);
        if verdict.intent == "for_assistant" {
            verdict.intent = "note".into();
        }
    }
    if verdict.addressed >= 0.5 && verdict.veto() < 0.5 && verdict.out_of_context < 0.75 && verdict.kind == "conversation" {
        verdict.intent = "for_assistant".into();
    }
    verdict
}

pub(crate) async fn classify(d: &Shared, ids: &[String], text: &str, user_voice: Option<f64>) -> Verdict {
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
        return rule(text, &phrases);
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
        return Verdict { kind: "media".into(), intent: "ignore".into(), judge: "rule", ..Verdict::inherited() };
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
        user_voice,
    };
    match ask_classifier(&key, classifier_state(&moment), with_pending, expecting).await {
        Ok(mut verdict) => {
            if let Some(uv) = user_voice {
                if uv < 0.32 && !wake_word(text, &phrases) {
                    verdict.other = verdict.other.max(0.65);
                    verdict.addressed = verdict.addressed.min(0.45);
                } else if uv >= 0.55 && verdict.echo < 0.5 && verdict.played < 0.5 && verdict.other < 0.55 {
                    if looks_directed_or_question(text) || verdict.addressed >= 0.32 {
                        verdict.addressed = (verdict.addressed + 0.22).max(0.68).min(1.0);
                        verdict.out_of_context = verdict.out_of_context.min(0.40);
                        verdict.kind = "conversation".into();
                        verdict.intent = "for_assistant".into();
                    }
                }
            }
            settle(verdict, text, &phrases)
        }
        Err(e) => {
            eprintln!("[ambient] voice classifier failed: {e}");
            rule(text, &phrases)
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
        user_voice,
    };
    match ask_classifier(&key, classifier_state(&moment), with_pending, expecting).await {
        Ok(mut verdict) => {
            if let Some(uv) = user_voice {
                if uv < 0.32 && !wake_word(text, &phrases) {
                    verdict.other = verdict.other.max(0.68);
                    verdict.addressed = verdict.addressed.min(0.40);
                } else if uv >= 0.55 && verdict.echo < 0.5 && verdict.played < 0.5 && verdict.other < 0.55 {
                    if looks_directed_or_question(text) || verdict.addressed >= 0.35 {
                        verdict.addressed = (verdict.addressed + 0.20).max(0.65).min(1.0);
                        verdict.out_of_context = verdict.out_of_context.min(0.40);
                        verdict.kind = "conversation".into();
                        verdict.intent = "for_assistant".into();
                    }
                }
            }
            settle(verdict, text, &phrases)
        }
        Err(_) => rule(text, &phrases),
    }
}
