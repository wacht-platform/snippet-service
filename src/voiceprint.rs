use serde::{Deserialize, Serialize};

pub const DIM: usize = 24;
const FRAME_SAMPLES: usize = 512; // 32 ms at 16 kHz
const SAMPLE_RATE: f32 = 16_000.0;
const NUM_BANDS: usize = 16;
const NUM_CEPSTRA: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceprintSample {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub vector: Vec<f32>,
}

/// Streaming parallel acoustic voiceprint accumulator.
/// Updates frame-by-frame (~15 µs per 32 ms frame) as 16 kHz s16le PCM arrives,
/// so the speaker vector and cosine similarity score are ready in memory with
/// zero extra latency when transcription partials or finals arrive.
#[derive(Clone, Default)]
pub struct VoiceprintAccumulator {
    tail: Vec<i16>,
    voiced_frames: usize,
    cep_sum: [f64; NUM_CEPSTRA],
    cep_sq_sum: [f64; 8],
    f0_sum: f64,
    f0_sq_sum: f64,
    f0_count: usize,
    harm_sum: f64,
    centroid_sum: f64,
}

impl VoiceprintAccumulator {
    pub fn reset(&mut self) {
        self.tail.clear();
        self.voiced_frames = 0;
        self.cep_sum = [0.0; NUM_CEPSTRA];
        self.cep_sq_sum = [0.0; 8];
        self.f0_sum = 0.0;
        self.f0_sq_sum = 0.0;
        self.f0_count = 0;
        self.harm_sum = 0.0;
        self.centroid_sum = 0.0;
    }

    pub fn voiced_frames(&self) -> usize {
        self.voiced_frames
    }

    /// Feeds a chunk of 16 kHz s16le PCM bytes in parallel with STT streaming.
    pub fn feed_pcm(&mut self, pcm: &[u8]) {
        for chunk in pcm.chunks_exact(2) {
            self.tail.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            if self.tail.len() == FRAME_SAMPLES {
                let frame = self.tail.clone();
                // 50% overlap (256 samples = 16 ms hop) for smooth tracking
                self.tail.drain(..FRAME_SAMPLES / 2);
                self.process_frame(&frame);
            }
        }
    }

