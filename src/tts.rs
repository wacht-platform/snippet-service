use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use ort::session::Session;
use ort::value::Tensor;

const ESPEAK_DATA: &[u8] = include_bytes!("../assets/espeak-ng-data-en.tar.gz");
const VOCAB: &str = include_str!("../assets/kokoro-vocab.json");
const RATE: u32 = 24000;
const MAX_TOKENS: usize = 400;
const GAP_SAMPLES: usize = RATE as usize / 12;

struct Loaded {
    used: Instant,
    session: Session,
    style: Vec<f32>,
}

static ENGINE: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();
static ESPEAK: OnceLock<Result<(), String>> = OnceLock::new();
static VOCAB_MAP: OnceLock<HashMap<char, i64>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Loaded>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

fn vocab() -> &'static HashMap<char, i64> {
    VOCAB_MAP.get_or_init(|| {
        let raw: HashMap<String, i64> = serde_json::from_str(VOCAB).unwrap_or_default();
        raw.into_iter().filter_map(|(k, v)| k.chars().next().map(|c| (c, v))).collect()
    })
}

pub fn audio_dir() -> std::path::PathBuf {
    crate::config::snippet_home().join("tts").join("replies")
}

pub fn prepare() -> Result<(), String> {
    ESPEAK
        .get_or_init(|| {
            let root = crate::config::snippet_home().join("espeak");
            if !root.join("espeak-ng-data").join("phontab").exists() {
                std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                let gz = flate2::read::GzDecoder::new(ESPEAK_DATA);
                tar::Archive::new(gz).unpack(&root).map_err(|e| e.to_string())?;
            }
            unsafe { std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &root) };
            Ok(())
        })
        .clone()
}

pub fn unload(voice: &str) {
    let mut guard = engine().lock().unwrap();
    if voice == crate::models::TTS_VOICE {
        *guard = None;
    }
}

pub fn unload_idle(after: std::time::Duration) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|l| l.used.elapsed() > after) {
        *guard = None;
    }
}

pub fn warm(voice: &str) -> Result<(), String> {
    let mut guard = engine().lock().unwrap();
    if guard.is_some() {
        return Ok(());
    }
    prepare()?;
    let dir = crate::models::dir(voice);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(6);
    let session = Session::builder()
        .map_err(|e| e.to_string())?
        .with_intra_threads(threads)
        .map_err(|e| e.to_string())?
        .commit_from_file(dir.join("kokoro.onnx"))
        .map_err(|e| e.to_string())?;
    let style: Vec<f32> = std::fs::read(dir.join("style.bin"))
        .map_err(|e| e.to_string())?
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    *guard = Some(Loaded { used: Instant::now(), session, style });
    Ok(())
}

fn clause_tokens(segment: &str, punct: Option<char>) -> Result<Vec<i64>, String> {
    let mut phonemes = String::new();
    if !segment.trim().is_empty() {
        let parts = espeak_rs::text_to_phonemes(segment.trim(), "en-us", None).map_err(|e| e.to_string())?;
        phonemes.push_str(&parts.join(" "));
    }
    if let Some(p) = punct {
        phonemes.push(p);
    }
    phonemes.push(' ');
    let map = vocab();
    Ok(phonemes.chars().filter_map(|c| map.get(&c).copied()).collect())
}

fn groups(text: &str) -> Result<Vec<Vec<i64>>, String> {
    let mut clauses: Vec<Vec<i64>> = Vec::new();
    let mut segment = String::new();
    for c in text.chars() {
        if matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | '\n') {
            let punct = if c == '\n' { Some('.') } else { Some(c) };
            let tokens = clause_tokens(&segment, punct)?;
            if tokens.len() > 1 {
                clauses.push(tokens);
            }
            segment.clear();
        } else {
            segment.push(c);
        }
    }
    let tail = clause_tokens(&segment, None)?;
    if tail.len() > 1 {
        clauses.push(tail);
    }
    let mut out: Vec<Vec<i64>> = Vec::new();
    for clause in clauses {
        for piece in clause.chunks(MAX_TOKENS) {
            match out.last_mut() {
                Some(last) if last.len() + piece.len() <= MAX_TOKENS => last.extend_from_slice(piece),
                _ => out.push(piece.to_vec()),
            }
        }
    }
    Ok(out)
}

fn run(loaded: &mut Loaded, ids: &[i64]) -> Result<Vec<f32>, String> {
    let n = ids.len();
    let start = n * 256;
    let style = loaded
        .style
        .get(start..start + 256)
        .ok_or("voice style is out of range")?
        .to_vec();
    let mut tokens = Vec::with_capacity(n + 2);
    tokens.push(0i64);
    tokens.extend_from_slice(ids);
    tokens.push(0);
    let outputs = loaded
        .session
        .run(ort::inputs![
            "tokens" => Tensor::from_array(([1usize, tokens.len()], tokens)).map_err(|e| e.to_string())?,
            "style" => Tensor::from_array(([1usize, 256usize], style)).map_err(|e| e.to_string())?,
            "speed" => Tensor::from_array(([1usize], vec![1.0f32])).map_err(|e| e.to_string())?,
        ])
        .map_err(|e| e.to_string())?;
    let (_, samples) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    Ok(samples.to_vec())
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
        let pieces = groups(&text)?;
        if pieces.is_empty() {
            return Err("nothing to say".to_string());
        }
        let mut guard = engine().lock().unwrap();
        let loaded = guard.as_mut().ok_or("voice is not loaded")?;
        loaded.used = Instant::now();
        let mut samples: Vec<f32> = Vec::new();
        for ids in &pieces {
            if !samples.is_empty() {
                samples.extend(std::iter::repeat_n(0.0f32, GAP_SAMPLES));
            }
            samples.extend(run(loaded, ids)?);
        }
        Ok(wav(&samples, RATE))
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
