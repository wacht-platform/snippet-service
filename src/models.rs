use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct Entry {
    pub id: &'static str,
    pub kind: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub size_mb: u32,
    source: Source,
}

enum Source {
    Archive(&'static str),
    Piper(&'static str),
}

pub const CATALOG: &[Entry] = &[
    Entry {
        id: "moonshine-base",
        kind: "stt",
        name: "Moonshine base",
        detail: "English. Small and fast.",
        size_mb: 74,
        source: Source::Archive("https://blob.handy.computer/moonshine-base.tar.gz"),
    },
    Entry {
        id: "parakeet-v3-int8",
        kind: "stt",
        name: "Parakeet v3",
        detail: "Most accurate. English and 24 other European languages.",
        size_mb: 640,
        source: Source::Archive("https://blob.handy.computer/parakeet-v3-int8.tar.gz"),
    },
    Entry { id: "en_US-lessac-medium", kind: "tts", name: "Lessac (US)", detail: "English, US", size_mb: 63, source: Source::Piper("en_US-lessac-medium") },
    Entry { id: "en_US-amy-medium", kind: "tts", name: "Amy (US)", detail: "English, US", size_mb: 63, source: Source::Piper("en_US-amy-medium") },
    Entry { id: "en_GB-alan-medium", kind: "tts", name: "Alan (UK)", detail: "English, UK", size_mb: 63, source: Source::Piper("en_GB-alan-medium") },
    Entry { id: "en_GB-jenny_dioco-medium", kind: "tts", name: "Jenny (UK)", detail: "English, UK", size_mb: 63, source: Source::Piper("en_GB-jenny_dioco-medium") },
];

#[derive(Clone, Default)]
struct Progress {
    done: u64,
    total: u64,
    running: bool,
    error: Option<String>,
}

static PROGRESS: OnceLock<Mutex<HashMap<String, Progress>>> = OnceLock::new();

fn progress() -> &'static Mutex<HashMap<String, Progress>> {
    PROGRESS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn find(id: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.id == id)
}

pub fn dir(id: &str) -> PathBuf {
    crate::config::snippet_home().join("models").join(id)
}

pub fn is_ready(id: &str) -> bool {
    find(id).is_some() && dir(id).join(".ready").exists()
}

pub fn list() -> Vec<Value> {
    let map = progress().lock().unwrap();
    CATALOG
        .iter()
        .map(|e| {
            let p = map.get(e.id).cloned().unwrap_or_default();
            let status = if is_ready(e.id) {
                "ready"
            } else if p.running {
                "downloading"
            } else if p.error.is_some() {
                "failed"
            } else {
                "missing"
            };
            json!({
                "id": e.id,
                "kind": e.kind,
                "name": e.name,
                "detail": e.detail,
                "size_mb": e.size_mb,
                "status": status,
                "done": p.done,
                "total": p.total,
                "error": p.error,
            })
        })
        .collect()
}

fn set(id: &str, f: impl FnOnce(&mut Progress)) {
    let mut map = progress().lock().unwrap();
    f(map.entry(id.to_string()).or_default());
}

pub fn start_download(id: &str) -> Result<(), String> {
    let entry = find(id).ok_or("unknown model")?;
    if is_ready(id) {
        return Ok(());
    }
    {
        let mut map = progress().lock().unwrap();
        let p = map.entry(id.to_string()).or_default();
        if p.running {
            return Ok(());
        }
        *p = Progress { running: true, ..Default::default() };
    }
    let id = id.to_string();
    tokio::spawn(async move {
        let result = download(entry).await;
        set(&id, |p| {
            p.running = false;
            p.error = result.err();
        });
    });
    Ok(())
}

async fn fetch(url: &str, target: &std::path::Path, id: &str, base: u64) -> Result<u64, String> {
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("download failed: {}", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    set(id, |p| p.total = p.total.max(base + total));
    let mut file = tokio::fs::File::create(target).await.map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();
    let mut got = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await.map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        set(id, |p| p.done = base + got);
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await.map_err(|e| e.to_string())?;
    Ok(got)
}

async fn download(entry: &'static Entry) -> Result<(), String> {
    let target = dir(entry.id);
    let _ = tokio::fs::remove_dir_all(&target).await;
    tokio::fs::create_dir_all(&target).await.map_err(|e| e.to_string())?;
    match &entry.source {
        Source::Piper(voice) => {
            let mut parts = voice.split('-');
            let region = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default();
            let quality = parts.next().unwrap_or_default();
            let lang = region.split('_').next().unwrap_or_default();
            let base = format!(
                "https://huggingface.co/rhasspy/piper-voices/resolve/main/{lang}/{region}/{name}/{quality}/{voice}"
            );
            let config = target.join(format!("{voice}.onnx.json"));
            let model = target.join(format!("{voice}.onnx"));
            let first = fetch(&format!("{base}.onnx.json"), &config, entry.id, 0).await?;
            fetch(&format!("{base}.onnx"), &model, entry.id, first).await?;
        }
        Source::Archive(url) => {
            let archive = target.join("download.tar.gz");
            fetch(url, &archive, entry.id, 0).await?;
            let dest = target.clone();
            let file = archive.clone();
            tokio::task::spawn_blocking(move || extract(&file, &dest))
                .await
                .map_err(|e| e.to_string())??;
            let _ = tokio::fs::remove_file(&archive).await;
        }
    }
    tokio::fs::write(target.join(".ready"), b"ok").await.map_err(|e| e.to_string())
}

fn extract(archive: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let parts: Vec<_> = path.components().collect();
        if parts.len() < 2 {
            continue;
        }
        let relative: PathBuf = parts[1..].iter().collect();
        let name = relative.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with("._") || relative.components().any(|c| c.as_os_str() == "..") {
            continue;
        }
        let out = dest.join(&relative);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub async fn delete(id: &str) -> Result<(), String> {
    find(id).ok_or("unknown model")?;
    crate::stt::unload(id);
    crate::tts::unload(id);
    let _ = tokio::fs::remove_dir_all(dir(id)).await;
    set(id, |p| *p = Progress::default());
    Ok(())
}
