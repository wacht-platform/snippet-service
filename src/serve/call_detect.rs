use std::collections::{HashSet, VecDeque};

use super::speakers::Speaker;

pub(crate) const WINDOW_MS: i64 = 90_000;
pub(crate) const MIN_LINES: usize = 4;
pub(crate) const MIN_MEETING: usize = 1;
pub(crate) const MIN_SHARE: f64 = 0.75;
pub(crate) const MIN_SPAN_MS: i64 = 20_000;
pub(crate) const MEDIA_QUIET_MS: i64 = 120_000;
pub(crate) const QUIET_RESET_MS: i64 = 180_000;
const MIN_SCORED: usize = 3;
pub(crate) const ALT_MIN_TURNS: usize = 4;
pub(crate) const ALT_MIN_SWITCHES: usize = 3;
pub(crate) const ALT_MIN_SPAN_MS: i64 = 15_000;
pub(crate) const ALT_MAX_TURN_MS: i64 = 25_000;
const OTHER_PARTY: f64 = 0.6;
const PLAYED: f64 = 0.8;

const FILLERS: &[&str] = &[
    "hmm", "hm", "mm", "mmm", "mhmm", "uh", "uhh", "um", "umm", "ah", "oh", "er", "erm", "so", "huh", "eh", "ooh", "aah", "and", "but", "the", "a",
];