    fn process_frame(&mut self, samples: &[i16]) {
        let mut x = [0.0f32; FRAME_SAMPLES];
        let mut energy = 0.0f32;
        for (i, &s) in samples.iter().enumerate() {
            let v = s as f32 / 32768.0;
            x[i] = v;
            energy += v * v;
        }
        let rms = (energy / FRAME_SAMPLES as f32).sqrt();
        // Skip silence / very low ambient floor frames
        if rms < 0.012 {
            return;
        }

        // 1. Pre-emphasis + Hamming window for vocal-tract spectral envelope
        let mut w = [0.0f32; FRAME_SAMPLES];
        let mut prev = x[0];
        for i in 0..FRAME_SAMPLES {
            let pe = x[i] - 0.97 * prev;
            prev = x[i];
            let hamming = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME_SAMPLES - 1) as f32).cos();
            w[i] = pe * hamming;
        }

        // 2. 16 Mel-spaced resonator bands (130 Hz .. 3800 Hz) via Goertzel
        const FREQS: [f32; NUM_BANDS] = [
            135.0, 200.0, 275.0, 360.0, 460.0, 575.0, 710.0, 865.0,
            1045.0, 1255.0, 1500.0, 1785.0, 2120.0, 2510.0, 2965.0, 3500.0,
        ];
        let mut log_band = [0.0f64; NUM_BANDS];
        let mut weighted_freq = 0.0f64;
        let mut total_mag = 1e-9f64;

        for (b, &freq) in FREQS.iter().enumerate() {
            let k = (freq / SAMPLE_RATE) * FRAME_SAMPLES as f32;
            let omega = 2.0 * std::f32::consts::PI * k / FRAME_SAMPLES as f32;
            let coeff = 2.0 * omega.cos();
            let mut s1 = 0.0f32;
            let mut s2 = 0.0f32;
            for &sample in &w {
                let s0 = sample + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(1e-10) / (FRAME_SAMPLES as f32);
            let mag = (power as f64).sqrt();
            log_band[b] = (power as f64 + 1e-9).ln();
            weighted_freq += (freq as f64 / 4000.0) * mag;
            total_mag += mag;
        }

        // 3. Mean-normalize log-band energies within the frame (channel/gain invariance)
        let mean_log: f64 = log_band.iter().sum::<f64>() / NUM_BANDS as f64;
        for b in &mut log_band {
            *b -= mean_log;
        }

        // 4. DCT-II to extract 12 vocal-tract cepstral coefficients (c_1 .. c_12)
        for m in 0..NUM_CEPSTRA {
            let order = (m + 1) as f64;
            let mut sum = 0.0f64;
            for (b, &val) in log_band.iter().enumerate() {
                let angle = std::f64::consts::PI * order * (b as f64 + 0.5) / NUM_BANDS as f64;
                sum += val * angle.cos();
            }
            let c = sum * (2.0 / NUM_BANDS as f64).sqrt();
            self.cep_sum[m] += c;
            if m < 8 {
                self.cep_sq_sum[m] += c * c;
            }
        }

        // 5. Pitch (F0: 75 Hz .. 350 Hz -> lag 45 .. 213 samples) via normalized autocorrelation
        let mut best_corr = 0.0f32;
        let mut best_lag = 0usize;
        for lag in (45..=210).step_by(2) {
            let mut num = 0.0f32;
            let mut den1 = 1e-9f32;
            let mut den2 = 1e-9f32;
            let len = FRAME_SAMPLES - lag;
            for i in 0..len {
                let a = x[i];
                let b = x[i + lag];
                num += a * b;
                den1 += a * a;
                den2 += b * b;
            }
            let r = num / (den1 * den2).sqrt();
            if r > best_corr {
                best_corr = r;
                best_lag = lag;
            }
        }
        if best_corr >= 0.42 && best_lag > 0 {
            let f0 = SAMPLE_RATE / best_lag as f32;
            // Normalize log-F0 around 150 Hz in [-1, 1]
            let log_f0 = ((f0 / 150.0).ln()) as f64;
            self.f0_sum += log_f0;
            self.f0_sq_sum += log_f0 * log_f0;
            self.f0_count += 1;
        }

        self.harm_sum += best_corr.clamp(0.0, 1.0) as f64;
        self.centroid_sum += weighted_freq / total_mag;
        self.voiced_frames += 1;
    }

    /// Returns the 24-D L2-normalized speaker embedding vector if enough voiced speech was heard.
    pub fn finish_vector(&self) -> Option<Vec<f32>> {
        if self.voiced_frames < 4 {
            return None;
        }
        let n = self.voiced_frames as f64;
        let mut vec = Vec::with_capacity(DIM);

        // [0..12]: Mean cepstral coefficients (vocal tract shape)
        for m in 0..NUM_CEPSTRA {
            vec.push((self.cep_sum[m] / n) as f32);
        }
        // [12..20]: Centered cepstral standard deviations (zero-mean so different speakers don't share positive bias)
        const TYPICAL_STD: [f64; 8] = [0.65, 0.55, 0.48, 0.42, 0.38, 0.35, 0.32, 0.30];
        for m in 0..8 {
            let mean = self.cep_sum[m] / n;
            let var = (self.cep_sq_sum[m] / n - mean * mean).max(0.0);
            vec.push(((var.sqrt() - TYPICAL_STD[m]) * 0.5) as f32);
        }
        // [20..24]: Pitch (F0 mean heavily weighted, centered F0 std), harmonicity, and spectral centroid
        let (f0_mean, f0_std) = if self.f0_count > 0 {
            let fn_cnt = self.f0_count as f64;
            let m = self.f0_sum / fn_cnt;
            let v = (self.f0_sq_sum / fn_cnt - m * m).max(0.0);
            (m * 2.6, (v.sqrt() - 0.12) * 0.8)
        } else {
            (0.0, 0.0)
        };
        vec.push(f0_mean as f32);
        vec.push(f0_std as f32);
        vec.push(((self.harm_sum / n) - 0.62) as f32);
        vec.push(((self.centroid_sum / n) - 0.38) as f32);

        normalize(&mut vec);
        Some(vec)
    }

    /// Computes how likely the accumulated voice belongs to the enrolled user (`0.00..=1.00`).
    pub fn score(&self, samples: &[VoiceprintSample]) -> Option<f64> {
        if samples.is_empty() {
            return None;
        }
        let live = self.finish_vector()?;
        score_vector(&live, samples)
    }
}

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x = (*x as f64 / norm) as f32;
        }
    }
}

