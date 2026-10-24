use super::*;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;

use super::ambient_watch::{self, Verdict};

const HOLD: Duration = Duration::from_millis(1600);
const FAST_HOLD: Duration = Duration::from_millis(300);
const ACTIVITY_HOLD: Duration = Duration::from_secs(30);
const STALL: Duration = Duration::from_millis(2000);
const WAKE_VETO: Duration = Duration::from_millis(400);
const LATE_WINDOW: Duration = Duration::from_secs(20);
const MERGE_GAP: f64 = 1.5;
const CONTINUED_GAP: f64 = 4.0;
const MAX_PARTS: usize = 5;
const MAX_SPAN: f64 = 30.0;
const PASSIVE_THRESHOLD: f64 = 0.7;
const PINNED_THRESHOLD: f64 = 0.5;
const LIVE_REPLY_THRESHOLD: f64 = 0.55;
const MATCH_THRESHOLD: f64 = 0.7;
const MEMORABLE_THRESHOLD: f64 = 0.65;

#[derive(Clone)]
pub(crate) struct Part {
    pub id: String,
    pub seq: i64,
    pub text: String,
    pub started: f64,
    pub ended: f64,
    pub started_at: String,
    pub summary: String,
    pub speakers: Vec<Value>,
    pub continued: bool,
}

pub(crate) enum Msg {
    Done(Part),
    Skip(i64),
    Activity(Option<i64>),
}

struct Flushed {
    ids: Vec<String>,
    ended: f64,
    at: Instant,
    delivered: bool,
}

static ROUTERS: Mutex<Option<HashMap<String, mpsc::UnboundedSender<Msg>>>> = Mutex::new(None);