pub(crate) fn meaningful_words(text: &str) -> usize {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !FILLERS.contains(w)).count()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Signal {
    pub at_ms: i64,
    pub kind: String,
    pub intent: String,
    pub addressed: f64,
    pub played: f64,
    pub echo: f64,
    pub delivered: bool,
    pub wake: bool,
    pub media: bool,
    pub during_playback: bool,
    pub words: usize,
    pub user_voice: Option<f64>,
    pub other: f64,
    pub dur_ms: i64,
    pub speaker: Option<Speaker>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Party {
    You,
    Voice(u32),
    Other,
    Unknown,
}

fn party(s: &Signal) -> Party {
    match s.speaker {
        Some(Speaker::You) => Party::You,
        Some(Speaker::Voice(n)) => Party::Voice(n),
        // settle() lifts other_party for every note, so a remark alone is not evidence of a second person.
        _ if s.other >= OTHER_PARTY && s.user_voice.map_or(s.intent == "meeting", |v| v < 0.5) => Party::Other,
        _ => Party::Unknown,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Assistant,
    Played,
    Neutral,
    Meeting,
    Note,
    Unclear,
}

impl Class {
    pub fn call_like(self) -> bool {
        matches!(self, Class::Meeting | Class::Note)
    }
}

pub(crate) fn classify(s: &Signal) -> Class {
    let conversation = s.kind == "conversation";
    if s.wake || s.delivered || (conversation && s.intent == "for_assistant" && s.addressed >= 0.6) {
        return Class::Assistant;
    }
    if s.kind == "media" || s.media || s.played >= PLAYED {
        return Class::Played;
    }
    if s.kind == "noise" || s.kind == "background" || s.intent == "ignore" || s.words == 0 || s.during_playback || s.echo >= 0.5 {
        return Class::Neutral;
    }
    match s.intent.as_str() {
        "meeting" if conversation && s.addressed < 0.6 => Class::Meeting,
        "note" if conversation && s.addressed < 0.5 => Class::Note,
        _ => Class::Unclear,
    }
}

#[derive(Clone, Copy)]
struct Seen {
    at: i64,
    class: Class,
    voice: Option<f64>,
    party: Party,
    long: bool,
}

#[derive(Default)]
pub(crate) struct Detector {
    window: VecDeque<Seen>,
    last_played: Option<i64>,
    last_call_like: Option<i64>,
    cooling: bool,
}

pub(crate) struct Observed {
    pub class: Class,
    /// Start of the detected call (its first call-like line), when this line triggers one.
    pub trigger: Option<i64>,
}

impl Detector {
    pub fn observe(&mut self, s: &Signal) -> Observed {
        let class = classify(s);
        let at = s.at_ms;
        if class == Class::Played {
            self.last_played = Some(at);
        }
        if class == Class::Assistant {
            self.window.clear();
            return Observed { class, trigger: None };
        }
        self.window.push_back(Seen { at, class, voice: s.user_voice, party: party(s), long: s.dur_ms > ALT_MAX_TURN_MS });
        while self.window.front().is_some_and(|w| at - w.at > WINDOW_MS) {
            self.window.pop_front();
        }
        if class.call_like() {
            if self.cooling && self.last_call_like.is_some_and(|t| at - t > QUIET_RESET_MS) {
                self.cooling = false;
            }
            self.last_call_like = Some(at);
        }
        if self.cooling || self.last_played.is_some_and(|t| at - t < MEDIA_QUIET_MS) {
            return Observed { class, trigger: None };
        }
        let trigger = self.qualifies().or_else(|| self.alternates());
        if trigger.is_some() {
            self.cooling = true;
        }
        Observed { class, trigger }
    }

    fn qualifies(&self) -> Option<i64> {
        let calls: Vec<&Seen> = self.window.iter().filter(|w| w.class.call_like()).collect();
        let meeting = calls.iter().filter(|w| w.class == Class::Meeting).count();
        let against = self.window.iter().filter(|w| matches!(w.class, Class::Played | Class::Unclear)).count();
        let (first, last) = (calls.first()?.at, calls.last()?.at);
        let share = calls.len() as f64 / (calls.len() + against) as f64;
        let scored: Vec<f64> = self.window.iter().filter_map(|w| w.voice).collect();
        // Played dialogue has many voices but never the user's; a call always has the user in it.
        let user_absent = scored.len() >= MIN_SCORED && scored.iter().all(|v| *v < 0.5);
        (calls.len() >= MIN_LINES && meeting >= MIN_MEETING && share >= MIN_SHARE && last - first >= MIN_SPAN_MS && !user_absent)
            .then_some(first)
    }

    /// The user taking turns with one or more other live voices, even when the text alone is unclear.
    fn alternates(&self) -> Option<i64> {
        let turns: Vec<&Seen> = self
            .window
            .iter()
            .filter(|w| matches!(w.class, Class::Meeting | Class::Note | Class::Unclear) && !w.long && w.party != Party::Unknown)
            .collect();
        let parties: HashSet<Party> = turns.iter().map(|w| w.party).collect();
        let switches = turns.windows(2).filter(|p| p[0].party != p[1].party).count();
        let (first, last) = (turns.first()?.at, turns.last()?.at);
        (parties.contains(&Party::You)
            && parties.len() >= 2
            && turns.len() >= ALT_MIN_TURNS
            && switches >= ALT_MIN_SWITCHES
            && last - first >= ALT_MIN_SPAN_MS)
            .then_some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(at_s: i64, intent: &str) -> Signal {
        Signal { at_ms: at_s * 1000, kind: "conversation".into(), intent: intent.into(), addressed: 0.2, words: 4, ..Signal::default() }
    }

    fn run(d: &mut Detector, lines: &[Signal]) -> Option<i64> {
        lines.iter().find_map(|l| d.observe(l).trigger)
    }

    #[test]
    fn exchange_triggers_from_first_line() {
        let mut d = Detector::default();
        let lines: Vec<Signal> = (0..5).map(|i| line(i * 8, if i % 2 == 0 { "meeting" } else { "note" })).collect();
        assert_eq!(run(&mut d, &lines), Some(0));
    }

    #[test]
    fn one_stray_line_never_triggers() {
        let mut d = Detector::default();
        assert!(run(&mut d, &[line(0, "meeting")]).is_none());
        let mut d = Detector::default();
        assert!(run(&mut d, &[line(0, "meeting"), line(5, "meeting"), line(30, "meeting")]).is_none());
    }

    #[test]
    fn played_audio_never_triggers() {
        let mut d = Detector::default();
        let played: Vec<Signal> = (0..30).map(|i| Signal { kind: "media".into(), ..line(i * 5, "meeting") }).collect();
        assert!(run(&mut d, &played).is_none());
        let mut d = Detector::default();
        let scored: Vec<Signal> = (0..30).map(|i| Signal { played: 0.9, ..line(i * 5, "meeting") }).collect();
        assert!(run(&mut d, &scored).is_none());
    }

    #[test]
    fn dialogue_without_the_user_is_not_a_call() {
        let mut d = Detector::default();
        let tv: Vec<Signal> = (0..12).map(|i| Signal { user_voice: Some(0.05), ..line(i * 6, "meeting") }).collect();
        assert!(run(&mut d, &tv).is_none());
        let mut d = Detector::default();
        let call: Vec<Signal> =
            (0..12).map(|i| Signal { user_voice: Some(if i % 2 == 0 { 0.9 } else { 0.05 }), ..line(i * 6, "meeting") }).collect();
        assert!(run(&mut d, &call).is_some());
    }

    #[test]
    fn talking_to_the_assistant_resets_the_window() {
        let mut d = Detector::default();
        let mut lines: Vec<Signal> = (0..3).map(|i| line(i * 8, "meeting")).collect();
        lines.push(Signal { delivered: true, ..line(30, "for_assistant") });
        lines.extend((0..3).map(|i| line(40 + i * 8, "meeting")));
        assert!(run(&mut d, &lines).is_none());
    }

    #[test]
    fn recent_media_holds_off_and_prompt_does_not_repeat() {
        let mut d = Detector::default();
        let mut lines = vec![Signal { kind: "media".into(), ..line(0, "note") }];
        lines.extend((0..6).map(|i| line(10 + i * 8, "meeting")));
        assert!(run(&mut d, &lines).is_none());
        let mut d = Detector::default();
        let first: Vec<Signal> = (0..20).map(|i| line(i * 8, "meeting")).collect();
        assert_eq!(first.iter().filter(|l| d.observe(l).trigger.is_some()).count(), 1);
        let later: Vec<Signal> = (0..6).map(|i| line(160 + 200 + i * 8, "meeting")).collect();
        assert_eq!(run(&mut d, &later), Some(360 * 1000));
    }
}

#[cfg(test)]
mod replay {
    use super::*;

    const HISTORY: &str = include_str!("../../tests/fixtures/call_history.tsv");
    const LABELS: &str = include_str!("../../tests/fixtures/call_labels.tsv");

    /// Replays 8 days of real classified ambient history (no text) against hand labels.
    #[test]
    fn replay_eval_on_history() {
        let mut labels: Vec<(String, String, i64, i64)> = Vec::new();
        for row in LABELS.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let f: Vec<&str> = row.split('\t').collect();
            labels.push((f[0].into(), f[1].into(), f[2].parse().unwrap(), f[3].parse().unwrap()));
        }
        let mut detectors: std::collections::HashMap<String, Detector> = Default::default();
        let (mut first, mut last) = (i64::MAX, i64::MIN);
        let mut triggers = Vec::new();
        for row in HISTORY.lines().skip(1) {
            let f: Vec<&str> = row.split('\t').collect();
            let s = Signal {
                at_ms: f[1].parse().unwrap(),
                kind: f[3].into(),
                intent: f[4].into(),
                addressed: f[5].parse().unwrap(),
                delivered: f[6] == "1",
                during_playback: f[7] == "1",
                media: f[8] == "1",
                words: f[9].parse().unwrap(),
                wake: f[10] == "1",
                ..Signal::default()
            };
            first = first.min(s.at_ms);
            last = last.max(s.at_ms);
            if detectors.entry(f[0].to_string()).or_default().observe(&s).trigger.is_some() {
                triggers.push((f[0].to_string(), s.at_ms));
            }
        }
        let slack = 30_000;
        let inside = |kind: &str, dev: &str, t: i64| {
            labels.iter().position(|(k, d, a, b)| k == kind && d == dev && *a - slack <= t && t <= *b + slack)
        };
        let positives = labels.iter().filter(|l| l.0 == "pos").count();
        let mut caught = std::collections::HashSet::new();
        let mut false_prompts = 0;
        for (dev, t) in &triggers {
            if let Some(i) = inside("pos", dev, *t) {
                caught.insert(i);
            } else if inside("amb", dev, *t).is_none() {
                false_prompts += 1;
            }
        }
        let days = ((last - first) as f64 / 86_400_000.0).ceil();
        let scored = triggers.len() - triggers.iter().filter(|(d, t)| inside("pos", d, *t).is_none() && inside("amb", d, *t).is_some()).count();
        let precision = (scored - false_prompts) as f64 / scored.max(1) as f64;
        println!(
            "call replay: {} prompts over {days} days, recall {}/{positives} ({:.0}%), precision {:.0}%, false prompts {false_prompts} ({:.2}/day)",
            triggers.len(),
            caught.len(),
            100.0 * caught.len() as f64 / positives as f64,
            100.0 * precision,
            false_prompts as f64 / days
        );
        assert!(caught.len() >= 12, "recall regressed: {}/{positives}", caught.len());
        assert!(false_prompts <= 1, "false prompts regressed: {false_prompts}");
    }
}
