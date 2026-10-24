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

pub(super) struct Spoken {
    pub text: String,
    pub speakers: String,
    pub summary: &'static str,
}

pub(super) async fn transcribe_ambient(d: &Shared, path: &Path) -> Result<Spoken, String> {
    let owned = path.to_path_buf();
    let samples = tokio::task::spawn_blocking(move || crate::diarize::read_audio_16k(&owned))
        .await
        .map_err(|e| e.to_string())??;
    let ready = crate::models::is_ready(crate::models::SPEAKER_MODEL);
    let words = crate::stt::transcribe_words(crate::models::STT_MODEL, samples.clone());
    let analysis = {
        let d = d.clone();
        tokio::task::spawn_blocking(move || {
            if ready { crate::diarize::analyze(&d.store, &samples) } else { Ok(Vec::new()) }
        })
    };
    let (words, analysis) = tokio::join!(words, analysis);
    let (text, words) = words?;
    let text = fix_wake_spelling(&collapse_repeats(&text));
    let words: Vec<crate::stt::Word> = {
        let mut kept: Vec<crate::stt::Word> = Vec::new();
        let mut run = 0;
        for w in words {
            if kept.last().is_some_and(|l| l.text.eq_ignore_ascii_case(&w.text)) {
                run += 1;
                if run >= 3 {
                    continue;
                }
            } else {
                run = 0;
            }
            kept.push(w);
        }
        kept
    };
    let text = if is_filler(&text) && crate::ambient::expectation().is_none() && crate::ambient::conversation().is_none() { String::new() } else { text };
    let turns = match analysis.map_err(|e| e.to_string())? {
        Ok(turns) => turns,
        Err(e) => {
            eprintln!("[ambient] speaker analysis failed: {e}");
            Vec::new()
        }
    };
    if text.is_empty() {
        return Ok(Spoken { text, speakers: String::new(), summary: "unknown" });
    }
    let infos = crate::diarize::speakers(&d.store);
    let enrolled = infos.iter().any(|s| s.is_user);
    let owner = |w: &crate::stt::Word| -> i64 {
        let mid = (w.start + w.end) / 2.0;
        turns
            .iter()
            .min_by(|a, b| {
                let da = if mid < a.start { a.start - mid } else if mid > a.end { mid - a.end } else { 0.0 };
                let db = if mid < b.start { b.start - mid } else if mid > b.end { mid - b.end } else { 0.0 };
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|t| t.speaker)
            .unwrap_or(-1)
    };
    let mut lines: Vec<(i64, String, f32, f32)> = Vec::new();
    for w in &words {
        let who = owner(w);
        match lines.last_mut() {
            Some(last) if last.0 == who => {
                last.1.push(' ');
                last.1.push_str(&w.text);
                last.3 = w.end;
            }
            _ => lines.push((who, w.text.clone(), w.start, w.end)),
        }
    }
    if lines.is_empty() {
        lines.push((-1, text.clone(), 0.0, 0.0));
    } else if lines.len() == 1 {
        lines[0].1 = text.clone();
    }
    let named = |id: i64| infos.iter().find(|s| s.id == id);
    let has_user = lines.iter().any(|l| named(l.0).is_some_and(|s| s.is_user));
    let has_other = lines.iter().any(|l| l.0 != -1 && !named(l.0).is_some_and(|s| s.is_user));
    let distinct: std::collections::HashSet<i64> = lines.iter().map(|l| l.0).filter(|id| *id != -1).collect();
    let summary = if enrolled {
        match (has_user, has_other) {
            (true, false) => "you",
            (false, true) => "other",
            (true, true) => "mixed",
            _ => "unknown",
        }
    } else if distinct.len() > 1 {
        "mixed"
    } else {
        "unknown"
    };
    let speakers = serde_json::Value::Array(
        lines
            .iter()
            .map(|(id, t, a, b)| {
                let info = named(*id);
                serde_json::json!({
                    "speaker": id,
                    "label": info.map(crate::diarize::display_name).unwrap_or_else(|| "Unknown".to_string()),
                    "is_user": info.is_some_and(|s| s.is_user),
                    "text": t,
                    "start": a,
                    "end": b,
                })
            })
            .collect(),
    )
    .to_string();
    Ok(Spoken { text, speakers, summary })
}

fn collapse_repeats(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut run = 0;
    for w in text.split_whitespace() {
        let key = |x: &str| x.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if out.last().is_some_and(|l| key(l) == key(w) && !key(w).is_empty()) {
            run += 1;
            if run >= 3 {
                continue;
            }
        } else {
            run = 0;
        }
        out.push(w);
    }
    out.join(" ")
}

fn is_filler(text: &str) -> bool {
    const FILLER: [&str; 12] = ["uh", "um", "okay", "ok", "wow", "hmm", "well", "oh", "ah", "mm", "yeah", "huh"];
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    !words.is_empty() && words.len() <= 3 && words.iter().all(|w| FILLER.contains(&w.as_str()))
}

fn fix_wake_spelling(text: &str) -> String {
    let lower = text.to_lowercase();
    for variant in ["snip it", "snip but", "snip bit", "snippit", "snipit", "snip pet"] {
        if lower.starts_with(variant) && !lower[variant.len()..].starts_with(|c: char| c.is_alphanumeric()) {
            return format!("Snippet{}", &text[variant.len()..]);
        }
    }
    text.to_string()
}
