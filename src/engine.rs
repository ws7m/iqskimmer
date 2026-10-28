//! DSP engine: runs on its own thread, consuming I/Q from a source and producing
//! waterfall rows plus a set of per-signal CW decoders (run in parallel with rayon).
//!
//! Two STFTs run over the same I/Q:
//!  * waterfall: fine frequency resolution (~12 Hz bins, ~64 rows/s) for display and signal detection;
//!  * channelizer: ~80 Hz noise bandwidth at a 250 Hz frame rate, whose bins are the
//!    envelopes each CW decoder keys from. One FFT serves every decoder.

use crate::dsp::callsign;
use crate::dsp::cw::CwDecoder;
use crate::dsp::stft::Stft;
use crate::source::{self, SourceConfig, SourceEvent, SourceHandle};
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use parking_lot::Mutex;
use rayon::prelude::*;
use rustfft::num_complex::Complex32;
use std::sync::Arc;
use std::time::Instant;

/// Waterfall rows are quantized dB above the local noise floor.
pub const WF_DB_MIN: f32 = -10.0;
pub const WF_DB_STEP: f32 = 0.25;
const MAX_PENDING_ROWS: usize = 2048;

const ENVELOPE_RATE: u32 = 250;
const NOISE_BLOCK: usize = 64;
const MERGE_HZ: f64 = 45.0;
const CAND_HITS: u32 = 4;
const CHANNEL_TIMEOUT_S: f64 = 25.0;
const VIEW_TEXT: usize = 400;

fn tail(s: &str, n: usize) -> &str {
    let mut i = s.len().saturating_sub(n);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

pub fn wf_db(q: u8) -> f32 {
    WF_DB_MIN + q as f32 * WF_DB_STEP
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Detection threshold, dB above the noise floor.
    pub threshold_db: f32,
    pub swap_iq: bool,
    pub max_decoders: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self { threshold_db: 12.0, swap_iq: false, max_decoders: 500 }
    }
}

#[derive(Clone, Debug)]
pub struct DecoderView {
    pub id: u64,
    pub offset_hz: f64,
    /// Callsign (with "CQ " if calling), or empty if none decoded yet.
    pub label: String,
    pub cq: bool,
    pub text: String,
    pub wpm: f32,
    pub snr_db: f32,
    pub key_down: bool,
    /// Seconds since this decoder last produced a character.
    pub idle_s: f32,
}

#[derive(Default)]
pub struct Shared {
    pub rate: u32,
    pub center_hz: f64,
    pub wf_bins: usize,
    pub wf_bin_hz: f64,
    pub wf_rows_per_s: f32,
    /// New waterfall rows since the GUI last drained them.
    pub new_rows: Vec<Vec<u8>>,
    pub decoders: Vec<DecoderView>,
    pub status: String,
    pub running: bool,
    /// DSP time / stream time.
    pub load: f32,
    pub stream_time_s: f64,
}

enum Cmd {
    Settings(Settings),
}

pub struct Engine {
    pub shared: Arc<Mutex<Shared>>,
    cmd_tx: Sender<Cmd>,
    src_tx: Sender<SourceEvent>,
    source: Option<SourceHandle>,
}

impl Engine {
    pub fn new(settings: Settings) -> Self {
        let shared = Arc::new(Mutex::new(Shared { status: "Idle".into(), ..Default::default() }));
        let (src_tx, src_rx) = bounded::<SourceEvent>(512);
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("dsp".into())
            .spawn(move || run(src_rx, cmd_rx, sh, settings))
            .expect("spawn dsp thread");
        Self { shared, cmd_tx, src_tx, source: None }
    }

    pub fn start(&mut self, cfg: SourceConfig) -> Result<(), String> {
        self.stop();
        let _ = self.src_tx.send(SourceEvent::Reset);
        let h = source::start(cfg, self.src_tx.clone())?;
        self.source = Some(h);
        self.shared.lock().running = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(h) = self.source.take() {
            h.stop();
        }
        let mut s = self.shared.lock();
        s.running = false;
        s.status = "Stopped".into();
    }

    pub fn is_running(&self) -> bool {
        self.source.is_some()
    }

