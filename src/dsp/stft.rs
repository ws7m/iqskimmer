//! Streaming short-time Fourier transform producing fft-shifted power spectra.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub struct Stft {
    win: Vec<f32>,
    fft_len: usize,
    hop: usize,
    fft: Arc<dyn Fft<f32>>,
    pending: Vec<Complex32>,
    work: Vec<Complex32>,
    scratch: Vec<Complex32>,
    power: Vec<f32>,
}

impl Stft {
    /// `win_len` samples are windowed (Blackman-Harris) and zero-padded to `fft_len`.
    pub fn new(win_len: usize, fft_len: usize, hop: usize) -> Self {
        assert!(fft_len >= win_len && hop > 0);
        let fft = FftPlanner::new().plan_fft_forward(fft_len);
        let n = win_len as f32;
        let mut win: Vec<f32> = (0..win_len)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / n;
                0.35875 - 0.48829 * x.cos() + 0.14128 * (2.0 * x).cos() - 0.01168 * (3.0 * x).cos()
            })
            .collect();
        // Normalize so a full-scale tone reads 0 dB regardless of window length.
        let sum: f32 = win.iter().sum();
        win.iter_mut().for_each(|w| *w /= sum);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self {
            win,
            fft_len,
            hop,
            fft,
            pending: Vec::new(),
            work: vec![Complex32::default(); fft_len],
            scratch,
            power: vec![0.0; fft_len],
        }
    }

    pub fn fft_len(&self) -> usize {
        self.fft_len
    }

    /// Feed samples; `on_frame` receives each new power spectrum, index 0 = most negative frequency.
    pub fn push(&mut self, samples: &[Complex32], mut on_frame: impl FnMut(&[f32])) {
        self.pending.extend_from_slice(samples);
        let wl = self.win.len();
        let mut start = 0;
        while start + wl <= self.pending.len() {
            for (i, w) in self.win.iter().enumerate() {
                self.work[i] = self.pending[start + i] * *w;
            }
            self.work[wl..].fill(Complex32::default());
            self.fft.process_with_scratch(&mut self.work, &mut self.scratch);
            let half = self.fft_len / 2;
            for (i, p) in self.power.iter_mut().enumerate() {
                *p = self.work[(i + half) % self.fft_len].norm_sqr();
            }
            on_frame(&self.power);
            start += self.hop;
        }
        self.pending.drain(..start.min(self.pending.len()));
    }
}
