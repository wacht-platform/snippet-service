pub async fn run(args: Vec<String>) {
    let raw = std::fs::read_to_string(std::path::Path::new(&std::env::var("HOME").unwrap()).join(".snippet/config.toml")).unwrap();
    let mut c: crate::config::SnippetConfig = toml::from_str(&raw).unwrap();
    c.ambient = c.ambient.normalized().unwrap();
    let p = super::flux_stt::params(&c).expect("deepgram key");
    let device = "replay-device";
    if std::env::var("TTS_IDLE").is_ok() {
        crate::tts_stream::apply_config(&c);
        let (phone_tx, _phone_rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(_phone_rx);
        crate::tts_stream::register_phone(device, phone_tx);
        crate::tts_stream::prewarm(device);
        println!("tts prewarmed");
    }
    if std::env::var("PREWARM").is_ok() {
        super::flux_stt::prewarm(device, p.clone());
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let mut tasks = Vec::new();
    for arg in args {
        let (path, gap) = arg.split_once(':').unwrap();
        let gap: f32 = gap.parse().unwrap();
        let pcm = std::fs::read(path).unwrap();
        let id = std::path::Path::new(path).file_stem().unwrap().to_string_lossy().to_string();
        let Some(mut t) = super::flux_stt::begin(device, p.clone()) else {
            println!("{id} begin returned None");
            continue;
        };
        let preroll = 320 * 32;
        t.push(&pcm[..preroll.min(pcm.len())]);
        let mut next = tokio::time::Instant::now();
        for frame in pcm[preroll.min(pcm.len())..].chunks(640) {
            next += std::time::Duration::from_millis(20);
            tokio::time::sleep_until(next).await;
            t.push(frame);
        }
        let at = std::time::Instant::now();
        let secs = pcm.len() as f32 / 32_000.0;
        tasks.push(tokio::spawn(async move {
            let r = t.finish(&id, at).await;
            let ms = at.elapsed().as_millis();
            match r {
                Ok((text, words)) => println!(
                    "RESULT {id} {secs:.1}s ok +{ms}ms words={} first={:?} last={:?} text={text:?}",
                    words.len(),
                    words.first().map(|w| (w.start, w.end)),
                    words.last().map(|w| (w.start, w.end))
                ),
                Err(e) => println!("RESULT {id} {secs:.1}s FALLBACK +{ms}ms {e}"),
            }
        }));
        tokio::time::sleep(std::time::Duration::from_secs_f32(gap)).await;
    }
    for t in tasks {
        let _ = t.await;
    }
}
