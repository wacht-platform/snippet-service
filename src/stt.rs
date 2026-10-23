use std::path::Path;
use std::sync::{Mutex, OnceLock};

use transcribe_rs::SpeechModel;
use transcribe_rs::TranscribeOptions;
use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity};

struct Loaded {
    id: String,
    used: std::time::Instant,
    model: ParakeetModel,
}

#[derive(Debug, Clone)]
pub struct Word {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

static ENGINE: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Loaded>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub fn unload(id: &str) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.id == id) {
        *guard = None;
    }
}

pub fn unload_idle(after: std::time::Duration) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.used.elapsed() > after) {
        *guard = None;
    }
}

fn load(id: &str) -> Result<ParakeetModel, String> {
    if id != crate::models::STT_MODEL {
        return Err(format!("unknown speech model {id}"));
    }
    ParakeetModel::load(&crate::models::dir(id), &Quantization::Int8).map_err(|e| e.to_string())
}

pub fn warm(id: &str) -> Result<(), String> {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.id == id) {
        return Ok(());
    }
    *guard = Some(Loaded { id: id.to_string(), used: std::time::Instant::now(), model: load(id)? });
    Ok(())
}

pub async fn transcribe(id: &str, path: &Path) -> Result<String, String> {
    if !crate::models::is_ready(id) {
        return Err("the speech model is not downloaded".into());
    }
    let id = id.to_string();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        warm(&id)?;
        let mut guard = engine().lock().unwrap();
        let loaded = guard.as_mut().ok_or("speech model is not loaded")?;
        loaded.used = std::time::Instant::now();
        let result = loaded
            .model
            .transcribe_file(
                &path,
                &TranscribeOptions {
                    leading_silence_ms: Some(500),
                    trailing_silence_ms: Some(1000),
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(result.text.trim().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

const EXTRA_LEAD: f32 = 0.25;

pub async fn transcribe_words(id: &str, samples: Vec<f32>) -> Result<(String, Vec<Word>), String> {
    if !crate::models::is_ready(id) {
        return Err("the speech model is not downloaded".into());
    }
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        warm(&id)?;
        let mut guard = engine().lock().unwrap();
        let loaded = guard.as_mut().ok_or("speech model is not loaded")?;
        loaded.used = std::time::Instant::now();
        let lead = (EXTRA_LEAD * 16000.0) as usize;
        let mut padded = vec![0.0f32; lead];
        padded.extend_from_slice(&samples);
        padded.extend(std::iter::repeat_n(0.0f32, 16000));
        let result = loaded
            .model
            .transcribe_with(
                &padded,
                &ParakeetParams { timestamp_granularity: Some(TimestampGranularity::Word), ..Default::default() },
            )
            .map_err(|e| e.to_string())?;
        let words = result
            .segments
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.text.trim().is_empty())
            .map(|s| Word { start: (s.start - EXTRA_LEAD).max(0.0), end: (s.end - EXTRA_LEAD).max(0.0), text: s.text.trim().to_string() })
            .collect();
        Ok((result.text.trim().to_string(), words))
    })
    .await
    .map_err(|e| e.to_string())?
}
