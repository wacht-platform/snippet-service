use std::io::Write;
use std::time::Instant;

use snippet::voiceprint::{self, VoiceprintAccumulator, fbank::Fbank, neural};

fn load(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    neural::to_f32(&voiceprint::pcm_from_audio(&bytes).unwrap())
}

fn legacy(clip: &[f32]) -> Option<Vec<f32>> {
    let bytes: Vec<u8> = clip.iter().flat_map(|s| (*s as i16).to_le_bytes()).collect();
    let mut acc = VoiceprintAccumulator::default();
    acc.feed_pcm(&bytes);
    acc.finish_vector()
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

fn cpu_secs() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
    t(u.ru_utime) + t(u.ru_stime)
}

/// VP_MANIFEST="<wav> <speaker>" lines, VP_OUT=jsonl of neural and legacy vectors per crop.
#[test]
#[ignore]
fn eval_embeddings() {
    let (Ok(manifest), Ok(out)) = (std::env::var("VP_MANIFEST"), std::env::var("VP_OUT")) else { return };
    let mut m = neural::Embedder::load().unwrap();
    let mut w = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    for line in std::fs::read_to_string(manifest).unwrap().lines() {
        let (path, spk) = line.split_once(' ').unwrap();
        let x = load(path);
        for (crop, secs) in [("enrol", 20.0f32), ("1", 1.0), ("2", 2.0), ("3", 3.0), ("6", 6.0)] {
            let (voiced, clip) = neural::voiced(&x, secs);
            let row = serde_json::json!({
                "path": path, "spk": spk, "crop": crop, "voiced": voiced, "clip_secs": clip.len() as f32 / 16000.0,
                "neural": m.embed(&clip).ok(), "legacy": legacy(&clip),
            });
            writeln!(w, "{row}").unwrap();
        }
    }
}

/// VP_FBANK_IN=<wav> VP_FBANK_OUT=<raw f32le, 80 per frame>
#[test]
#[ignore]
fn fbank_dump() {
    let (Ok(input), Ok(out)) = (std::env::var("VP_FBANK_IN"), std::env::var("VP_FBANK_OUT")) else { return };
    let x = load(&input);
    let mut feats = Vec::new();
    Fbank::new().compute(&x, &mut feats);
    std::fs::write(out, feats.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
}

/// VP_BENCH_WAV=<a long wav> prints cold load, warm latency percentiles, CPU per call and RSS growth.
#[test]
#[ignore]
fn bench() {
    let Ok(path) = std::env::var("VP_BENCH_WAV") else { return };
    let x = load(&path);
    let rss0 = rss_kb();
    let t = Instant::now();
    let mut m = neural::Embedder::load().unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let rss1 = rss_kb();
    let t = Instant::now();
    m.embed(&x[..48000]).unwrap();
    println!("cold: load {load_ms:.0}ms, first 3s call {:.1}ms, rss +{}MB after load", t.elapsed().as_secs_f64() * 1000.0, (rss1 - rss0) / 1024);
    for secs in [1usize, 3, 6] {
        let clip = &x[..secs * 16000];
        for _ in 0..5 {
            m.embed(clip).unwrap();
        }
        let mut ms = Vec::new();
        let cpu0 = cpu_secs();
        let n = 50;
        for _ in 0..n {
            let t = Instant::now();
            let (_, c) = neural::voiced(clip, 6.0);
            m.embed(&c).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let cpu = (cpu_secs() - cpu0) * 1000.0 / n as f64;
        ms.sort_by(|a, b| a.total_cmp(b));
        println!("{secs}s: p50 {:.1}ms p95 {:.1}ms cpu {cpu:.1}ms/call", ms[n / 2], ms[n * 95 / 100]);
    }
    let t = Instant::now();
    let mut feats = Vec::new();
    let mut fb = Fbank::new();
    for _ in 0..50 {
        fb.compute(&x[..48000], &mut feats);
    }
    println!("fbank 3s: {:.2}ms", t.elapsed().as_secs_f64() * 1000.0 / 50.0);
    println!("rss total +{}MB (load + {} warm calls)", (rss_kb() - rss0) / 1024, 165);
}
