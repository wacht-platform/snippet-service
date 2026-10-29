use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

use super::neural::{self, Embedder};
use super::{VoiceprintAccumulator, VoiceprintSample};

pub const MIN_VOICED: f32 = 1.0;
pub const STRONG_VOICED: f32 = 2.0;
const MAX_SECS: f32 = 6.0;
const ENROL_SECS: f32 = 20.0;
const DEADLINE: Duration = Duration::from_millis(300);
const IDLE: Duration = Duration::from_secs(600);
const RETRY: Duration = Duration::from_secs(600);

pub const OTHER_SIM: f32 = 0.20;
pub const USER_SIM: f32 = 0.60;
const MID_SIM: f32 = 0.35;
const SLOPE: f32 = 0.06;

#[derive(Debug, Clone)]
pub struct VoiceScore {
    pub neural: bool,
    pub similarity: Option<f32>,
    pub likelihood: Option<f64>,
    pub voiced_secs: f32,
    pub embedding: Option<Arc<[f32]>>,
    pub ms: u32,
}

impl VoiceScore {
    pub fn confident_other(&self) -> bool {
        self.neural && self.voiced_secs >= STRONG_VOICED && self.similarity.is_some_and(|s| s < OTHER_SIM)
    }

    pub fn confident_user(&self) -> bool {
        self.neural && self.voiced_secs >= STRONG_VOICED && self.similarity.is_some_and(|s| s >= USER_SIM)
    }
}

pub fn likelihood(similarity: f32) -> f64 {
    let z = (similarity - MID_SIM) / SLOPE;
    (((1.0 / (1.0 + (-z).exp())) * 100.0).round() / 100.0) as f64
}

type Slot = watch::Sender<Option<Arc<VoiceScore>>>;

#[derive(Clone)]
pub struct VoiceHandle {
    rx: watch::Receiver<Option<Arc<VoiceScore>>>,
    deadline: Instant,
}

impl VoiceHandle {
    pub fn ready(&self) -> Option<Arc<VoiceScore>> {
        self.rx.borrow().clone()
    }

    pub async fn wait(&self) -> Option<Arc<VoiceScore>> {
        let mut rx = self.rx.clone();
        let got = tokio::time::timeout_at(self.deadline.into(), rx.wait_for(|v| v.is_some())).await;
        match got {
            Ok(Ok(v)) => v.clone(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(timeout: Duration) -> Self {
        let (tx, rx) = watch::channel(None);
        std::mem::forget(tx);
        Self { rx, deadline: Instant::now() + timeout }
    }
}

enum Job {
    Score { id: String, pcm: Vec<f32>, refs: Vec<Arc<[f32]>>, legacy: Vec<VoiceprintSample>, tx: Slot, at: Instant },
    Embed { pcm: Vec<f32>, reply: oneshot::Sender<Result<(Vec<f32>, f32), String>> },
    Warm,
}

fn sender() -> &'static Mutex<Option<mpsc::Sender<Job>>> {
    static TX: OnceLock<Mutex<Option<mpsc::Sender<Job>>>> = OnceLock::new();
    TX.get_or_init(Default::default)
}

fn loaded() -> &'static std::sync::atomic::AtomicBool {
    static L: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &L
}

pub fn neural_ready() -> bool {
    loaded().load(std::sync::atomic::Ordering::Relaxed)
}

fn submit(job: Job) {
    let mut guard = sender().lock().unwrap();
    let job = match guard.as_ref() {
        Some(tx) => match tx.send(job) {
            Ok(()) => return,
            Err(mpsc::SendError(job)) => job,
        },
        None => job,
    };
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("voiceprint".into()).spawn(move || worker(rx));
    if spawned.is_ok() {
        let _ = tx.send(job);
        *guard = Some(tx);
    }
}

pub fn warm() {
    submit(Job::Warm);
}

/// Scores a finalised utterance in the background. `None` when there is nothing to compare against.
pub fn start(id: &str, device: &str, pcm: &[u8], manual: &[VoiceprintSample]) -> Option<VoiceHandle> {
    let mut refs: Vec<Arc<[f32]>> = manual
        .iter()
        .filter(|s| s.model == neural::MODEL && s.vector.len() == neural::DIM)
        .map(|s| Arc::from(s.vector.as_slice()))
        .collect();
    refs.extend(super::store::adaptive(device));
    let legacy: Vec<VoiceprintSample> = manual.iter().filter(|s| s.model.is_empty() && s.vector.len() == super::DIM).cloned().collect();
    if refs.is_empty() && legacy.is_empty() {
        return None;
    }
    let (tx, rx) = watch::channel(None);
    let at = Instant::now();
    submit(Job::Score { id: id.to_string(), pcm: neural::to_f32(pcm), refs, legacy, tx, at });
    Some(VoiceHandle { rx, deadline: at + DEADLINE })
}

/// Neural embedding of an enrolment clip; returns the vector and voiced seconds.
pub async fn embed_enrolment(pcm: &[u8]) -> Result<(Vec<f32>, f32), String> {
    let (reply, rx) = oneshot::channel();
    submit(Job::Embed { pcm: neural::to_f32(pcm), reply });
    match tokio::time::timeout(Duration::from_secs(120), rx).await {
        Ok(Ok(r)) => r,
        _ => Err("voiceprint model timed out".into()),
    }
}

struct State {
    model: Option<Embedder>,
    failed: Option<Instant>,
}

impl State {
    fn model(&mut self) -> Option<&mut Embedder> {
        if self.model.is_none() && self.failed.is_none_or(|t| t.elapsed() > RETRY) {
            let t = Instant::now();
            match Embedder::load() {
                Ok(m) => {
                    eprintln!("[voiceprint] loaded {} in {}ms", neural::MODEL, t.elapsed().as_millis());
                    self.model = Some(m);
                    self.failed = None;
                    loaded().store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("[voiceprint] model unavailable, using legacy scorer: {e}");
                    self.failed = Some(Instant::now());
                }
            }
        }
        self.model.as_mut()
    }

