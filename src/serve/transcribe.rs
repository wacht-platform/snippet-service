use super::*;

use std::path::{Path, PathBuf};
use tokio::process::Command;

const MAX_ATTACHMENTS: usize = 5;
const MAX_AUDIO_DURATION_SECONDS: f64 = 3.0 * 60.0;

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
        return Err(format!("a message can contain at most {MAX_ATTACHMENTS} attachments"));
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
    let duration = audio_duration_seconds(path).await?;
    if duration >= MAX_AUDIO_DURATION_SECONDS {
        return Err(format!(
            "audio attachment `{}` must be 3 minutes or shorter",
            path.display()
        ));
    }
    let audio = tokio::fs::read(path)
        .await
        .map_err(|e| format!("could not read audio attachment: {e}"))?;

    d.reload_config().await;
    let key = d
        .config
        .lock()
        .unwrap()
        .assemblyai_api_key
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "assemblyai_api_key is not configured in config.toml".to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| format!("could not create transcription client: {e}"))?;

    let upload_response = client
        .post("https://api.assemblyai.com/v2/upload")
        .header("authorization", &key)
        .header("content-type", "application/octet-stream")
        .body(audio)
        .send()
        .await
        .map_err(|e| format!("AssemblyAI upload request failed: {e}"))?;
    if !upload_response.status().is_success() {
        let status = upload_response.status();
        let body = upload_response.text().await.unwrap_or_default();
        return Err(format!("AssemblyAI upload failed ({status}): {body}"));
    }
    let upload: serde_json::Value = upload_response
        .json()
        .await
        .map_err(|e| format!("invalid AssemblyAI upload response: {e}"))?;
    let upload_url = upload
        .get("upload_url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "AssemblyAI upload response did not contain upload_url".to_string())?;

    let submit_response = client
        .post("https://api.assemblyai.com/v2/transcript")
        .header("authorization", &key)
        .json(&serde_json::json!({"audio_url": upload_url}))
        .send()
        .await
        .map_err(|e| format!("AssemblyAI transcript request failed: {e}"))?;
    if !submit_response.status().is_success() {
        let status = submit_response.status();
        let body = submit_response.text().await.unwrap_or_default();
        return Err(format!("AssemblyAI transcript submission failed ({status}): {body}"));
    }
    let submitted: serde_json::Value = submit_response
        .json()
        .await
        .map_err(|e| format!("invalid AssemblyAI transcript response: {e}"))?;
    let id = submitted
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "AssemblyAI response did not contain transcript id".to_string())?;
    let poll_url = format!("https://api.assemblyai.com/v2/transcript/{id}");

    for _ in 0..300 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let response = client
            .get(&poll_url)
            .header("authorization", &key)
            .send()
            .await
            .map_err(|e| format!("AssemblyAI polling failed: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("AssemblyAI polling failed ({status}): {body}"));
        }
        let result: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("invalid AssemblyAI polling response: {e}"))?;
        match result.get("status").and_then(serde_json::Value::as_str) {
            Some("completed") => {
                return Ok(result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string());
            }
            Some("error") => {
                return Err(result
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("AssemblyAI transcription failed")
                    .to_string());
            }
            _ => {}
        }
    }
    Err("AssemblyAI transcription timed out".to_string())
}