pub(crate) fn submit(d: &Shared, device: &str, msg: Msg) {
    let mut guard = ROUTERS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(tx) = map.get(device) {
        match tx.send(msg) {
            Ok(()) => return,
            Err(mpsc::error::SendError(m)) => {
                map.remove(device);
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(actor(d.clone(), device.to_string(), rx));
                let _ = tx.send(m);
                map.insert(device.to_string(), tx);
                return;
            }
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(actor(d.clone(), device.to_string(), rx));
    let _ = tx.send(msg);
    map.insert(device.to_string(), tx);
}

enum Wake {
    Msg(Msg),
    Closed,
    Timer,
}

async fn actor(d: Shared, device: String, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut announced: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut drained_max: i64 = 0;
    let mut buffered: BTreeMap<i64, Option<Part>> = BTreeMap::new();
    let mut stalled: Option<Instant> = None;
    let mut turn: Vec<Part> = Vec::new();
    let mut deadline: Option<Instant> = None;
    let mut last: Option<Flushed> = None;
    loop {
        let next = [deadline, stalled.map(|s| s + STALL)].into_iter().flatten().min();
        let wake = match next {
            Some(t) => tokio::select! {
                m = rx.recv() => m.map(Wake::Msg).unwrap_or(Wake::Closed),
                _ = tokio::time::sleep_until(t.into()) => Wake::Timer,
            },
            None => rx.recv().await.map(Wake::Msg).unwrap_or(Wake::Closed),
        };
        match wake {
            Wake::Closed => break,
            Wake::Msg(Msg::Activity(seq)) => {
                if let Some(seq) = seq.filter(|s| *s > drained_max) {
                    announced.insert(seq);
                }
                if !turn.is_empty() {
                    deadline = Some(Instant::now() + ACTIVITY_HOLD);
                }
            }
            Wake::Msg(Msg::Done(part)) => {
                if part.seq <= 0 {
                    accept(&d, &device, part, &mut turn, &mut deadline, &mut last).await;
                } else {
                    announced.remove(&part.seq);
                    buffered.insert(part.seq, Some(part));
                }
            }
            Wake::Msg(Msg::Skip(seq)) => {
                if seq > 0 {
                    announced.remove(&seq);
                    buffered.insert(seq, None);
                }
            }
            Wake::Timer => {}
        }
        if stalled.is_some_and(|s| s.elapsed() >= STALL) {
            if let Some((first, _)) = buffered.iter().next() {
                let first = *first;
                announced.retain(|s| *s >= first);
            }
            stalled = None;
        }
        while let Some((&first, _)) = buffered.iter().next() {
            if announced.range(..first).next().is_some() {
                break;
            }
            drained_max = drained_max.max(first);
            match buffered.remove(&first) {
                Some(Some(part)) => accept(&d, &device, part, &mut turn, &mut deadline, &mut last).await,
                Some(None) => {
                    if announced.is_empty() && !turn.is_empty() && deadline.is_some_and(|t| t > Instant::now() + HOLD) {
                        deadline = Some(Instant::now() + HOLD);
                    }
                }
                None => {}
            }
        }
        stalled = if buffered.is_empty() { None } else { stalled.or(Some(Instant::now())) };
        if deadline.is_some_and(|t| Instant::now() >= t) && !turn.is_empty() {
            let parts = std::mem::take(&mut turn);
            deadline = None;
            flush(&d, &device, parts, crate::ambient::conversation().is_some(), &mut last).await;
        }
    }
}

fn merge_summary(parts: &[Part]) -> String {
    let has_you = parts.iter().any(|p| p.summary == "you" || p.summary == "mixed");
    let has_other = parts.iter().any(|p| p.summary == "other" || p.summary == "mixed");
    match (has_you, has_other) {
        (true, true) => "mixed",
        (true, false) => "you",
        (false, true) => "other",
        _ => "unknown",
    }
    .to_string()
}

fn compatible(a: &Part, b: &Part) -> bool {
    !((a.summary == "you" && b.summary == "other") || (a.summary == "other" && b.summary == "you"))
}

fn enrolled(d: &Shared) -> bool {
    crate::diarize::enrolled(&d.store)
}

async fn accept(
    d: &Shared,
    device: &str,
    part: Part,
    turn: &mut Vec<Part>,
    deadline: &mut Option<Instant>,
    last: &mut Option<Flushed>,
) {
    let (phrases, ttl) = {
        let c = d.config.lock().unwrap();
        (c.ambient.wake_phrases.clone(), c.ambient.conversation_timeout_secs)
    };
    let gated = enrolled(d) && part.summary == "other";
    let wake = !gated && ambient_watch::wake_word(&part.text, &phrases);
    if turn.is_empty() && !gated {
        if let Some(l) = last.as_mut() {
            let gap = part.started - l.ended;
            let routable = crate::ambient::lease_holder().is_some_and(|(h, _)| h == device);
            if l.delivered && routable && crate::ambient::conversation().is_some() && l.at.elapsed() < LATE_WINDOW && gap <= CONTINUED_GAP && part.summary != "other" && (part.summary == "you" || !enrolled(d)) {
                let parent = l.ids.clone();
                let ended = part.ended;
                let ids = vec![part.id.clone()];
                deliver(d, device, &[part], &Verdict::inherited(), false, Some(&parent), ttl, false).await;
                l.ended = ended;
                mark_verdict(d, &ids, &Verdict::inherited(), true);
                return;
            }
        }
    }
    let mergeable = turn.last().is_some_and(|prev| {
        let gap = part.started - prev.ended;
        let allowed = if part.continued || deadline.is_some_and(|t| t > Instant::now() + HOLD) { CONTINUED_GAP } else { MERGE_GAP };
        gap <= allowed
            && compatible(prev, &part)
            && turn.len() < MAX_PARTS
            && part.ended - turn[0].started <= MAX_SPAN
    });
    if !mergeable && !turn.is_empty() {
        let parts = std::mem::take(turn);
        flush(d, device, parts, crate::ambient::conversation().is_some(), last).await;
    }
    turn.push(part);
    if wake {
        let parts = std::mem::take(turn);
        *deadline = None;
        flush(d, device, parts, true, last).await;
    } else if crate::ambient::conversation().is_some() {
        *deadline = Some(Instant::now() + FAST_HOLD);
    } else {
        *deadline = Some(Instant::now() + HOLD);
    }
}

fn mark_verdict(d: &Shared, ids: &[String], v: &Verdict, delivered: bool) {
    let kept = v.kept();
    for id in ids {
        let _ = d.store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_events SET verdict = ?2, addressed = ?3, judge = ?4, kept = ?5, intent = ?6, delivered = ?7
                 WHERE id = ?1",
                rusqlite::params![id, v.kind, v.addressed, v.judge, kept as i64, v.intent, delivered as i64],
            )?;
            if kept {
                c.execute(
                    "INSERT INTO ambient_events_fts(rowid, text) SELECT rowid, text FROM ambient_events WHERE id = ?1",
                    [id],
                )?;
            }
            Ok(())
        });
    }
}

fn decide(live: bool, pinned: bool, v: &Verdict, wake: bool, expecting: bool, from_others: bool, you: bool, unsure_voice: bool) -> (bool, bool) {
    if !v.kept() || from_others {
        return (false, false);
    }
    let matched = expecting && v.matches >= MATCH_THRESHOLD;
    if live {
        if wake || matched || v.continues >= 0.6 {
            return (true, false);
        }
        let bar = if unsure_voice && !you { 0.7 } else { LIVE_REPLY_THRESHOLD };
        return (v.addressed >= bar && (v.for_assistant() || v.addressed >= 0.6), false);
    }
    if pinned {
        let asked = v.for_assistant() && v.addressed >= PINNED_THRESHOLD;
        let go = wake || asked || matched;
        return (go, go);
    }
    let bar = if unsure_voice && !you { 0.8 } else if v.kind == "conversation" { 0.55 } else { PASSIVE_THRESHOLD };
    let asked = v.for_assistant() && v.addressed >= bar;
    (wake || asked || matched, false)
}