    fn unload(&mut self) {
        if self.model.take().is_some() {
            loaded().store(false, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[voiceprint] unloaded after idle");
        }
    }
}

fn worker(rx: mpsc::Receiver<Job>) {
    let mut st = State { model: None, failed: None };
    loop {
        let job = if st.model.is_some() {
            match rx.recv_timeout(IDLE) {
                Ok(j) => j,
                Err(RecvTimeoutError::Timeout) => {
                    st.unload();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(j) => j,
                Err(_) => return,
            }
        };
        match job {
            Job::Warm => {
                st.model();
            }
            Job::Embed { pcm, reply } => {
                let (voiced, clip) = neural::voiced(&pcm, ENROL_SECS);
                let r = if voiced < MIN_VOICED {
                    Err("Not enough clear speech detected — speak naturally for 2–3 seconds and try again".to_string())
                } else {
                    match st.model() {
                        Some(m) => m.embed(&clip).map(|e| (e, voiced)),
                        None => Err("voiceprint model unavailable".into()),
                    }
                };
                let _ = reply.send(r);
            }
            Job::Score { id, pcm, refs, legacy, tx, at } => {
                if tx.is_closed() {
                    continue;
                }
                let score = score(&mut st, &pcm, &refs, &legacy);
                let ms = at.elapsed().as_millis() as u32;
                let score = VoiceScore { ms, ..score };
                crate::ambient::mark(&id, "voice");
                eprintln!(
                    "[voiceprint] {} {} sim={} user_voice={} voiced={:.1}s refs={} {}ms",
                    &id[..id.len().min(8)],
                    if score.neural { "neural" } else { "legacy" },
                    score.similarity.map_or("-".into(), |s| format!("{s:.3}")),
                    score.likelihood.map_or("-".into(), |s| format!("{s:.2}")),
                    score.voiced_secs,
                    refs.len(),
                    ms
                );
                let _ = tx.send(Some(Arc::new(score)));
            }
        }
    }
}

fn score(st: &mut State, pcm: &[f32], refs: &[Arc<[f32]>], legacy: &[VoiceprintSample]) -> VoiceScore {
    let (voiced, clip) = neural::voiced(pcm, MAX_SECS);
    let none = VoiceScore { neural: false, similarity: None, likelihood: None, voiced_secs: voiced, embedding: None, ms: 0 };
    if voiced < MIN_VOICED {
        return none;
    }
    if !refs.is_empty() {
        if let Some(m) = st.model() {
            return match m.embed(&clip) {
                Ok(emb) => {
                    let sim = similarity(&emb, refs);
                    VoiceScore { neural: true, similarity: Some(sim), likelihood: Some(likelihood(sim)), embedding: Some(emb.into()), ..none }
                }
                Err(e) => {
                    eprintln!("[voiceprint] embed failed: {e}");
                    none
                }
            };
        }
    }
    if legacy.is_empty() || st.model.is_some() || st.failed.is_none() {
        return none;
    }
    let mut acc = VoiceprintAccumulator::default();
    let bytes: Vec<u8> = clip.iter().flat_map(|s| (*s as i16).to_le_bytes()).collect();
    acc.feed_pcm(&bytes);
    VoiceScore { likelihood: acc.score(legacy), ..none }
}

/// Cosine to the centroid of the reference embeddings.
pub fn similarity(emb: &[f32], refs: &[Arc<[f32]>]) -> f32 {
    let mut c = vec![0f32; emb.len()];
    for r in refs {
        for (a, b) in c.iter_mut().zip(r.iter()) {
            *a += b;
        }
    }
    let n = c.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
    neural::cosine(emb, &c) / n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(model: &str, dim: usize) -> VoiceprintSample {
        VoiceprintSample { id: "x".into(), label: "x".into(), created_at: 0, model: model.into(), audio_path: None, vector: vec![0.1; dim] }
    }

    #[test]
    fn no_comparable_samples_means_no_job() {
        assert!(start("t", "dev-none", &[0u8; 64000], &[sample("wespeaker-resnet34-lm-v0", 256)]).is_none());
        assert!(start("t", "dev-none", &[0u8; 64000], &[sample(neural::MODEL, 24)]).is_none());
        assert!(start("t", "dev-none", &[0u8; 64000], &[]).is_none());
    }

    #[test]
    fn unavailable_model_falls_back_to_legacy_or_none() {
        let pcm: Vec<f32> = (0..16000 * 3).map(|i| ((i as f32) * 0.07).sin() * 8000.0 * (1.0 + ((i as f32) * 0.0013).sin())).collect();
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| (*s as i16).to_le_bytes()).collect();
        let mut acc = VoiceprintAccumulator::default();
        acc.feed_pcm(&bytes);
        let legacy = VoiceprintSample { vector: acc.finish_vector().unwrap(), ..sample("", 0) };
        let refs: Vec<Arc<[f32]>> = vec![Arc::from(vec![0.0625f32; 256].as_slice())];
        let mut st = State { model: None, failed: Some(Instant::now()) };
        let s = score(&mut st, &pcm, &refs, &[legacy]);
        assert!(!s.neural && s.similarity.is_none() && s.likelihood.is_some());
        let s = score(&mut st, &pcm, &refs, &[]);
        assert!(!s.neural && s.likelihood.is_none());
    }

    #[test]
    fn short_clip_is_none() {
        let mut st = State { model: None, failed: Some(Instant::now()) };
        let pcm: Vec<f32> = (0..16000 * 3).map(|i| if i < 12000 { ((i as f32) * 0.07).sin() * 8000.0 } else { 0.0 }).collect();
        let refs: Vec<Arc<[f32]>> = vec![Arc::from(vec![0.0625f32; 256].as_slice())];
        let s = score(&mut st, &pcm, &refs, &[]);
        assert!(s.voiced_secs < MIN_VOICED);
        assert!(s.likelihood.is_none() && s.similarity.is_none());
    }

    #[test]
    fn likelihood_is_monotonic_and_centred() {
        assert!(likelihood(OTHER_SIM) < 0.1);
        assert!(likelihood(USER_SIM) > 0.9);
        assert!((likelihood(MID_SIM) - 0.5).abs() < 0.01);
    }

    #[test]
    fn confidence_needs_enough_audio() {
        let s = VoiceScore { neural: true, similarity: Some(0.0), likelihood: Some(0.0), voiced_secs: 1.5, embedding: None, ms: 0 };
        assert!(!s.confident_other());
        assert!(VoiceScore { voiced_secs: 2.5, ..s.clone() }.confident_other());
        assert!(!VoiceScore { neural: false, voiced_secs: 2.5, ..s }.confident_other());
    }
}
