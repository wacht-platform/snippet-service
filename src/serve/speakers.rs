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
pub(crate) const YOU: f64 = 0.8;
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
    you_ms: Option<i64>,
    next: u32,
    cache: VecDeque<(String, Speaker)>,
}

impl Speakers {
    pub fn assign(&mut self, key: &str, at_ms: i64, voice: Option<&VoiceScore>, played: bool) -> Speaker {
        if let Some((_, s)) = self.cache.iter().find(|(k, _)| k == key) {
            return *s;
        }
        if voice.is_none() {
            return Speaker::Unknown;
        }
        let s = self.decide(at_ms, voice, played, true);
        self.cache.push_back((key.to_string(), s));
        while self.cache.len() > CACHE {
            self.cache.pop_front();
        }
        s
    }

    pub fn peek(&mut self, at_ms: i64, voice: Option<&VoiceScore>) -> Speaker {
        self.decide(at_ms, voice, false, false)
    }

    pub fn present(&self, now_ms: i64) -> Vec<String> {
        let mut out = Vec::new();
        if self.you_ms.is_some_and(|t| now_ms - t <= KEEP_MS) {
            out.push(Speaker::You.label());
        }
        out.extend(self.clusters.iter().filter(|c| now_ms - c.last_ms <= KEEP_MS).map(|c| Speaker::Voice(c.n).label()));
        out
    }

    fn decide(&mut self, at_ms: i64, voice: Option<&VoiceScore>, played: bool, learn: bool) -> Speaker {
        if learn {
            self.clusters.retain(|c| at_ms - c.last_ms <= KEEP_MS);
            if self.clusters.is_empty() {
                self.next = 0;
            }
        }
        let Some(v) = voice.filter(|v| v.neural) else { return Speaker::Unknown };
        let Some(emb) = v.embedding.as_deref() else { return Speaker::Unknown };
        if v.likelihood.is_some_and(|l| l >= YOU) {
            if learn {
                self.you_ms = Some(at_ms);
            }
            return Speaker::You;
        }
        let best = self
            .clusters
            .iter_mut()
            .map(|c| (cosine(emb, &c.centroid), c))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        let need = if v.voiced_secs >= FOUND_SECS { JOIN } else { JOIN_SHORT };
        if let Some((_, c)) = best.filter(|(sim, _)| *sim >= need) {
            if learn {
                let w = 1.0 / (c.count + 1) as f32;
                for (x, y) in c.centroid.iter_mut().zip(emb) {
                    *x = *x * (1.0 - w) + y * w;
                }
                let norm = c.centroid.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                c.centroid.iter_mut().for_each(|x| *x /= norm);
                c.count += 1;
                c.last_ms = at_ms;
            }
            return Speaker::Voice(c.n);
        }
        if !learn || played || v.voiced_secs < FOUND_SECS || v.likelihood.is_some_and(|l| l >= 0.5) {
            return Speaker::Unknown;
        }
        self.next += 1;
        self.clusters.push(Cluster { n: self.next, centroid: emb.to_vec(), count: 1, last_ms: at_ms });
        Speaker::Voice(self.next)
    }
}

fn book() -> std::sync::MutexGuard<'static, HashMap<String, Speakers>> {
    static BOOK: std::sync::OnceLock<Mutex<HashMap<String, Speakers>>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn assign(device: &str, key: &str, at_ms: i64, voice: Option<&VoiceScore>, played: bool) -> Speaker {
    book().entry(device.to_string()).or_default().assign(key, at_ms, voice, played)
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
}