async fn flush(d: &Shared, device: &str, parts: Vec<Part>, fast: bool, last: &mut Option<Flushed>) {
    if parts.is_empty() {
        return;
    }
    let ids: Vec<String> = parts.iter().map(|p| p.id.clone()).collect();
    if let Some(last) = ids.last() {
        super::ambient_routes::mark(last, "flush");
    }
    let text = parts.iter().map(|p| p.text.trim()).collect::<Vec<_>>().join(" ");
    let summary = merge_summary(&parts);
    let (phrases, ttl) = {
        let c = d.config.lock().unwrap();
        (c.ambient.wake_phrases.clone(), c.ambient.conversation_timeout_secs)
    };
    let holder = crate::ambient::lease_holder().map(|(h, _)| h);
    let routable = holder.as_deref() == Some(device);
    let from_others = enrolled(d) && summary == "other";
    let wake = !from_others && ambient_watch::wake_word(&text, &phrases);
    if wake && !enrolled(d) {
        claim_wake_speaker(d, &parts);
    }
    let expecting = crate::ambient::expectation().is_some();
    let live = crate::ambient::conversation().is_some();
    let pinned = crate::ambient::is_pinned();

    let (d2, ids2, text2) = (d.clone(), ids.clone(), text.clone());
    let mut task = tokio::spawn(async move { ambient_watch::classify(&d2, &ids2, &text2).await });
    let mut delivered = false;
    let mut early: Option<Verdict> = None;
    if fast && routable && !from_others {
        early = tokio::time::timeout(WAKE_VETO, &mut task).await.ok().and_then(|r| r.ok());
        let go = match &early {
            Some(v) => decide(live, pinned, v, wake, expecting, from_others, summary == "you", enrolled(d) && summary != "you").0,
            None => wake,
        };
        if go {
            let v = wake_verdict(early.as_ref());
            deliver(d, device, &parts, &v, wake, None, ttl, !live).await;
            delivered = true;
            if !live && !pinned {
                crate::ambient::open_conversation(device, ttl);
            }
        }
    }
    if delivered && early.is_none() {
        let (d3, ids3) = (d.clone(), ids.clone());
        tokio::spawn(async move {
            let v = task.await.unwrap_or_else(|_| Verdict::inherited());
            mark_verdict(&d3, &ids3, &v, true);
        });
        *last = Some(Flushed {
            ids,
            ended: parts.last().map(|p| p.ended).unwrap_or(0.0),
            at: Instant::now(),
            delivered,
        });
        return;
    }
    let verdict = match early {
        Some(v) => v,
        None => task.await.unwrap_or_else(|_| Verdict::inherited()),
    };
    if let Some(last) = ids.last() {
        super::ambient_routes::mark(last, "classified");
    }
    if !delivered && routable {
        let (go, open) = decide(live, pinned, &verdict, wake, expecting, from_others, summary == "you", enrolled(d) && summary != "you");
        if go {
            if open {
                crate::ambient::open_conversation(device, ttl);
            }
            deliver(d, device, &parts, &verdict, wake, None, ttl, false).await;
            delivered = true;
        }
    }
    if !delivered && verdict.kept() && !from_others && verdict.memorable >= MEMORABLE_THRESHOLD {
        let (rendered, _) = render(&parts, &text);
        super::agent_routes::record_agent_message(d, crate::mission_control::SESSION_ID, "voice classifier", &rendered, false).await;
    }
    mark_verdict(d, &ids, &verdict, delivered);
    *last = Some(Flushed {
        ids,
        ended: parts.last().map(|p| p.ended).unwrap_or(0.0),
        at: Instant::now(),
        delivered,
    });
}

fn wake_verdict(early: Option<&Verdict>) -> Verdict {
    match early {
        Some(v) => Verdict {
            kind: v.kind.clone(),
            intent: v.intent.clone(),
            addressed: v.addressed.max(0.9),
            intent_confidence: v.intent_confidence,
            answers_pending: v.answers_pending,
            matches: v.matches,
            continues: v.continues,
            suggest: v.suggest,
            memorable: v.memorable,
            judge: v.judge,
        },
        None => Verdict { addressed: 0.9, ..Verdict::inherited() },
    }
}

fn render(parts: &[Part], fallback: &str) -> (String, String) {
    let lines: Vec<&Value> = parts.iter().flat_map(|p| p.speakers.iter()).collect();
    let plain = lines.len() <= 1
        && lines.iter().all(|l| l["is_user"] == true || l["speaker"].as_i64() == Some(-1));
    if lines.is_empty() || plain {
        return (fallback.to_string(), String::new());
    }
    let mut names: Vec<String> = Vec::new();
    let mut body = Vec::new();
    for l in lines {
        let label = l["label"].as_str().unwrap_or("Unknown").to_string();
        body.push(format!("{}: {}", label, l["text"].as_str().unwrap_or("")));
        if !names.contains(&label) {
            names.push(label);
        }
    }
    (body.join("\n"), names.join(", "))
}

