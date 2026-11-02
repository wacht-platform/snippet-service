use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::voiceprint::VoiceScore;
use crate::voiceprint::neural::cosine;

pub(crate) const KEEP_MS: i64 = 600_000;
/// Same-voice cosine to a cluster centroid; sits between the engine's OTHER_SIM (0.20) and USER_SIM (0.60).
pub(crate) const JOIN: f32 = 0.55;
/// A short clip only joins a voice it matches closely; it never founds one.
pub(crate) const JOIN_SHORT: f32 = 0.65;
pub(crate) const FOUND_SECS: f32 = 2.0;
pub(crate) const YOU: f64 = 0.65;
const CACHE: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Speaker {
    You,
    Voice(u32),
    Unknown,
}

impl Speaker {
    pub fn label(self) -> String {
        match self {
            Speaker::You => "You".into(),
            Speaker::Voice(n) => format!("Voice {n}"),
            Speaker::Unknown => "unknown".into(),
        }
    }
}

struct Cluster {
    n: u32,
    centroid: Vec<f32>,
    count: u32,
    last_ms: i64,
}

/// Rolling voice clusters for one device, over the last ten minutes.
#[derive(Default)]
pub(crate) struct Speakers {
    clusters: Vec<Cluster>,
    stt_tags: Vec<(String, Speaker, i64)>,
    you_ms: Option<i64>,
    next: u32,
    cache: VecDeque<(String, Speaker)>,
}

impl Speakers {
    #[cfg(test)]
    pub fn assign(&mut self, key: &str, at_ms: i64, voice: Option<&VoiceScore>, played: bool) -> Speaker {
        self.assign_with_tag(key, at_ms, voice, None, played)
    }

    pub fn assign_with_tag(
        &mut self,
        key: &str,
        at_ms: i64,
        voice: Option<&VoiceScore>,
        stt_speaker: Option<&str>,
        played: bool,
    ) -> Speaker {
        if let Some((_, s)) = self.cache.iter().find(|(k, _)| k == key) {
            return *s;
        }
        if voice.is_none() && stt_speaker.is_none() {
            return Speaker::Unknown;
        }
        let s = self.decide(at_ms, voice, stt_speaker, played, true);
        if s != Speaker::Unknown || voice.is_some() {
            self.cache.push_back((key.to_string(), s));
            while self.cache.len() > CACHE {
                self.cache.pop_front();
            }
        }
        s
    }

    pub fn peek(&mut self, at_ms: i64, voice: Option<&VoiceScore>) -> Speaker {
        self.decide(at_ms, voice, None, false, false)
    }

    pub fn present(&self, now_ms: i64) -> Vec<String> {
        let mut out = Vec::new();
        if self.you_ms.is_some_and(|t| now_ms - t <= KEEP_MS) {
            out.push(Speaker::You.label());
        }
        for c in self.clusters.iter().filter(|c| now_ms - c.last_ms <= KEEP_MS) {
            let lbl = Speaker::Voice(c.n).label();
            if !out.contains(&lbl) {
                out.push(lbl);
            }
        }
        for (_, sp, _) in self.stt_tags.iter().filter(|(_, _, t)| now_ms - *t <= KEEP_MS) {
            if *sp != Speaker::Unknown {
                let lbl = sp.label();
                if !out.contains(&lbl) {
                    out.push(lbl);
                }
            }
        }
        out
    }

