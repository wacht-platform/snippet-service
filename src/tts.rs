use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use piper_rs::Piper;

pub const VOICES: &[(&str, &str)] = &[
    ("en_US-lessac-medium", "Lessac (US)"),
    ("en_US-amy-medium", "Amy (US)"),
    ("en_GB-alan-medium", "Alan (UK)"),
    ("en_GB-jenny_dioco-medium", "Jenny (UK)"),
];

struct Loaded {
    voice: String,
    piper: Piper,
}

static ENGINE: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Loaded>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub fn is_known(voice: &str) -> bool {
    voice == "system" || VOICES.iter().any(|(id, _)| *id == voice)
}

fn voice_dir(voice: &str) -> PathBuf {
    crate::config::snippet_home().join("tts").join(voice)
}

pub fn audio_dir() -> PathBuf {
    crate::config::snippet_home().join("tts").join("replies")
}

async fn ensure_voice(voice: &str) -> Result<(PathBuf, PathBuf), String> {
    let dir = voice_dir(voice);
    let model = dir.join(format!("{voice}.onnx"));
    let config = dir.join(format!("{voice}.onnx.json"));
    if model.exists() && config.exists() {
        return Ok((model, config));
    }
    let mut parts = voice.split('-');
    let region = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    let quality = parts.next().unwrap_or_default();
    let lang = region.split('_').next().unwrap_or_default();
    if lang.is_empty() || name.is_empty() || quality.is_empty() {
        return Err(format!("unknown voice {voice}"));
    }
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let base = format!("https://huggingface.co/rhasspy/piper-voices/resolve/main/{lang}/{region}/{name}/{quality}/{voice}");
    for (url, target) in [(format!("{base}.onnx.json"), &config), (format!("{base}.onnx"), &model)] {
        let response = reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("voice download failed: {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        let part = target.with_extension("part");
        tokio::fs::write(&part, &bytes).await.map_err(|e| e.to_string())?;
        tokio::fs::rename(&part, target).await.map_err(|e| e.to_string())?;
    }
    Ok((model, config))
}

pub async fn prepare(voice: &str) -> Result<(), String> {
    let (model, config) = ensure_voice(voice).await?;
    let voice = voice.to_string();
    tokio::task::spawn_blocking(move || {
        let mut guard = engine().lock().unwrap();
        if guard.as_ref().is_some_and(|l| l.voice == voice) {
            return Ok(());
        }
        let piper = Piper::new(&model, &config).map_err(|e| e.to_string())?;
        *guard = Some(Loaded { voice, piper });
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
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
    prepare(voice).await?;
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
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
