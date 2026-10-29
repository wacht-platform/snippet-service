use std::io::Write;
use std::path::PathBuf;

use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::TensorRef;
use sha2::{Digest, Sha256};

use super::fbank::{BINS, Fbank};

pub const MODEL: &str = "wespeaker-resnet34-lm-v1";
pub const DIM: usize = 256;
const FILE: &str = "wespeaker-resnet34-lm-v1.onnx";
const URL: &str = "https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM/resolve/main/voxceleb_resnet34_LM.onnx";
const SHA256: &str = "7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068";

const VAD_FRAME: usize = 320;
const HANG: usize = 2;

pub fn model_path() -> PathBuf {
    crate::config::snippet_home().join("models").join(FILE)
}

fn verify(path: &std::path::Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| e.to_string())?;
    let got = format!("{:x}", hasher.finalize());
    if got == SHA256 { Ok(()) } else { Err(format!("checksum mismatch for {}: {got}", path.display())) }
}

fn download(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("onnx.part");
    let mut resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?
        .get(URL)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download {URL}: {e}"))?;
    let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    resp.copy_to(&mut out).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    if let Err(e) = verify(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn load() -> Result<Session, String> {
    let path = model_path();
    if path.exists() {
        if let Err(e) = verify(&path) {
            eprintln!("[voiceprint] {e}; re-downloading");
            let _ = std::fs::remove_file(&path);
        }
    }
    if !path.exists() {
        eprintln!("[voiceprint] downloading {MODEL}");
        download(&path)?;
    }
    open(&path)
}

fn open(path: &std::path::Path) -> Result<Session, String> {
    Session::builder()
        .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
        .and_then(|b| b.with_intra_threads(2))
        .and_then(|b| b.with_inter_threads(1))
        .and_then(|b| b.with_parallel_execution(false))
        .and_then(|b| b.commit_from_file(path))
        .map_err(|e| e.to_string())
}

pub fn to_f32(pcm: &[u8]) -> Vec<f32> {
    pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32).collect()
}

/// Energy VAD over 20 ms frames with an adaptive noise floor. Returns voiced seconds and keeps only voiced audio
/// (with a short hangover), trimmed to the last `max_secs`.
pub fn voiced(samples: &[f32], max_secs: f32) -> (f32, Vec<f32>) {
    let frames = samples.len() / VAD_FRAME;
    if frames == 0 {
        return (0.0, Vec::new());
    }
    let rms: Vec<f32> = samples
        .chunks_exact(VAD_FRAME)
        .map(|f| {
            let m = f.iter().sum::<f32>() / VAD_FRAME as f32;
            (f.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / VAD_FRAME as f32).sqrt()
        })
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[frames / 10];
    let peak = sorted[frames - 1 - frames / 20];
    let gate = (floor * 3.0).max(peak * 0.1).max(150.0);
    let speech: Vec<bool> = rms.iter().map(|r| *r > gate).collect();
    let count = speech.iter().filter(|v| **v).count();
    let keep: Vec<bool> = (0..frames).map(|i| speech[i.saturating_sub(HANG)..(i + HANG + 1).min(frames)].iter().any(|v| *v)).collect();
    let cap = (max_secs * 16000.0) as usize / VAD_FRAME;
    let mut out = Vec::with_capacity(cap.min(frames) * VAD_FRAME);
    let mut picked: Vec<usize> = (0..frames).rev().filter(|i| keep[*i]).take(cap).collect();
    picked.reverse();
    for i in picked {
        out.extend_from_slice(&samples[i * VAD_FRAME..(i + 1) * VAD_FRAME]);
    }
    (count as f32 * VAD_FRAME as f32 / 16000.0, out)
}

pub struct Embedder {
    session: Session,
    fbank: Fbank,
    feats: Vec<f32>,
}

impl Embedder {
    pub fn new(session: Session) -> Self {
        Self { session, fbank: Fbank::new(), feats: Vec::with_capacity(600 * BINS) }
    }

    pub fn load() -> Result<Self, String> {
        load().map(Self::new)
    }

    /// L2-normalised embedding of int16-scale samples (already voiced/trimmed).
    pub fn embed(&mut self, samples: &[f32]) -> Result<Vec<f32>, String> {
        let frames = self.fbank.compute(samples, &mut self.feats);
        if frames < 10 {
            return Err("clip too short".into());
        }
        let input = TensorRef::from_array_view(([1usize, frames, BINS], self.feats.as_slice())).map_err(|e| e.to_string())?;
        let outputs = self.session.run(ort::inputs!["feats" => input]).map_err(|e| e.to_string())?;
        let (_, emb) = outputs["embs"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        if emb.len() != DIM {
            return Err(format!("unexpected embedding size {}", emb.len()));
        }
        let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
        Ok(emb.iter().map(|x| x / norm).collect())
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    #[test]
    fn corrupt_model_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.onnx");
        std::fs::write(&path, b"not an onnx model").unwrap();
        assert!(super::verify(&path).is_err());
        assert!(super::open(&path).is_err());
        assert!(super::open(&dir.path().join("missing.onnx")).is_err());
    }
}