fn context(d: &Shared, device: &str, ids: &[String]) -> String {
    let excluded = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let since = chrono::Utc::now().timestamp() - 60;
    let lines: Vec<String> = d
        .store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT text FROM ambient_events
                 WHERE device = ?1 AND status = 'done' AND kept = 1 AND delivered = 0 AND routed = 1
                   AND created_at >= ?2 AND text != '' AND id NOT IN (SELECT value FROM json_each(?3))
                 ORDER BY started_at DESC LIMIT 3",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![device, since, excluded], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    if lines.is_empty() {
        return String::new();
    }
    let body: Vec<String> = lines
        .into_iter()
        .rev()
        .map(|t| format!("[earlier, not sent] {}", ambient_watch::escape(&t.chars().take(200).collect::<String>())))
        .collect();
    format!("<context>{}</context>", body.join("\n"))
}

async fn deliver(
    d: &Shared,
    device: &str,
    parts: &[Part],
    v: &Verdict,
    wake: bool,
    continuation_of: Option<&[String]>,
    ttl: u32,
    opened: bool,
) {
    let _ = ttl;
    let ids: Vec<String> = parts.iter().map(|p| p.id.clone()).collect();
    if let Some(last) = ids.last() {
        super::ambient_routes::mark(last, "deliver");
    }
    let text = parts.iter().map(|p| p.text.trim()).collect::<Vec<_>>().join(" ");
    let (rendered, names) = render(parts, &text);
    let mode = if opened || crate::ambient::conversation().is_some() { "active" } else { "passive" };
    let summary = merge_summary(parts);
    let expecting = crate::ambient::expectation();
    crate::ambient::note_user_turn();
    let conversation = crate::ambient::conversation().map(|c| c.id).unwrap_or_else(|| "passive".to_string());
    let _ = crate::ambient::record_turn(&conversation, "user", &rendered, device);
    let mut attrs = format!(
        " mode=\"{mode}\" ids=\"{}\" parts=\"{}\" device=\"{}\" at=\"{}\" speaker=\"{}\" judged=\"{}/{}\" addressed=\"{:.2}\"",
        ambient_watch::escape(&ids.join(",")),
        parts.len(),
        ambient_watch::escape(device),
        ambient_watch::escape(parts.first().map(|p| p.started_at.as_str()).unwrap_or("")),
        summary,
        ambient_watch::escape(&v.kind),
        ambient_watch::escape(&v.intent),
        v.addressed
    );
    if !names.is_empty() {
        attrs.push_str(&format!(" speakers=\"{}\"", ambient_watch::escape(&names)));
    }
    if let Some(e) = &expecting {
        attrs.push_str(&format!(" expects=\"{}\" matched=\"{:.2}\"", ambient_watch::escape(&e.kind), v.matches));
    }
    if v.suggest >= 0.6 {
        attrs.push_str(" suggest=\"conversation\"");
    }
    if wake {
        attrs.push_str(" wake=\"true\"");
    }
    if let Some(parent) = continuation_of {
        attrs.push_str(&format!(" continuation_of=\"{}\"", ambient_watch::escape(&parent.join(","))));
    }
    if let Some(heard) = crate::speech::take_interruption() {
        attrs.push_str(&format!(" interrupted_reply=\"{}\"", ambient_watch::escape(&heard)));
    }
    let ctx = context(d, device, &ids);
    let envelope = format!("<ambient_turn{attrs}>{ctx}{}</ambient_turn>", ambient_watch::escape(&rendered));
    super::autonomy::deliver_to_mission_control(d, envelope).await;
    for id in &ids {
        let _ = d.store.with_connection(|c| {
            c.execute("UPDATE ambient_events SET delivered = 1 WHERE id = ?1", [id])?;
            Ok(())
        });
    }
}

fn claim_wake_speaker(d: &Shared, parts: &[Part]) {
    let phrases = d.config.lock().unwrap().ambient.wake_phrases.clone();
    let ids: std::collections::HashSet<i64> = parts
        .iter()
        .flat_map(|p| p.speakers.iter())
        .filter(|l| ambient_watch::wake_word(l["text"].as_str().unwrap_or(""), &phrases))
        .filter_map(|l| l["speaker"].as_i64())
        .filter(|id| *id != -1)
        .collect();
    if ids.len() == 1 {
        if let Some(id) = ids.into_iter().next() {
            let _ = crate::diarize::claim(&d.store, id, false);
        }
    }
}
