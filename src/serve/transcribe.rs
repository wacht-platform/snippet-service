use super::*;

use std::path::{Path, PathBuf};
use tokio::process::Command;

const MAX_ATTACHMENTS: usize = 5;

fn is_audio_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("aac" | "flac" | "m4a" | "mp3" | "oga" | "ogg" | "opus" | "wav" | "webm")
    )
}

fn attachment_path(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    if !(trimmed.starts_with("[attached image —") || trimmed.starts_with("[attached file —"))
        || !trimmed.ends_with(']')
    {
        return None;
    }
    let path = trimmed.rsplit_once(": ")?.1.trim_end_matches(']').trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

async fn audio_duration_seconds(path: &Path) -> Result<f64, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|e| format!("could not inspect audio duration: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "could not inspect audio duration".to_string()
        } else {
            format!("could not inspect audio duration: {detail}")
        });
    }
    let seconds = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "could not determine audio duration".to_string())?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("could not determine audio duration".to_string());
    }
    Ok(seconds)
}

/// Expand uploaded audio attachments before the message enters the harness.
/// The original attachment markers remain in the message, so every model sees
/// both the transcript and the source attachment reference.
pub(super) async fn prepare_message(d: &Daemon, text: String) -> Result<String, String> {
    let markers: Vec<PathBuf> = text.lines().filter_map(attachment_path).collect();
    if markers.len() > MAX_ATTACHMENTS {
        return Err(format!(
            "a message can contain at most {MAX_ATTACHMENTS} attachments"
        ));
    }
    let audio_paths: Vec<PathBuf> = markers
        .iter()
        .filter(|path| is_audio_path(path))
        .cloned()
        .collect();
    if audio_paths.is_empty() {
        return Ok(text);
    }

    let mut out = text;
    for path in audio_paths {
        let transcript = transcribe_audio_file(d, &path).await?;
        out.push_str(&format!(
            "\n\n[Audio transcript for {}]\n{}",
            path.display(),
            if transcript.is_empty() {
                "(no speech detected)"
            } else {
                &transcript
            }
        ));
    }
    Ok(out)
}

async fn transcribe_audio_file(d: &Daemon, path: &Path) -> Result<String, String> {
    d.reload_config().await;
    let voice = d.config.lock().unwrap().voice.clone();
    if !voice.enabled {
        return Err("voice notes are turned off in Settings → Voice".to_string());
    }
    if !crate::models::is_ready(crate::models::STT_MODEL) {
        return Err("download the speech model in Settings → Voice to use voice notes".to_string());
    }
    let duration = audio_duration_seconds(path).await?;
    let limit = voice.max_seconds.max(crate::config::VOICE_MIN_SECONDS);
    if duration >= limit as f64 + 1.0 {
        return Err(format!(
            "audio attachment `{}` is longer than the {} limit set in Settings → Voice",
            path.display(),
            if limit % 60 == 0 { format!("{}-minute", limit / 60) } else { format!("{limit}-second") }
        ));
    }
    let wav = std::env::temp_dir().join(format!("snippet-voice-{}.wav", uuid::Uuid::new_v4()));
    let converted = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-ar", "16000", "-ac", "1"])
        .arg(&wav)
        .output()
        .await
        .map_err(|e| format!("could not convert audio: {e}"))?;
    if !converted.status.success() {
        let _ = tokio::fs::remove_file(&wav).await;
        return Err(format!("could not convert audio: {}", String::from_utf8_lossy(&converted.stderr).trim()));
    }
    let result = crate::stt::transcribe(crate::models::STT_MODEL, &wav).await;
    let _ = tokio::fs::remove_file(&wav).await;
    result
}

pub(super) async fn transcribe_ambient(_d: &Daemon, path: &Path) -> Result<String, String> {
    crate::stt::transcribe(crate::models::STT_MODEL, path).await
}