pub fn score_vector(live: &[f32], samples: &[VoiceprintSample]) -> Option<f64> {
    if samples.is_empty() || live.len() != DIM {
        return None;
    }
    let mut best_score = -1.0f64;
    for s in samples {
        if s.vector.len() != DIM {
            continue;
        }
        let dot: f64 = live
            .iter()
            .zip(&s.vector)
            .map(|(a, b)| (*a as f64) * (*b as f64))
            .sum();
        // Penalize pitch (F0 index 20) mismatch directly so a speaker in a different pitch register scores low
        let pitch_diff = (live[20] as f64 - s.vector[20] as f64).abs();
        let pitch_penalty = (pitch_diff * 0.35).min(0.35);
        let adj = dot - pitch_penalty;
        if adj > best_score {
            best_score = adj;
        }
    }
    if best_score <= -0.99 {
        return None;
    }
    // Strict discriminative mapping: adj <= 0.42 -> 0.0, adj >= 0.88 -> 1.0
    let mapped = ((best_score - 0.42) / 0.46).clamp(0.0, 1.0);
    Some((mapped * 100.0).round() / 100.0)
}

pub fn score_pcm(pcm: &[u8], samples: &[VoiceprintSample]) -> Option<f64> {
    if samples.is_empty() || pcm.len() < FRAME_SAMPLES * 4 {
        return None;
    }
    let mut acc = VoiceprintAccumulator::default();
    acc.feed_pcm(pcm);
    acc.score(samples)
}

/// Extracts a 24-D voiceprint vector from raw 16 kHz s16le PCM or a RIFF WAV file.
pub fn extract_from_pcm_or_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let pcm = if bytes.len() > 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        extract_wav_s16le_16k(bytes)?
    } else {
        bytes.to_vec()
    };
    let mut acc = VoiceprintAccumulator::default();
    acc.feed_pcm(&pcm);
    acc.finish_vector().ok_or_else(|| {
        "Not enough clear speech detected — speak naturally for 2–3 seconds and try again".to_string()
    })
}

fn extract_wav_s16le_16k(wav: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 12usize;
    let mut channels = 1u16;
    let mut sample_rate = 16_000u32;
    let mut bits_per_sample = 16u16;
    let mut data_slice: Option<&[u8]> = None;

    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        pos += 8;
        let end = (pos + size).min(wav.len());
        if id == b"fmt " && end - pos >= 16 {
            channels = u16::from_le_bytes([wav[pos + 2], wav[pos + 3]]).max(1);
            sample_rate = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]).max(8000);
            bits_per_sample = u16::from_le_bytes([wav[pos + 14], wav[pos + 15]]);
        } else if id == b"data" {
            data_slice = Some(&wav[pos..end]);
            break;
        }
        pos = end + (size & 1);
    }

    let raw = data_slice.ok_or_else(|| "WAV file had no data chunk".to_string())?;
    if bits_per_sample != 16 {
        return Err(format!("Unsupported WAV bit depth: {bits_per_sample}"));
    }
    let ch = channels as usize;
    let frame_bytes = ch * 2;
    let total_frames = raw.len() / frame_bytes;
    let mut mono = Vec::with_capacity(total_frames);
    for f in 0..total_frames {
        let base = f * frame_bytes;
        let s = i16::from_le_bytes([raw[base], raw[base + 1]]);
        mono.push(s);
    }
    if sample_rate == 16_000 {
        let mut out = Vec::with_capacity(mono.len() * 2);
        for s in mono {
            out.extend_from_slice(&s.to_le_bytes());
        }
        return Ok(out);
    }
    // Resample to 16 kHz
    let step = sample_rate as f64 / 16_000.0;
    let out_len = (mono.len() as f64 / step) as usize;
    let mut out = Vec::with_capacity(out_len * 2);
    for i in 0..out_len {
        let src = i as f64 * step;
        let idx = src as usize;
        let frac = (src - idx as f64) as f32;
        let s0 = mono[idx.min(mono.len().saturating_sub(1))] as f32;
        let s1 = mono[(idx + 1).min(mono.len().saturating_sub(1))] as f32;
        let v = (s0 + (s1 - s0) * frac).clamp(-32768.0, 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}
