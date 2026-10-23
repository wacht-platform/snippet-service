use std::path::Path;
use std::sync::{Mutex, OnceLock};

use transcribe_rs::SpeechModel;
use transcribe_rs::TranscribeOptions;
use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::parakeet::ParakeetModel;

struct Loaded {
    id: String,
    used: std::time::Instant,
    model: Box<dyn SpeechModel + Send>,
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

fn load(id: &str) -> Result<Box<dyn SpeechModel + Send>, String> {
    let dir = crate::models::dir(id);
    match id {
        "parakeet-v3-int8" => ParakeetModel::load(&dir, &Quantization::Int8)
            .map(|m| Box::new(m) as Box<dyn SpeechModel + Send>)
            .map_err(|e| e.to_string()),
        _ => Err(format!("unknown speech model {id}")),
    }
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

pub fn unload_idle(after: std::time::Duration) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.used.elapsed() > after) {
        *guard = None;
    }
}
