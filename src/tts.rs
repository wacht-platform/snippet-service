use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const RATE: u32 = 24000;

pub fn audio_dir() -> std::path::PathBuf {
    crate::config::snippet_home().join("tts").join("replies")
}

const ABBREVIATIONS: &[&str] = &["mr", "mrs", "ms", "dr", "st", "vs", "etc", "e.g", "i.e", "prof", "no", "approx"];

fn splits_here(segment: &str, c: char, next: Option<char>) -> bool {
    if c == '\n' {
        return true;
    }
    let boundary = next.is_none_or(|n| n.is_whitespace() || matches!(n, '"' | '\'' | ')' | ']'));
    if !boundary {
        return false;
    }
    if c == '.' {
        let word = segment
            .split_whitespace()
            .next_back()
            .unwrap_or("")
            .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '.')
            .to_lowercase();
        if ABBREVIATIONS.contains(&word.as_str()) {
            return false;
        }
    }
    true
}

const RAMP_HOSTED: [usize; 2] = [6, 12];
const BREAK_BEFORE: &[&str] = &["to", "that", "and", "but", "so", "for", "with", "in", "on", "at", "of", "which", "because", "when", "if", "then"];

fn shorten(first: &str, limit: usize) -> Option<(String, String)> {
    let words: Vec<&str> = first.split_whitespace().collect();
    if words.len() <= limit {
        return None;
    }
    let clause = words.iter().take(limit + 1).position(|w| w.ends_with([',', ';', ':', '\u{2014}'])).filter(|i| i + 1 >= 3);
    let word = |j: usize| words[j].to_lowercase().trim_matches(|c: char| !c.is_alphanumeric()).to_string();
    let at = match clause {
        Some(i) => i + 1,
        None => (3..=(limit + 2).min(words.len() - 1))
            .filter(|j| BREAK_BEFORE.contains(&word(*j).as_str()))
            .min_by_key(|j| j.abs_diff(limit))
            .unwrap_or_else(|| {
                let mut at = limit;
                while at + 1 < words.len() && ["the", "a", "an", "of", "to", "and", "my", "your", "our", "its", "this", "that"].contains(&word(at - 1).as_str()) {
                    at += 1;
                }
                at
            }),
    };
    if at + 3 > words.len() {
        return None;
    }
    Some((words[..at].join(" "), words[at..].join(" ")))
}

pub fn chunk_text(text: &str) -> Vec<String> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, c) in chars.iter().copied().enumerate() {
        current.push(c);
        if matches!(c, '.' | '!' | '?') && splits_here(&current[..current.len() - c.len_utf8()], c, chars.get(i + 1).copied()) {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current);
    }
    let mut out: Vec<String> = Vec::new();
    for (i, sentence) in sentences.into_iter().enumerate() {
        let sentence = sentence.trim().to_string();
        if sentence.is_empty() {
            continue;
        }
        if i == 0 && sentence.split_whitespace().count() >= 5 {
            let mut cut = None;
            let mut words = 0;
            let mut last = ' ';
            for (idx, ch) in sentence.char_indices() {
                if ch == ' ' && !matches!(last, ',' | ';' | ':' | '\u{2014}') {
                    words += 1;
                }
                if matches!(ch, ',' | ';' | ':' | '\u{2014}') && words >= 2 && sentence[idx + ch.len_utf8()..].starts_with(' ') {
                    cut = Some(idx + ch.len_utf8());
                    break;
                }
                last = ch;
            }
            if let Some(at) = cut {
                let rest = sentence[at..].trim().to_string();
                out.push(sentence[..at].trim().to_string());
                if !rest.is_empty() {
                    out.push(rest);
                }
                continue;
            }
        }
        out.push(sentence);
    }
    let mut merged: Vec<String> = Vec::new();
    for piece in out {
        let keep_first = merged.len() == 1;
        match merged.last_mut() {
            Some(prev) if !keep_first && prev.split_whitespace().count() < 3 => {
                prev.push(' ');
                prev.push_str(&piece);
            }
            _ => merged.push(piece),
        }
    }
    let ramp = RAMP_HOSTED;
    for (i, limit) in ramp.into_iter().enumerate() {
        if let Some(piece) = merged.get(i).cloned() {
            if let Some((head, tail)) = shorten(&piece, limit) {
                merged[i] = head;
                merged.insert(i + 1, tail);
            }
        }
    }
    let mut final_chunks: Vec<String> = Vec::new();
    for piece in merged {
        let mut rest = piece.as_str();
        while rest.len() > 160 {
            let cut = rest[..160].rfind(' ').unwrap_or(160);
            final_chunks.push(rest[..cut].trim().to_string());
            rest = rest[cut..].trim_start();
        }
        if !rest.is_empty() {
            final_chunks.push(rest.to_string());
        }
    }
    final_chunks
}

fn trim_quiet(mut v: Vec<f32>) -> Vec<f32> {
    const FLOOR: f32 = 0.004;
    const KEEP: usize = 360;
    let first = v.iter().position(|s| s.abs() > FLOOR).unwrap_or(0);
    let last = v.iter().rposition(|s| s.abs() > FLOOR).unwrap_or(v.len().saturating_sub(1));
    let end = (last + KEEP).min(v.len());
    v.truncate(end);
    v.drain(..first.saturating_sub(KEEP));
    v
}
static REMOTE: Mutex<Option<(String, String)>> = Mutex::new(None);
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

pub fn apply_config(c: &crate::config::SnippetConfig) {
    let key = c.deepgram_api_key.clone().filter(|k| !k.trim().is_empty());
    let voice = if c.ambient.tts_voice.starts_with("aura") { c.ambient.tts_voice.clone() } else { "aura-2-thalia-en".to_string() };
    *REMOTE.lock().unwrap() = key.map(|k| (k, voice));
    crate::tts_stream::apply_config(c);
}

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(None)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default()
    })
}
async fn deepgram(key: &str, voice: &str, text: &str) -> Result<(Vec<u8>, u32), String> {
    let resp = http()
        .post("https://api.deepgram.com/v1/speak")
        .query(&[("model", voice), ("encoding", "linear16"), ("sample_rate", "24000"), ("container", "none")])
        .header("Authorization", format!("Token {key}"))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body).chars().take(200).collect::<String>()));
    }
    let samples: Vec<f32> = body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    if samples.is_empty() {
        return Err("empty audio".into());
    }
    let samples = trim_quiet(samples);
    let ms = (samples.len() as u64 * 1000 / RATE as u64) as u32;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
    Ok((bytes, ms))
}
pub fn prewarm() {
    if REMOTE.lock().unwrap().is_none() {
        return;
    }
    tokio::spawn(async {
        let _ = http().get("https://api.deepgram.com/").timeout(std::time::Duration::from_secs(4)).send().await;
    });
}
pub async fn speak_pcm(text: &str) -> Result<(Vec<u8>, u32), String> {
    let (key, voice) = REMOTE.lock().unwrap().clone().ok_or("set deepgram_api_key in the config to enable spoken replies")?;
    let started = Instant::now();
    let done = deepgram(&key, &voice, text).await?;
    eprintln!("[ambient-tts] deepgram {}ms for {}ms of audio", started.elapsed().as_millis(), done.1);
    Ok(done)
}

pub async fn clean_old_replies() {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    let mut stack = vec![audio_dir()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if meta.modified().is_ok_and(|m| m < cutoff) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
        if dir != audio_dir() {
            let _ = tokio::fs::remove_dir(&dir).await;
        }
    }
}
