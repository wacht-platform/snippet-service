use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::VoiceprintSample;
use super::engine::VoiceScore;
use super::neural;

const CAP: usize = 20;
const MIN_GAP_SECS: i64 = 30;

type Adaptive = HashMap<String, Vec<VoiceprintSample>>;

fn dir() -> PathBuf {
    crate::config::snippet_home().join("voiceprints")
}

fn index() -> PathBuf {
    dir().join("adaptive.json")
}

fn cache() -> &'static Mutex<Option<Adaptive>> {
    static C: OnceLock<Mutex<Option<Adaptive>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn with<R>(f: impl FnOnce(&mut Adaptive) -> R) -> R {
    let mut guard = cache().lock().unwrap();
    let map = guard.get_or_insert_with(|| {
        std::fs::read(index()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    });
    f(map)
}

fn save(map: &Adaptive) {
    let path = index();
    let _ = std::fs::create_dir_all(dir());
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(map) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub(super) fn adaptive(device: &str) -> Vec<Arc<[f32]>> {
    with(|m| {
        m.get(device)
            .map(|v| {
                v.iter()
                    .filter(|s| s.model == neural::MODEL && s.vector.len() == neural::DIM)
                    .map(|s| Arc::from(s.vector.as_slice()))
                    .collect()
            })
            .unwrap_or_default()
    })
}

pub fn adaptive_count(device: &str) -> usize {
    with(|m| m.get(device).map_or(0, |v| v.iter().filter(|s| s.model == neural::MODEL).count()))
}

/// Adds a confidently-matched utterance as a per-device sample (blocking file IO).
pub fn adapt(device: &str, score: &VoiceScore, audio: Option<&Path>) -> bool {
    let Some(emb) = score.embedding.as_ref().filter(|_| score.confident_user()) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    let recent = with(|m| m.get(device).and_then(|v| v.last()).is_some_and(|s| now - s.created_at < MIN_GAP_SECS));
    if recent {
        return false;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let audio_path = audio.and_then(|src| {
        let dst = dir().join(format!("{id}.wav"));
        std::fs::create_dir_all(dir()).ok()?;
        std::fs::copy(src, &dst).ok()?;
        Some(dst.to_string_lossy().into_owned())
    });
    let sample = VoiceprintSample { id, label: "adaptive".into(), created_at: now, model: neural::MODEL.into(), audio_path, vector: emb.to_vec() };
    with(|m| {
        let v = m.entry(device.to_string()).or_default();
        v.push(sample);
        while v.len() > CAP {
            let old = v.remove(0);
            delete_audio(&old.audio_path);
        }
        save(m);
    });
    true
}

pub fn clear_adaptive() {
    with(|m| {
        for s in m.values().flatten() {
            delete_audio(&s.audio_path);
        }
        m.clear();
        save(m);
    });
}

pub fn store_enrolment_audio(pcm: &[u8]) -> Result<String, String> {
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let path = dir().join(format!("{}.wav", uuid::Uuid::new_v4()));
    std::fs::write(&path, super::wav(pcm)).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn delete_audio(path: &Option<String>) {
    if let Some(p) = path.as_deref().filter(|p| Path::new(p).starts_with(dir())) {
        let _ = std::fs::remove_file(p);
    }
}