    fn decide(
        &mut self,
        at_ms: i64,
        voice: Option<&VoiceScore>,
        stt_speaker: Option<&str>,
        played: bool,
        learn: bool,
    ) -> Speaker {
        let tag = stt_speaker.map(str::trim).filter(|s| !s.is_empty());
        if learn {
            self.clusters.retain(|c| at_ms - c.last_ms <= KEEP_MS);
            self.stt_tags.retain(|(_, _, t)| at_ms - *t <= KEEP_MS);
            if self.clusters.is_empty() && self.stt_tags.is_empty() {
                self.next = 0;
            }
        }
        if let Some(v) = voice.filter(|v| v.neural)
            && let Some(emb) = v.embedding.as_deref()
        {
            if v.likelihood.is_some_and(|l| l >= YOU) {
                if learn {
                    self.you_ms = Some(at_ms);
                    if let Some(t) = tag {
                        self.record_tag(t, Speaker::You, at_ms);
                    }
                }
                return Speaker::You;
            }
            if v.likelihood.is_some_and(|l| l >= 0.5) {
                if let Some(t) = tag
                    && let Some(s) = self.lookup_tag(t, at_ms, learn)
                {
                    return s;
                }
                return Speaker::Unknown;
            }
            let best = self
                .clusters
                .iter_mut()
                .map(|c| (cosine(emb, &c.centroid), c))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            let need = if v.voiced_secs >= FOUND_SECS { JOIN } else { JOIN_SHORT };
            if let Some((_, c)) = best.filter(|(sim, _)| *sim >= need) {
                let sp = Speaker::Voice(c.n);
                if learn {
                    let w = 1.0 / (c.count + 1) as f32;
                    for (x, y) in c.centroid.iter_mut().zip(emb) {
                        *x = *x * (1.0 - w) + y * w;
                    }
                    let norm = c.centroid.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                    c.centroid.iter_mut().for_each(|x| *x /= norm);
                    c.count += 1;
                    c.last_ms = at_ms;
                    if let Some(t) = tag {
                        self.record_tag(t, sp, at_ms);
                    }
                }
                return sp;
            }
            if learn && !played && v.voiced_secs >= FOUND_SECS {
                let n = if let Some(t) = tag
                    && let Some(Speaker::Voice(existing)) = self.lookup_tag(t, at_ms, false)
                {
                    existing
                } else {
                    self.next += 1;
                    self.next
                };
                self.clusters.push(Cluster { n, centroid: emb.to_vec(), count: 1, last_ms: at_ms });
                let sp = Speaker::Voice(n);
                if let Some(t) = tag {
                    self.record_tag(t, sp, at_ms);
                }
                return sp;
            }
        }
        if let Some(t) = tag {
            if let Some(s) = self.lookup_tag(t, at_ms, learn) {
                return s;
            }
            if learn && !played {
                self.next += 1;
                let sp = Speaker::Voice(self.next);
                self.record_tag(t, sp, at_ms);
                return sp;
            }
        }
        Speaker::Unknown
    }

    fn lookup_tag(&mut self, tag: &str, at_ms: i64, learn: bool) -> Option<Speaker> {
        let entry = self.stt_tags.iter_mut().find(|(k, _, t)| k == tag && at_ms - *t <= KEEP_MS)?;
        if learn {
            entry.2 = at_ms;
            if entry.1 == Speaker::You {
                self.you_ms = Some(at_ms);
            } else if let Speaker::Voice(n) = entry.1
                && let Some(c) = self.clusters.iter_mut().find(|c| c.n == n)
            {
                c.last_ms = at_ms;
            }
        }
        Some(entry.1)
    }

    fn record_tag(&mut self, tag: &str, sp: Speaker, at_ms: i64) {
        if let Some(entry) = self.stt_tags.iter_mut().find(|(k, _, _)| k == tag) {
            entry.1 = sp;
            entry.2 = at_ms;
        } else {
            self.stt_tags.push((tag.to_string(), sp, at_ms));
        }
    }
}

fn book() -> std::sync::MutexGuard<'static, HashMap<String, Speakers>> {
    static BOOK: std::sync::OnceLock<Mutex<HashMap<String, Speakers>>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn assign_with_tag(
    device: &str,
    key: &str,
    at_ms: i64,
    voice: Option<&VoiceScore>,
    stt_speaker: Option<&str>,
    played: bool,
) -> Speaker {
    book().entry(device.to_string()).or_default().assign_with_tag(key, at_ms, voice, stt_speaker, played)
}

