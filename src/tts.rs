use std::sync::{Mutex, OnceLock};

use piper_rs::Piper;

struct Loaded {
    voice: String,
    piper: Piper,
}

static ENGINE: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Loaded>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub fn audio_dir() -> std::path::PathBuf {
    crate::config::snippet_home().join("tts").join("replies")
}

pub fn unload(voice: &str) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.voice == voice) {
        *guard = None;
    }
}

pub fn warm(voice: &str) -> Result<(), String> {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.voice == voice) {
        return Ok(());
    }
    let dir = crate::models::dir(voice);
    let piper = Piper::new(&dir.join(format!("{voice}.onnx")), &dir.join(format!("{voice}.onnx.json")))
        .map_err(|e| e.to_string())?;
    *guard = Some(Loaded { voice: voice.to_string(), piper });
    Ok(())
}

fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend(b"RIFF");
    out.extend((36 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

pub async fn synthesize(voice: &str, text: &str) -> Result<Vec<u8>, String> {
    if !crate::models::is_ready(voice) {
        return Err("the reply voice is not downloaded".into());
    }
    let voice = voice.to_string();
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        warm(&voice)?;
        let mut guard = engine().lock().unwrap();
        let loaded = guard.as_mut().ok_or("voice is not loaded")?;
        let (samples, rate) = loaded
            .piper
            .create(&text, false, None, None, None, None)
            .map_err(|e| e.to_string())?;
        Ok(wav(&samples, rate))
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn clean_old_replies() {
    let dir = audio_dir();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(modified) = entry.metadata().await.and_then(|m| m.modified()) {
            if modified < cutoff {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
}
