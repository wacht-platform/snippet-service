use std::sync::{Mutex, OnceLock};

pub fn audio_dir() -> std::path::PathBuf {
    crate::config::snippet_home().join("tts").join("replies")
}

static HOST: Mutex<String> = Mutex::new(String::new());
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

pub fn deepgram_host() -> String {
    let h = HOST.lock().unwrap();
    if h.is_empty() { "api.deepgram.com".to_string() } else { h.clone() }
}

pub fn apply_config(c: &crate::config::SnippetConfig) {
    *HOST.lock().unwrap() = c
        .ambient
        .deepgram_host
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
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

pub fn prewarm() {
    if !crate::tts_stream::enabled() {
        return;
    }
    tokio::spawn(async {
        let _ = http()
            .get(format!("https://{}/", deepgram_host()))
            .timeout(std::time::Duration::from_secs(4))
            .send()
            .await;
    });
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
