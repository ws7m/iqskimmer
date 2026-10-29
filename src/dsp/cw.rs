//! CW decoder operating on a single channel's power envelope.
//!
//! Pipeline: adaptive signal/noise level tracking -> hysteresis threshold -> debounce ->
//! mark/space durations -> speed estimate (2-cluster on recent marks) -> Morse lookup.

use super::morse;
use std::collections::VecDeque;

const SQUELCH_DB: f32 = 12.0;
/// A speed estimate pinned near the 60 WPM limit means we are keying on noise.
const MAX_TRUSTED_WPM: f32 = 55.0;
const MAX_TEXT: usize = 4000;

pub struct CwDecoder {
    /// Envelope sample rate (Hz).
    fs: f32,
    sig_p: f32,
    noise_p: f32,
    sig_db: f32,
    noise_db: f32,
    primed: bool,
    key: bool,
    run: usize,
    pending: usize,
    /// Current dit length estimate in envelope samples.
    dit: f32,
    speed_known: bool,
    marks: VecDeque<f32>,
    spaces: VecDeque<f32>,
    rhythm: (f32, f32),
    symbol: String,
    char_done: bool,
    word_done: bool,
    pub text: String,
    /// Monotonic count of characters ever emitted, for change detection.
    pub emitted: u64,
}

impl CwDecoder {
    pub fn new(fs: f32) -> Self {
        Self {
            fs,
            sig_p: 0.0,
            noise_p: 0.0,
            sig_db: -200.0,
            noise_db: 0.0,
            primed: false,
            key: false,
            run: 0,
            pending: 0,
            dit: 1.2 / 20.0 * fs,
            speed_known: false,
            marks: VecDeque::new(),
            spaces: VecDeque::new(),
            rhythm: (1.0, 1.0),
            symbol: String::new(),
            char_done: true,
            word_done: true,
            text: String::new(),
            emitted: 0,
        }
    }

    pub fn wpm(&self) -> f32 {
        1.2 * self.fs / self.dit
    }

    pub fn signal_db(&self) -> f32 {
        self.sig_db
    }

    pub fn noise_db(&self) -> f32 {
        self.noise_db
    }

    /// Share of the last 40 decoded characters made only of dits (E I S H 5), with how many
    /// characters were counted. About 25% in real text, 90%+ when keying on noise or
    /// digital-mode tone pips. (T is left out: contest exchanges are full of "TEST".)
    pub fn junk_share(&self) -> (f32, usize) {
        let recent: Vec<char> = self.text.chars().rev().filter(|c| !c.is_whitespace()).take(40).collect();
        let junk = recent.iter().filter(|c| matches!(c, 'E' | 'I' | 'S' | 'H' | '5')).count();
        (if recent.is_empty() { 1.0 } else { junk as f32 / recent.len() as f32 }, recent.len())
    }

    pub fn mark_count(&self) -> usize {
        self.marks.len()
    }

    /// Long/short mark-length ratio and the share of short marks over recent marks.
    pub fn rhythm(&self) -> (f32, f32) {
        self.rhythm
    }

    pub fn snr_db(&self) -> f32 {
        (self.sig_db - self.noise_db).max(0.0)
    }

    pub fn key_down(&self) -> bool {
        self.key
    }

    /// Feed one envelope power sample.
    pub fn feed(&mut self, p: f32) {
        let db = 10.0 * (p + 1e-30).log10();
        if !self.primed {
            self.noise_p = p;
            self.sig_p = p;
            self.primed = true;
        }
        // Two-level tracker: noise is the mean power of samples below the midpoint,
        // signal the mean power of samples above it. Robust to noise spikes, and it
        // bootstraps itself because a keyed signal pulls the two levels apart.
        let mid_p = (self.noise_p * self.sig_p).sqrt();
        if p < mid_p {
            let k = if p < self.noise_p { 4.0 } else { 2.0 } / self.fs;
            self.noise_p += k * (p - self.noise_p);
        } else {
            let k = if p > self.sig_p { 10.0 } else { 3.0 } / self.fs;
            self.sig_p += k * (p - self.sig_p);
        }
        self.noise_db = 10.0 * (self.noise_p + 1e-30).log10();
        self.sig_db = 10.0 * (self.sig_p + 1e-30).log10();

        let snr = self.sig_db - self.noise_db;
        let mid = self.noise_db + 0.5 * snr;
        let hyst = 0.12 * snr;
        let raw = if snr < SQUELCH_DB {
            false
        } else if self.key {
            db > mid - hyst
        } else {
            db > mid + hyst
        };

        self.run += 1;
        if raw != self.key {
            self.pending += 1;
            let debounce = (self.dit * 0.25).clamp(1.0, 4.0) as usize;
            if self.pending >= debounce {
                let len = (self.run - self.pending) as f32;
                let was_mark = self.key;
                self.key = raw;
                self.run = self.pending;
                self.pending = 0;
                self.segment(was_mark, len);
            }
        } else {
            self.pending = 0;
        }
        if !self.key {
            self.check_space(self.run as f32);
        }
    }

    fn segment(&mut self, was_mark: bool, len: f32) {
        if len <= 0.0 {
            return;
        }
        if was_mark {
            // Ignore absurdly long marks (carriers, tuning).
            if len > 12.0 * self.dit && len / self.fs > 0.5 {
                self.symbol.clear();
                return;
            }
            push_capped(&mut self.marks, len, 24);
            self.update_speed();
            self.symbol.push(if len < 2.0 * self.dit { '.' } else { '-' });
            if self.symbol.len() > 8 {
                self.symbol.clear();
            }
            self.char_done = false;
            self.word_done = false;
        } else if len < 2.0 * self.dit {
            push_capped(&mut self.spaces, len, 24);
        }
    }

