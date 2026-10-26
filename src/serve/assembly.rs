use std::time::{Duration, Instant};

pub(super) async fn transcribe_file(key: &str, path: &std::path::Path) -> Result<String, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().map_err(|e| e.to_string())?;
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("could not read audio: {e}"))?;
    let up: serde_json::Value = client
        .post("https://api.assemblyai.com/v2/upload")
        .header("authorization", key)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("AssemblyAI upload failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("AssemblyAI upload failed: {e}"))?;
    let url = up.get("upload_url").and_then(|u| u.as_str()).ok_or_else(|| format!("AssemblyAI upload rejected: {up}"))?;
    let made: serde_json::Value = client
        .post("https://api.assemblyai.com/v2/transcript")
        .header("authorization", key)
        .json(&serde_json::json!({ "audio_url": url, "speech_models": ["universal-3-pro", "universal-2"], "language_detection": true }))
        .send()
        .await
        .map_err(|e| format!("AssemblyAI request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("AssemblyAI request failed: {e}"))?;
    let id = made.get("id").and_then(|u| u.as_str()).ok_or_else(|| format!("AssemblyAI rejected the audio: {made}"))?.to_string();
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        tokio::time::sleep(Duration::from_millis(900)).await;
        let got: serde_json::Value = client
            .get(format!("https://api.assemblyai.com/v2/transcript/{id}"))
            .header("authorization", key)
            .send()
            .await
            .map_err(|e| format!("AssemblyAI poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("AssemblyAI poll failed: {e}"))?;
        match got.get("status").and_then(|x| x.as_str()) {
            Some("completed") => return Ok(got.get("text").and_then(|x| x.as_str()).unwrap_or("").trim().to_string()),
            Some("error") => return Err(format!("AssemblyAI could not transcribe: {}", got.get("error").and_then(|x| x.as_str()).unwrap_or("unknown error"))),
            _ => {}
        }
        if Instant::now() > deadline {
            return Err("AssemblyAI took too long to transcribe the audio".into());
        }
    }
}
