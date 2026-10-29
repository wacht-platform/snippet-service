pub const BINS: usize = 80;
const WIN: usize = 400;
const HOP: usize = 160;
const NFFT: usize = 512;
const SPEC: usize = NFFT / 2 + 1;

struct Filter {
    start: usize,
    weights: Vec<f32>,
}

/// Kaldi-compatible 80-bin log-mel filterbank (torchaudio.compliance.kaldi.fbank defaults, dither 0, povey window).
pub struct Fbank {
    window: [f32; WIN],
    filters: Vec<Filter>,
    tw_re: [f32; NFFT / 2],
    tw_im: [f32; NFFT / 2],
    rev: [u16; NFFT],
    frame: [f32; WIN],
    re: [f32; NFFT],
    im: [f32; NFFT],
    power: [f32; SPEC],
}

impl Default for Fbank {
    fn default() -> Self {
        Self::new()
    }
}

fn mel(f: f64) -> f64 {
    1127.0 * (1.0 + f / 700.0).ln()
}

impl Fbank {
    pub fn new() -> Self {
        let mut window = [0f32; WIN];
        for (i, w) in window.iter_mut().enumerate() {
            let a = 2.0 * std::f64::consts::PI * i as f64 / (WIN - 1) as f64;
            *w = (0.5 - 0.5 * a.cos()).powf(0.85) as f32;
        }
        let (lo, hi) = (mel(20.0), mel(8000.0));
        let delta = (hi - lo) / (BINS + 1) as f64;
        let width = 16000.0 / NFFT as f64;
        let filters = (0..BINS)
            .map(|b| {
                let (l, c, r) = (lo + b as f64 * delta, lo + (b + 1) as f64 * delta, lo + (b + 2) as f64 * delta);
                let w: Vec<f32> = (0..NFFT / 2)
                    .map(|k| {
                        let m = mel(width * k as f64);
                        ((m - l) / (c - l)).min((r - m) / (r - c)).max(0.0) as f32
                    })
                    .collect();
                let start = w.iter().position(|x| *x > 0.0).unwrap_or(0);
                let end = w.iter().rposition(|x| *x > 0.0).map_or(start, |e| e + 1);
                Filter { start, weights: w[start..end].to_vec() }
            })
            .collect();
        let mut tw_re = [0f32; NFFT / 2];
        let mut tw_im = [0f32; NFFT / 2];
        for k in 0..NFFT / 2 {
            let a = -2.0 * std::f64::consts::PI * k as f64 / NFFT as f64;
            tw_re[k] = a.cos() as f32;
            tw_im[k] = a.sin() as f32;
        }
        let bits = NFFT.trailing_zeros();
        let mut rev = [0u16; NFFT];
        for (i, r) in rev.iter_mut().enumerate() {
            *r = ((i as u32).reverse_bits() >> (32 - bits)) as u16;
        }
        Self { window, filters, tw_re, tw_im, rev, frame: [0.0; WIN], re: [0.0; NFFT], im: [0.0; NFFT], power: [0.0; SPEC] }
    }

    pub fn frames(samples: usize) -> usize {
        if samples < WIN { 0 } else { 1 + (samples - WIN) / HOP }
    }

    /// `samples` at int16 scale. Appends `frames * 80` mean-normalised features to `out` and returns the frame count.
    pub fn compute(&mut self, samples: &[f32], out: &mut Vec<f32>) -> usize {
        let frames = Self::frames(samples.len());
        out.clear();
        out.reserve(frames * BINS);
        for f in 0..frames {
            self.frame.copy_from_slice(&samples[f * HOP..f * HOP + WIN]);
            let mean = self.frame.iter().sum::<f32>() / WIN as f32;
            for x in self.frame.iter_mut() {
                *x -= mean;
            }
            for i in (1..WIN).rev() {
                self.frame[i] -= 0.97 * self.frame[i - 1];
            }
            self.frame[0] -= 0.97 * self.frame[0];
            for i in 0..NFFT {
                let j = self.rev[i] as usize;
                self.re[j] = if i < WIN { self.frame[i] * self.window[i] } else { 0.0 };
                self.im[j] = 0.0;
            }
            self.fft();
            for k in 0..SPEC {
                self.power[k] = self.re[k] * self.re[k] + self.im[k] * self.im[k];
            }
            for filt in &self.filters {
                let e: f32 = filt.weights.iter().zip(&self.power[filt.start..]).map(|(w, p)| w * p).sum();
                out.push(e.max(f32::EPSILON).ln());
            }
        }
        if frames > 0 {
            let mut mean = [0f32; BINS];
            for row in out.chunks_exact(BINS) {
                for (m, v) in mean.iter_mut().zip(row) {
                    *m += v;
                }
            }
            for m in mean.iter_mut() {
                *m /= frames as f32;
            }
            for row in out.chunks_exact_mut(BINS) {
                for (v, m) in row.iter_mut().zip(&mean) {
                    *v -= m;
                }
            }
        }
        frames
    }

    fn fft(&mut self) {
        let mut len = 2;
        while len <= NFFT {
            let half = len / 2;
            let step = NFFT / len;
            for start in (0..NFFT).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = (self.tw_re[k * step], self.tw_im[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = self.re[b] * wr - self.im[b] * wi;
                    let ti = self.re[b] * wi + self.im[b] * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            len *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_matches_dft_power() {
        let mut fb = Fbank::new();
        let x: Vec<f32> = (0..NFFT).map(|i| ((i * 7919) % 97) as f32 - 48.0).collect();
        for i in 0..NFFT {
            fb.re[fb.rev[i] as usize] = x[i];
            fb.im[fb.rev[i] as usize] = 0.0;
        }
        fb.fft();
        for k in [0usize, 1, 37, 128, 256] {
            let (mut r, mut im) = (0f64, 0f64);
            for (n, v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * n) as f64 / NFFT as f64;
                r += *v as f64 * a.cos();
                im += *v as f64 * a.sin();
            }
            assert!((fb.re[k] as f64 - r).abs() < 0.05 && (fb.im[k] as f64 - im).abs() < 0.05, "bin {k}");
        }
    }

    #[test]
    fn frame_count_snips_edges() {
        assert_eq!(Fbank::frames(399), 0);
        assert_eq!(Fbank::frames(400), 1);
        assert_eq!(Fbank::frames(16000), 98);
        let mut out = Vec::new();
        let x: Vec<f32> = (0..16000).map(|i| ((i as f32) * 0.05).sin() * 3000.0).collect();
        assert_eq!(Fbank::new().compute(&x, &mut out), 98);
        assert_eq!(out.len(), 98 * BINS);
    }
}