    fn check_space(&mut self, run: f32) {
        if !self.char_done && run > 2.0 * self.dit {
            self.char_done = true;
            if let Some(s) = morse::decode(&self.symbol) {
                if self.speed_known && self.wpm() < MAX_TRUSTED_WPM {
                    self.text.push_str(s);
                    self.emitted += 1;
                }
            }
            self.symbol.clear();
        }
        if !self.word_done && run > 5.0 * self.dit {
            self.word_done = true;
            if !self.text.is_empty() && !self.text.ends_with(' ') {
                self.text.push(' ');
            }
        }
        if self.text.len() > MAX_TEXT {
            let cut = self.text.len() - MAX_TEXT / 2;
            let cut = (cut..self.text.len()).find(|&i| self.text.is_char_boundary(i)).unwrap_or(0);
            self.text.drain(..cut);
        }
    }

    fn update_speed(&mut self) {
        if self.marks.len() < 3 {
            return;
        }
        let mut m: Vec<f32> = self.marks.iter().copied().collect();
        m.sort_by(|a, b| a.total_cmp(b));
        // Split into short/long clusters at the largest ratio gap.
        let (mut best, mut split) = (1.0f32, 0usize);
        for k in 1..m.len() {
            let r = m[k] / m[k - 1].max(1.0);
            if r > best {
                best = r;
                split = k;
            }
        }
        self.rhythm = rhythm(&m);
        let est = if best >= 1.8 {
            let short: f32 = m[..split].iter().sum();
            let long: f32 = m[split..].iter().sum();
            (short + long / 3.0) / m.len() as f32
        } else {
            // All marks look alike: decide dits vs dahs from the intra-character spaces.
            let mean = m.iter().sum::<f32>() / m.len() as f32;
            let space = if self.spaces.is_empty() {
                mean
            } else {
                let mut s: Vec<f32> = self.spaces.iter().copied().collect();
                s.sort_by(|a, b| a.total_cmp(b));
                s[s.len() / 2]
            };
            if mean > 2.0 * space { mean / 3.0 } else { mean }
        };
        // 5..60 WPM
        let lo = 1.2 / 60.0 * self.fs;
        let hi = 1.2 / 5.0 * self.fs;
        let est = est.clamp(lo, hi);
        self.dit = if self.speed_known { 0.6 * self.dit + 0.4 * est } else { est };
        self.speed_known = self.marks.len() >= 4;
    }
}

/// Long/short cluster mean ratio and short share of sorted mark lengths, ignoring noise
/// glitches (marks under a third of the median), which would otherwise form their own cluster.
fn rhythm(sorted: &[f32]) -> (f32, f32) {
    let median = sorted[sorted.len() / 2];
    let m: Vec<f32> = sorted.iter().copied().filter(|&x| x >= 0.35 * median).collect();
    let (mut best, mut split) = (1.0f32, 0usize);
    for k in 1..m.len() {
        let r = m[k] / m[k - 1].max(1.0);
        if r > best {
            best = r;
            split = k;
        }
    }
    if split == 0 {
        return (1.0, 1.0);
    }
    let short = m[..split].iter().sum::<f32>() / split as f32;
    let long = m[split..].iter().sum::<f32>() / (m.len() - split) as f32;
    (long / short, split as f32 / m.len() as f32)
}

fn push_capped(q: &mut VecDeque<f32>, v: f32, cap: usize) {
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(v);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    /// Render text as an on/off power envelope at `fs` with noise.
    fn envelope(text: &str, wpm: f32, fs: f32, snr_db: f32, seed: u64) -> Vec<f32> {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let dit = (1.2 / wpm * fs).round() as usize;
        let mut keys = vec![false; 10 * dit];
        for word in text.split_whitespace() {
            for ch in word.chars() {
                for (i, e) in morse::encode(ch).unwrap().chars().enumerate() {
                    if i > 0 {
                        keys.extend(std::iter::repeat_n(false, dit));
                    }
                    keys.extend(std::iter::repeat_n(true, if e == '-' { 3 * dit } else { dit }));
                }
                keys.extend(std::iter::repeat_n(false, 3 * dit));
            }
            keys.extend(std::iter::repeat_n(false, 4 * dit));
        }
        keys.extend(std::iter::repeat_n(false, 20 * dit));
        let sig = 10f32.powf(snr_db / 10.0);
        let raw: Vec<f32> = keys
            .into_iter()
            .map(|k| {
                let n: f32 = -rng.gen_range(f32::EPSILON..1.0f32).ln(); // exponential noise power
                n + if k { sig } else { 0.0 }
            })
            .collect();
        // The channelizer window spans ~3 envelope samples.
        raw.windows(3).map(|w| w.iter().sum::<f32>() / 3.0).collect()
    }

    #[test]
    fn decodes_clean_cq() {
        for wpm in [15.0, 25.0, 38.0] {
            let mut d = CwDecoder::new(250.0);
            for p in envelope("CQ CQ DE W1AW W1AW K CQ DE W1AW", wpm, 250.0, 20.0, 1) {
                d.feed(p);
            }
            assert!(d.text.contains("W1AW"), "{wpm} wpm: {:?}", d.text);
            assert!((d.wpm() - wpm).abs() / wpm < 0.15, "{wpm} wpm estimated {}", d.wpm());
        }
    }
}