    pub fn set_settings(&self, s: Settings) {
        let _ = self.cmd_tx.send(Cmd::Settings(s));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Channel {
    id: u64,
    offset: f64,
    bin: usize,
    dec: CwDecoder,
    last_seen: f64,
    last_char_t: f64,
    emitted_seen: u64,
    label_at: u64,
    peak_snr: f32,
    label: String,
    cq: bool,
}

struct Candidate {
    offset: f64,
    hits: u32,
    last_t: f64,
    snr: f32,
}

struct Dsp {
    shared: Arc<Mutex<Shared>>,
    settings: Settings,
    rate: u32,
    center: f64,
    wf: Option<Stft>,
    ch: Option<Stft>,
    wf_bin_hz: f64,
    ch_bin_hz: f64,
    wf_hop: usize,
    ch_hop: usize,
    noise_blocks: Vec<f32>,
    db_row: Vec<f32>,
    nf_row: Vec<f32>,
    samples: u64,
    channels: Vec<Channel>,
    cands: Vec<Candidate>,
    next_id: u64,
    frames: Vec<f32>,
    rows: Vec<Vec<u8>>,
    last_publish: f64,
    busy_s: f64,
    busy_window_start: f64,
}

fn run(src_rx: Receiver<SourceEvent>, cmd_rx: Receiver<Cmd>, shared: Arc<Mutex<Shared>>, settings: Settings) {
    let mut d = Dsp {
        shared,
        settings,
        rate: 0,
        center: 0.0,
        wf: None,
        ch: None,
        wf_bin_hz: 0.0,
        ch_bin_hz: 0.0,
        wf_hop: 0,
        ch_hop: 0,
        noise_blocks: Vec::new(),
        db_row: Vec::new(),
        nf_row: Vec::new(),
        samples: 0,
        channels: Vec::new(),
        cands: Vec::new(),
        next_id: 1,
        frames: Vec::new(),
        rows: Vec::new(),
        last_publish: 0.0,
        busy_s: 0.0,
        busy_window_start: 0.0,
    };
    loop {
        select! {
            recv(src_rx) -> ev => match ev {
                Ok(ev) => d.on_event(ev),
                Err(_) => return,
            },
            recv(cmd_rx) -> c => match c {
                Ok(Cmd::Settings(s)) => d.settings = s,
                Err(_) => return,
            },
        }
    }
}

impl Dsp {
    fn on_event(&mut self, ev: SourceEvent) {
        match ev {
            SourceEvent::Reset => {
                self.rate = 0;
                self.channels.clear();
                self.cands.clear();
                let mut s = self.shared.lock();
                s.decoders.clear();
                s.new_rows.clear();
            }
            SourceEvent::Center(c) => {
                if (c - self.center).abs() > 0.5 {
                    self.center = c;
                    self.channels.clear();
                    self.cands.clear();
                    self.shared.lock().center_hz = c;
                }
            }
            SourceEvent::Status(s) => self.shared.lock().status = s,
            SourceEvent::Samples { rate, mut data } => {
                if rate != self.rate {
                    self.configure(rate);
                }
                if self.settings.swap_iq {
                    data.iter_mut().for_each(|s| *s = Complex32::new(s.im, s.re));
                }
                let t0 = Instant::now();
                self.process(&data);
                self.busy_s += t0.elapsed().as_secs_f64();
            }
        }
    }

    fn configure(&mut self, rate: u32) {
        self.rate = rate;
        let r = rate as f64;
        let wf_win = rate as usize / 16;
        let wf_fft = wf_win.next_power_of_two();
        self.wf_hop = rate as usize / 64;
        self.wf_bin_hz = r / wf_fft as f64;
        let ch_win = rate as usize / 40;
        let ch_fft = (2 * ch_win).next_power_of_two();
        self.ch_hop = (rate / ENVELOPE_RATE) as usize;
        self.ch_bin_hz = r / ch_fft as f64;
        self.wf = Some(Stft::new(wf_win, wf_fft, self.wf_hop));
        self.ch = Some(Stft::new(ch_win, ch_fft, self.ch_hop));
        self.noise_blocks.clear();
        self.channels.clear();
        self.cands.clear();
        self.samples = 0;
        self.busy_s = 0.0;
        self.busy_window_start = 0.0;
        self.last_publish = 0.0;
        let mut s = self.shared.lock();
        s.rate = rate;
        s.wf_bins = wf_fft;
        s.wf_bin_hz = self.wf_bin_hz;
        s.wf_rows_per_s = rate as f32 / self.wf_hop as f32;
        s.new_rows.clear();
    }

    fn now(&self) -> f64 {
        self.samples as f64 / self.rate as f64
    }

    fn process(&mut self, data: &[Complex32]) {
        // Channelizer frames for this block, flattened.
        self.frames.clear();
        let mut ch = self.ch.take().expect("configured");
        let stride = ch.fft_len();
        let frames = &mut self.frames;
        ch.push(data, |p| frames.extend_from_slice(p));
        self.ch = Some(ch);

        // Waterfall rows + detection.
        let mut wf = self.wf.take().expect("configured");
        let block_start = self.samples;
        let mut row_idx = 0u64;
        let mut pending_rows: Vec<Vec<f32>> = Vec::new();
        wf.push(data, |p| pending_rows.push(p.to_vec()));
        self.wf = Some(wf);
        for p in pending_rows {
            self.samples = block_start + row_idx * self.wf_hop as u64;
            row_idx += 1;
            self.on_wf_row(&p);
        }
        self.samples = block_start + data.len() as u64;
        let now = self.now();

        // Run every decoder over the new envelope frames, in parallel.
        let frames = &self.frames;
        let env_dt = self.ch_hop as f64 / self.rate as f64;
        let n_frames = frames.len() / stride.max(1);
        let block_t0 = now - n_frames as f64 * env_dt;
        self.channels.par_iter_mut().for_each(|c| {
            for (i, f) in frames.chunks_exact(stride).enumerate() {
                c.dec.feed(f[c.bin]);
                if c.dec.emitted != c.emitted_seen {
                    c.emitted_seen = c.dec.emitted;
                    c.last_char_t = block_t0 + i as f64 * env_dt;
                }
            }
        });

        // Keep decoders that are still keying or recently detected.
        self.channels.retain(|c| now - c.last_seen.max(c.last_char_t) < CHANNEL_TIMEOUT_S);

        if now - self.last_publish >= 0.1 {
            self.publish(now);
        }
    }

    fn on_wf_row(&mut self, p: &[f32]) {
        let n = p.len();
        self.db_row.clear();
        self.db_row.extend(p.iter().map(|v| 10.0 * (v + 1e-30).log10()));

        // Noise floor: 30th percentile of each block of bins, smoothed over time, interpolated.
        let nb = n.div_ceil(NOISE_BLOCK);
        let fresh = self.noise_blocks.len() != nb;
        if fresh {
            self.noise_blocks = vec![0.0; nb];
        }
        let mut tmp = Vec::with_capacity(NOISE_BLOCK);
        for (j, blk) in self.db_row.chunks(NOISE_BLOCK).enumerate() {
            tmp.clear();
            tmp.extend_from_slice(blk);
            let k = tmp.len() * 3 / 10;
            let (_, v, _) = tmp.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
            // 30th percentile of exponentially distributed power sits 4.5 dB below the mean.
            let v = *v + 4.5;
            let nbk = &mut self.noise_blocks[j];
            *nbk = if fresh { v } else { *nbk + 0.05 * (v - *nbk) };
        }
        self.nf_row.resize(n, 0.0);
        for i in 0..n {
            let x = (i as f32 + 0.5) / NOISE_BLOCK as f32 - 0.5;
            let j0 = (x.floor().max(0.0) as usize).min(nb - 1);
            let j1 = (j0 + 1).min(nb - 1);
            let f = (x - j0 as f32).clamp(0.0, 1.0);
            self.nf_row[i] = self.noise_blocks[j0] * (1.0 - f) + self.noise_blocks[j1] * f;
        }

        let row: Vec<u8> = self
            .db_row
            .iter()
            .zip(&self.nf_row)
            .map(|(d, nf)| ((d - nf - WF_DB_MIN) / WF_DB_STEP).clamp(0.0, 255.0) as u8)
            .collect();
        if self.rows.len() < MAX_PENDING_ROWS {
            self.rows.push(row);
        }

        // Peak detection.
        let thr = self.settings.threshold_db;
        let k = (60.0 / self.wf_bin_hz).ceil() as usize;
        let edge = (n / 30).max(k + 1);
        let half = n as f64 / 2.0;
        let t = self.now();
        let mut peaks = Vec::new();
        let db = &self.db_row;
        for b in edge..n - edge {
            let snr = db[b] - self.nf_row[b];
            if snr < thr {
                continue;
            }
            if (b - k..=b + k).any(|j| j != b && (db[j] > db[b] || (db[j] == db[b] && j < b))) {
                continue;
            }
            let (l, c, r) = (db[b - 1], db[b], db[b + 1]);
            let den = l - 2.0 * c + r;
            let delta = if den.abs() > 1e-6 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
            peaks.push(((b as f64 + delta as f64 - half) * self.wf_bin_hz, snr));
        }
        for (off, snr) in peaks {
            self.on_peak(off, snr, t);
        }
        self.cands.retain(|c| t - c.last_t < 1.5);
    }

    fn on_peak(&mut self, off: f64, snr: f32, t: f64) {
        if let Some(c) = self.channels.iter_mut().find(|c| (c.offset - off).abs() < MERGE_HZ) {
            c.last_seen = t;
            c.peak_snr = c.peak_snr.max(snr);
            return;
        }
        let idx = match self.cands.iter().position(|c| (c.offset - off).abs() < MERGE_HZ / 2.0) {
            Some(i) => i,
            None => {
                self.cands.push(Candidate { offset: off, hits: 0, last_t: t, snr });
                self.cands.len() - 1
            }
        };
        let c = &mut self.cands[idx];
        c.hits += 1;
        c.last_t = t;
        c.offset += (off - c.offset) / c.hits as f64;
        c.snr = c.snr.max(snr);
        if c.hits < CAND_HITS || self.channels.len() >= self.settings.max_decoders {
            return;
        }
        let c = self.cands.swap_remove(idx);
        // Skip key clicks / sidebands of a much stronger neighbour.
        if self.channels.iter().any(|ch| (ch.offset - c.offset).abs() < 200.0 && ch.peak_snr > c.snr + 25.0) {
            return;
        }
        let ch_fft = self.ch.as_ref().map_or(1, |s| s.fft_len());
        let bin = ((c.offset / self.ch_bin_hz).round() as i64 + ch_fft as i64 / 2).clamp(0, ch_fft as i64 - 1) as usize;
        let id = self.next_id;
        self.next_id += 1;
        self.channels.push(Channel {
            id,
            offset: c.offset,
            bin,
            dec: CwDecoder::new(self.rate as f32 / self.ch_hop as f32),
            last_seen: t,
            last_char_t: t,
            emitted_seen: 0,
            label_at: 0,
            peak_snr: c.snr,
            label: String::new(),
            cq: false,
        });
    }

    fn publish(&mut self, now: f64) {
        self.last_publish = now;
        let mut views: Vec<DecoderView> = self
            .channels
            .iter_mut()
            .map(|c| {
                if c.emitted_seen != c.label_at {
                    c.label_at = c.emitted_seen;
                    match callsign::spot(&c.dec.text) {
                        Some(s) => {
                            c.label = if s.cq { format!("CQ {}", s.call) } else { s.call };
                            c.cq = s.cq;
                        }
                        None => {
                            c.label.clear();
                            c.cq = false;
                        }
                    }
                }
                DecoderView {
                    id: c.id,
                    offset_hz: c.offset,
                    label: c.label.clone(),
                    cq: c.cq,
                    text: tail(&c.dec.text, VIEW_TEXT).to_string(),
                    wpm: c.dec.wpm(),
                    snr_db: c.dec.snr_db(),
                    key_down: c.dec.key_down(),
                    idle_s: (now - c.last_char_t) as f32,
                }
            })
            .collect();
        views.sort_by(|a, b| a.offset_hz.total_cmp(&b.offset_hz));
        let span = now - self.busy_window_start;
        let load = if span > 0.0 { (self.busy_s / span) as f32 } else { 0.0 };
        if span > 2.0 {
            self.busy_s = 0.0;
            self.busy_window_start = now;
        }
        let mut s = self.shared.lock();
        s.decoders = views;
        s.load = load;
        s.stream_time_s = now;
        if s.new_rows.len() + self.rows.len() > MAX_PENDING_ROWS {
            s.new_rows.clear();
        }
        s.new_rows.append(&mut self.rows);
    }
}