pub(crate) fn peek(device: &str, at_ms: i64, voice: Option<&VoiceScore>) -> (Speaker, Vec<String>) {
    let mut b = book();
    let s = b.entry(device.to_string()).or_default();
    (s.peek(at_ms, voice), s.present(at_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn unit(seed: &[f32]) -> Vec<f32> {
        let n = seed.iter().map(|x| x * x).sum::<f32>().sqrt();
        seed.iter().map(|x| x / n).collect()
    }

    fn voice(emb: &[f32], likelihood: f64, secs: f32) -> VoiceScore {
        VoiceScore { neural: true, similarity: None, likelihood: Some(likelihood), voiced_secs: secs, embedding: Some(Arc::from(unit(emb))), ms: 0 }
    }

    #[test]
    fn groups_voices_and_keeps_you_apart() {
        let mut s = Speakers::default();
        let a = [1.0, 0.1, 0.0];
        let b = [0.0, 1.0, 0.1];
        assert_eq!(s.assign("1", 0, Some(&voice(&[0.0, 0.0, 1.0], 0.95, 3.0)), false), Speaker::You);
        assert_eq!(s.assign("2", 1000, Some(&voice(&a, 0.05, 3.0)), false), Speaker::Voice(1));
        assert_eq!(s.assign("3", 2000, Some(&voice(&b, 0.05, 3.0)), false), Speaker::Voice(2));
        assert_eq!(s.assign("4", 3000, Some(&voice(&[0.9, 0.2, 0.05], 0.05, 2.5)), false), Speaker::Voice(1));
        assert_eq!(s.assign("2", 9000, None, false), Speaker::Voice(1), "cached per line");
        assert_eq!(s.present(4000), ["You", "Voice 1", "Voice 2"]);
    }

    #[test]
    fn short_or_missing_clips_are_unknown_never_a_guess() {
        let mut s = Speakers::default();
        assert_eq!(s.assign("1", 0, None, false), Speaker::Unknown);
        assert_eq!(s.assign("2", 0, Some(&voice(&[1.0, 0.0, 0.0], 0.05, 1.2)), false), Speaker::Unknown);
        assert_eq!(s.assign("3", 0, Some(&voice(&[1.0, 0.0, 0.0], 0.05, 3.0)), false), Speaker::Voice(1));
        assert_eq!(s.assign("4", 0, Some(&voice(&[0.5, 0.85, 0.0], 0.05, 1.2)), false), Speaker::Unknown);
        assert_eq!(s.assign("5", 0, Some(&voice(&[0.0, 1.0, 0.0], 0.05, 3.0)), true), Speaker::Unknown, "played audio founds no voice");
    }

    #[test]
    fn voices_expire_after_ten_minutes() {
        let mut s = Speakers::default();
        assert_eq!(s.assign("1", 0, Some(&voice(&[1.0, 0.0, 0.0], 0.05, 3.0)), false), Speaker::Voice(1));
        assert_eq!(s.assign("2", KEEP_MS + 1, Some(&voice(&[0.0, 1.0, 0.0], 0.05, 3.0)), false), Speaker::Voice(1));
        assert!(s.present(KEEP_MS + 1).len() == 1);
    }

    #[test]
    fn stt_diarization_tags_anchor_to_you_and_resolve_short_turns() {
        let mut s = Speakers::default();
        let u = voice(&[0.0, 0.0, 1.0], 0.92, 3.0);
        let other = voice(&[1.0, 0.0, 0.0], 0.05, 3.0);
        assert_eq!(s.assign_with_tag("1", 1000, Some(&u), Some("A"), false), Speaker::You);
        assert_eq!(s.assign_with_tag("2", 2000, Some(&other), Some("B"), false), Speaker::Voice(1));
        // Short <1s clips with no neural score still resolve via anchored STT speaker tags
        assert_eq!(s.assign_with_tag("3", 3000, None, Some("A"), false), Speaker::You);
        assert_eq!(s.assign_with_tag("4", 4000, None, Some("B"), false), Speaker::Voice(1));
        // New diarized speaker "C" without neural embedding founds Voice 2
        assert_eq!(s.assign_with_tag("5", 5000, None, Some("C"), false), Speaker::Voice(2));
        assert_eq!(s.present(5000), ["You", "Voice 1", "Voice 2"]);
    }
}
