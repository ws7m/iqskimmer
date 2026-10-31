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
use crate::source::{self, SliceState, SourceConfig, SourceEvent, SourceHandle};
use crate::textlog::{TextLog, log_dir};
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use parking_lot::Mutex;
use rayon::prelude::*;
use rustfft::num_complex::Complex32;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Waterfall rows are quantized dB above the local noise floor.
pub const WF_DB_MIN: f32 = -10.0;
pub const WF_DB_STEP: f32 = 0.25;
const MAX_PENDING_ROWS: usize = 2048;

const ENVELOPE_RATE: u32 = 250;
const NOISE_BLOCK: usize = 64;
/// Default closest spacing of two decoders: about one channelizer bandwidth (~80 Hz). Closer
/// than this they hear the same signal (or the same pair of near zero-beat stations) and only
/// add a second, worse line of text. Adjustable as `Settings::decoder_spacing_hz`.
pub const DEFAULT_DECODER_SPACING_HZ: f64 = 75.0;
const CAND_HITS: u32 = 4;
const CHANNEL_TIMEOUT_S: f64 = 25.0;
/// How long a shown decoder stays up through a pause, and stays listed after its last
/// character. Covers a station calling CQ and then listening.
pub const HOLD_S: f64 = 10.0;
/// Median rise of the per-block noise level that marks a waterfall row as impulse noise.
/// Normal rows scatter well under 1 dB around zero.
const IMPULSE_RISE_DB: f32 = 1.5;
/// How far a CW peak must stand above the quieter of its two sides, 80-140 Hz away.
const MIN_PROMINENCE_DB: f32 = 8.0;
const VIEW_TEXT: usize = 400;

/// Separates Morse from digital modes (FT8, MFSK/Olivia tone pips) and noise, all of which
/// also produce on/off keying in a single channel:
/// * a speed pinned near the 60 WPM limit means no real dit length was found;
/// * FSK tones keep a neighbouring frequency busy while this one is quiet;
/// * marks should cluster into dits and dahs at roughly 1:3;
/// * pips and noise decode almost entirely to dit-only characters (E I S H 5), which are
///   about a quarter of real text.
/// Looser limits keep a channel that already qualified, so it doesn't flicker.
fn is_cw(c: &Channel) -> bool {
    let (junk, chars) = c.dec.junk_share();
    if c.dec.mark_count() < 8 || chars < 10 {
        return false;
    }
    let (ratio, _) = c.dec.rhythm();
    let wpm = c.dec.wpm();
    if c.cw {
        junk < 0.7 && wpm < 55.0 && c.neighbor_busy < 0.6 && (1.6..=5.0).contains(&ratio)
    } else {
        junk < 0.6 && wpm < 50.0 && c.neighbor_busy < 0.5 && (1.8..=4.5).contains(&ratio)
    }
}

/// Recent key-down state of a channel, one bit per envelope frame (~2 s). Bits are placed by
/// the engine's global frame number, so histories of different channels line up.
#[derive(Default)]
struct KeyHistory {
    bits: [u64; 8],
}

impl KeyHistory {
    fn set(&mut self, frame: u64, down: bool) {
        let pos = (frame % (64 * self.bits.len() as u64)) as usize;
        let (w, b) = (pos / 64, pos % 64);
        self.bits[w] = (self.bits[w] & !(1 << b)) | ((down as u64) << b);
    }

    /// Frames where both are keyed, and where either is.
    fn overlap(&self, other: &Self) -> (u32, u32) {
        let mut both = 0;
        let mut either = 0;
        for (a, b) in self.bits.iter().zip(&other.bits) {
            both += (a & b).count_ones();
            either += (a | b).count_ones();
        }
        (both, either)
    }
}

/// A strong signal can be detected twice a little apart in frequency. Both decoders then hear
/// the same keying and print identical text, so hide the weaker of any two CW decoders that
/// are within 300 Hz and key down together almost all the time.
/// Two shown decoders closer than `spacing_hz` can't be told apart (they share most of their
/// channel), so the weaker is hidden whatever its keying.
/// Pairs that don't key during the 2 s history (a pause) can't be compared, so a duplicate
/// stays hidden for `HOLD_S` after it was last seen copying its neighbour.
fn mark_duplicates(channels: &mut [Channel], now: f64, spacing_hz: f64) {
    let mut idx: Vec<usize> = (0..channels.len()).filter(|&i| channels[i].shown(now)).collect();
    idx.sort_by(|&a, &b| channels[a].offset.total_cmp(&channels[b].offset));
    for (n, &i) in idx.iter().enumerate() {
        for &j in &idx[n + 1..] {
            if channels[j].offset - channels[i].offset > 300.0 {
                break;
            }
            let close = channels[j].offset - channels[i].offset < spacing_hz;
            let (both, either) = channels[i].key_hist.overlap(&channels[j].key_hist);
            if close || (either >= 50 && both as f32 > 0.8 * either as f32) {
                // Keep hiding the one already hidden, so the two don't trade places (or both
                // end up hidden) as their signal-to-noise ratios wobble.
                let hidden = |k: usize| now < channels[k].duplicate_until;
                let weaker = if hidden(i) {
                    i
                } else if hidden(j) {
                    j
                } else if channels[i].dec.snr_db() < channels[j].dec.snr_db() {
                    i
                } else {
                    j
                };
                channels[weaker].duplicate_until = now + HOLD_S;
            }
        }
    }
}

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
    /// Save each shown decoder's text to Documents/IQSkimmer.
    pub log_text: bool,
    /// Closest two decoders may be (Hz); the weaker of a closer pair is hidden.
    pub decoder_spacing_hz: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold_db: 12.0,
            swap_iq: false,
            max_decoders: 500,
            log_text: false,
            decoder_spacing_hz: DEFAULT_DECODER_SPACING_HZ,
        }
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
    pub neighbor_busy: f32,
    pub rhythm: (f32, f32),
    pub junk: f32,
    /// Looks like Morse. Other decoders sit on digital modes or noise and aren't shown.
    pub cw: bool,
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
    /// The radio's open slices on the skimmed panadapter, when the source is a radio.
    pub slices: Vec<SliceState>,
    /// Text logging hit an error and turned itself off.
    pub log_failed: bool,
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

    /// Tune slice `slice` (A-D) of the radio, if the source is one, to `hz` in CW.
    pub fn tune_cw(&self, slice: char, hz: f64) -> bool {
        self.source.as_ref().is_some_and(|s| s.tune_cw(slice, hz))
    }

    /// Move the radio's panadapter center to `hz` (radio sources only).
    pub fn set_radio_center(&self, hz: f64) -> bool {
        self.source.as_ref().is_some_and(|s| s.set_center(hz))
    }

    /// Change the radio's I/Q sample rate while running (radio sources only).
    pub fn set_radio_rate(&self, rate: u32) -> bool {
        self.source.as_ref().is_some_and(|s| s.set_rate(rate))
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
    /// Fraction of gap time with a comparably strong signal close by (FSK tones hop there).
    neighbor_busy: f32,
    /// Keyed like Morse rather than a digital mode or noise; see `is_cw`.
    cw: bool,
    /// Shown until then: refreshed while `cw` holds, so a station pausing to listen stays up.
    shown_until: f64,
    /// Hidden until then as a copy of a stronger decoder close by (the same signal seen twice).
    duplicate_until: f64,
    key_hist: KeyHistory,
    /// Open text log, and how much of the decoder's text it has written.
    log: Option<TextLog>,
    logged_to: usize,
    peak_snr: f32,
    label: String,
    cq: bool,
}

impl Channel {
    /// Counts as CW now or did within the last `HOLD_S`.
    fn shown(&self, now: f64) -> bool {
        self.cw || now < self.shown_until
    }

    /// Shown, and not hidden as a duplicate.
    fn visible(&self, now: f64) -> bool {
        self.shown(now) && now >= self.duplicate_until
    }
}

struct Candidate {
    offset: f64,
    hits: u32,
    last_t: f64,
    snr: f32,
}

struct Dsp {
    /// Envelope frames processed, for aligning per-channel key histories.
    env_frames: u64,
    impulse_run: u32,
    impulse_flags: VecDeque<bool>,
    pending_peaks: VecDeque<(f64, Vec<(f64, f32)>)>,
    wf_win: usize,
    /// Dev aid: with IQRECORD=<file.wav>, raw I/Q is written there (restarted on rate changes).
    recorder: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
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
        env_frames: 0,
        impulse_run: 0,
        impulse_flags: VecDeque::new(),
        pending_peaks: VecDeque::new(),
        wf_win: 0,
        recorder: None,
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
                s.slices.clear();
            }
            SourceEvent::Center(c) => {
                if (c - self.center).abs() > 0.5 {
                    // Signals keep their absolute frequency when the panadapter moves, so keep
                    // their decoders, re-aimed at the new offset, unless they left the span.
                    let shift = c - self.center;
                    let keep = self.center != 0.0 && self.rate != 0;
                    self.center = c;
                    self.cands.clear();
                    if keep {
                        let edge = self.rate as f64 * 0.48;
                        for ch in &mut self.channels {
                            ch.offset -= shift;
                        }
                        self.channels.retain(|ch| ch.offset.abs() < edge);
                        for i in 0..self.channels.len() {
                            self.channels[i].bin = self.channel_bin(self.channels[i].offset);
                        }
                    } else {
                        self.channels.clear();
                    }
                    self.shared.lock().center_hz = c;
                }
            }
            SourceEvent::Status(s) => self.shared.lock().status = s,
            SourceEvent::Slices(list) => self.shared.lock().slices = list,
            SourceEvent::Samples { rate, mut data } => {
                if rate != self.rate {
                    self.configure(rate);
                }
                if self.settings.swap_iq {
                    data.iter_mut().for_each(|s| *s = Complex32::new(s.im, s.re));
                }
                if let Some(w) = &mut self.recorder {
                    for s in &data {
                        let _ = w.write_sample(s.re);
                        let _ = w.write_sample(s.im);
                    }
                }
                let t0 = Instant::now();
                self.process(&data);
                self.busy_s += t0.elapsed().as_secs_f64();
            }
        }
    }

    fn configure(&mut self, rate: u32) {
        self.rate = rate;
        if let Some(path) = std::env::var_os("IQRECORD") {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            self.recorder = hound::WavWriter::create(path, spec).ok();
        }
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
        self.wf_win = wf_win;
        self.impulse_run = 0;
        self.impulse_flags.clear();
        self.pending_peaks.clear();
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

    /// Channelizer bin (fft-shifted) for a frequency offset from the center.
    fn channel_bin(&self, offset: f64) -> usize {
        let ch_fft = self.ch.as_ref().map_or(1, |s| s.fft_len()) as i64;
        ((offset / self.ch_bin_hz).round() as i64 + ch_fft / 2).clamp(0, ch_fft - 1) as usize
    }

    fn rows_per_s(&self) -> f32 {
        self.rate as f32 / self.wf_hop as f32
    }

    /// Waterfall rows sharing samples with any one row.
    fn rows_per_window(&self) -> usize {
        self.wf_win.div_ceil(self.wf_hop)
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
        let ring_lo = (40.0 / self.ch_bin_hz).ceil() as usize;
        let ring_hi = (250.0 / self.ch_bin_hz).ceil() as usize;
        let busy_k = (env_dt / 3.0) as f32;
        let frame0 = self.env_frames;
        self.env_frames += n_frames as u64;
        self.channels.par_iter_mut().for_each(|c| {
            for (i, f) in frames.chunks_exact(stride).enumerate() {
                c.dec.feed(f[c.bin]);
                c.key_hist.set(frame0 + i as u64, c.dec.key_down());
                if !c.dec.key_down() && c.dec.snr_db() >= 10.0 {
                    // Strongest neighbour 40-250 Hz away while this channel is in a gap.
                    let lo = c.bin.saturating_sub(ring_hi)..c.bin.saturating_sub(ring_lo);
                    let hi = (c.bin + ring_lo).min(stride)..(c.bin + ring_hi + 1).min(stride);
                    let ring = lo.chain(hi).map(|j| f[j]).fold(0.0f32, f32::max);
                    // Comparable to this signal and clearly above noise (the max over ~36 noise
                    // bins alone reaches ~5 dB above the mean).
                    let level = (c.dec.signal_db() - 10.0).max(c.dec.noise_db() + 10.0);
                    let busy = 10.0 * (ring + 1e-30).log10() > level;
                    c.neighbor_busy += busy_k * (busy as u8 as f32 - c.neighbor_busy);
                }
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
        let levels: Vec<f32> = self
            .db_row
            .chunks(NOISE_BLOCK)
            .map(|blk| {
                tmp.clear();
                tmp.extend_from_slice(blk);
                let k = tmp.len() * 3 / 10;
                // 30th percentile of exponentially distributed power sits 4.5 dB below the mean.
                *tmp.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1 + 4.5
            })
            .collect();

        // Impulse noise (static crashes, power-line noise) lifts the whole spectrum for a
        // moment. The median block rise is immune to a few strong signals. A rise lasting
        // over a second is a real level change (gain, band) and is accepted as the new floor.
        let impulse = if fresh {
            false
        } else {
            let mut rise: Vec<f32> = levels.iter().zip(&self.noise_blocks).map(|(v, nb)| v - nb).collect();
            let m = rise.len() / 2;
            *rise.select_nth_unstable_by(m, |a, b| a.total_cmp(b)).1 > IMPULSE_RISE_DB
        };
        self.impulse_run = if impulse { self.impulse_run + 1 } else { 0 };
        let impulse = impulse && (self.impulse_run as f32) < self.rows_per_s();
        if !impulse {
            for (nbk, v) in self.noise_blocks.iter_mut().zip(&levels) {
                *nbk = if fresh { *v } else { *nbk + 0.05 * (v - *nbk) };
            }
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
        let (side_lo, side_hi) = ((80.0 / self.wf_bin_hz) as usize, (140.0 / self.wf_bin_hz).ceil() as usize);
        let edge = (n / 30).max(side_hi + 1);
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
            // CW is narrow: the spectrum must fall away on at least one side within ~100 Hz.
            // Wide signals (SSB, FT8 and other digital modes) fill both sides.
            let side = |r: std::ops::RangeInclusive<usize>| {
                let len = r.clone().count() as f32;
                10.0 * (r.map(|j| p[j]).sum::<f32>() / len + 1e-30).log10()
            };
            let quieter = side(b - side_hi..=b - side_lo).min(side(b + side_lo..=b + side_hi));
            if db[b] - quieter < MIN_PROMINENCE_DB {
                continue;
            }
            let (l, c, r) = (db[b - 1], db[b], db[b + 1]);
            let den = l - 2.0 * c + r;
            let delta = if den.abs() > 1e-6 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
            peaks.push(((b as f64 + delta as f64 - half) * self.wf_bin_hz, snr));
        }
        // A burst smears across every row whose FFT window overlaps it, so hold each row's
        // peaks until its neighbours are known and drop them if any neighbour was impulsive.
        let guard = self.rows_per_window();
        self.impulse_flags.push_back(impulse);
        if self.impulse_flags.len() > 2 * guard + 1 {
            self.impulse_flags.pop_front();
        }
        self.pending_peaks.push_back((t, peaks));
        if self.pending_peaks.len() > guard {
            let (pt, peaks) = self.pending_peaks.pop_front().expect("non-empty");
            if !self.impulse_flags.iter().any(|f| *f) {
                for (off, snr) in peaks {
                    self.on_peak(off, snr, pt);
                }
            }
        }
        self.cands.retain(|c| t - c.last_t < 1.5);
    }

    fn on_peak(&mut self, off: f64, snr: f32, t: f64) {
        let spacing = self.settings.decoder_spacing_hz;
        if let Some(c) = self.channels.iter_mut().find(|c| (c.offset - off).abs() < spacing) {
            c.last_seen = t;
            c.peak_snr = c.peak_snr.max(snr);
            return;
        }
        let idx = match self.cands.iter().position(|c| (c.offset - off).abs() < spacing / 2.0) {
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
        // Skip key clicks / sidebands of a much stronger neighbour, and anything a decoder
        // created while this candidate was collecting hits already covers.
        if self.channels.iter().any(|ch| {
            let d = (ch.offset - c.offset).abs();
            d < spacing || (d < 200.0 && ch.peak_snr > c.snr + 25.0)
        }) {
            return;
        }
        let bin = self.channel_bin(c.offset);
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
            neighbor_busy: 0.0,
            cw: false,
            shown_until: f64::NEG_INFINITY,
            duplicate_until: f64::NEG_INFINITY,
            key_hist: KeyHistory::default(),
            log: None,
            logged_to: 0,
            peak_snr: c.snr,
            label: String::new(),
            cq: false,
        });
    }

    /// Append new text of every shown decoder to its file. A decoder's file is created when it
    /// is first shown and starts with everything decoded so far.
    fn write_logs(&mut self, now: f64) {
        if !self.settings.log_text {
            for c in &mut self.channels {
                c.log = None;
            }
            return;
        }
        let mut error = None;
        for c in self.channels.iter_mut().filter(|c| c.visible(now)) {
            if c.log.is_none() {
                match TextLog::create(self.center + c.offset) {
                    Ok(l) => {
                        c.log = Some(l);
                        c.logged_to = 0;
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            let new = c.dec.text_since(c.logged_to);
            if !new.is_empty() {
                if let Err(e) = c.log.as_mut().expect("opened above").append(new, now) {
                    error = Some(e);
                    break;
                }
                c.logged_to = c.dec.text_end();
            }
        }
        if let Some(e) = error {
            // Stop rather than retry every 100 ms; the GUI shows the toggle as off.
            self.settings.log_text = false;
            let mut s = self.shared.lock();
            s.status = format!("Text logging stopped: {} ({})", e, log_dir().display());
            s.log_failed = true;
        }
    }

    fn publish(&mut self, now: f64) {
        self.last_publish = now;
        for c in &mut self.channels {
            c.cw = is_cw(c);
            if c.cw {
                c.shown_until = now + HOLD_S;
            }
        }
        mark_duplicates(&mut self.channels, now, self.settings.decoder_spacing_hz);
        self.write_logs(now);
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
                    neighbor_busy: c.neighbor_busy,
                    rhythm: c.dec.rhythm(),
                    junk: c.dec.junk_share().0,
                    cw: c.visible(now),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u64, offset: f64, keying: impl Fn(usize) -> bool) -> Channel {
        let mut c = Channel {
            id,
            offset,
            bin: 0,
            dec: CwDecoder::new(250.0),
            last_seen: 0.0,
            last_char_t: 0.0,
            emitted_seen: 0,
            label_at: 0,
            neighbor_busy: 0.0,
            cw: true,
            shown_until: f64::NEG_INFINITY,
            duplicate_until: f64::NEG_INFINITY,
            key_hist: KeyHistory::default(),
            log: None,
            logged_to: 0,
            peak_snr: 0.0,
            label: String::new(),
            cq: false,
        };
        for i in 0..512 {
            c.key_hist.set(i as u64, keying(i));
        }
        c
    }

    #[test]
    fn hides_one_of_two_decoders_keying_together() {
        let dits = |i: usize| (i / 15) % 2 == 0;
        let mut chans = vec![channel(1, 1000.0, dits), channel(2, 1070.0, dits), channel(3, 2000.0, dits)];
        mark_duplicates(&mut chans, 0.0, DEFAULT_DECODER_SPACING_HZ);
        // 1 and 2 are the same signal; 3 is too far away to be one of them.
        assert_eq!(chans.iter().filter(|c| !c.visible(0.0)).count(), 1);
        assert!(chans[2].visible(0.0));
    }

    #[test]
    fn aligns_histories_of_decoders_created_at_different_times() {
        let dits = |i: usize| (i / 15) % 2 == 0;
        let mut early = channel(1, 1000.0, |_| false);
        let mut late = channel(2, 1060.0, |_| false);
        for f in 0..900u64 {
            early.key_hist.set(f, dits(f as usize));
            if f >= 300 {
                late.key_hist.set(f, dits(f as usize));
            }
        }
        let mut chans = vec![early, late];
        mark_duplicates(&mut chans, 0.0, DEFAULT_DECODER_SPACING_HZ);
        assert_eq!(chans.iter().filter(|c| !c.visible(0.0)).count(), 1);
    }

    #[test]
    fn hides_one_of_two_decoders_closer_than_merge_distance() {
        // Independent keying, but only 60 Hz apart: they can't be told apart.
        let mut chans = vec![
            channel(1, 1000.0, |i| (i / 15) % 2 == 0),
            channel(2, 1060.0, |i| (i / 22) % 3 == 0),
        ];
        mark_duplicates(&mut chans, 0.0, DEFAULT_DECODER_SPACING_HZ);
        assert_eq!(chans.iter().filter(|c| !c.visible(0.0)).count(), 1);
    }

    #[test]
    fn keeps_hiding_the_same_one_of_a_pair() {
        let mut chans = vec![channel(1, 1000.0, |_| false), channel(2, 1050.0, |_| false)];
        chans[0].duplicate_until = 5.0; // decoder 1 was the one hidden last time
        mark_duplicates(&mut chans, 1.0, DEFAULT_DECODER_SPACING_HZ);
        assert!(!chans[0].visible(1.0));
        assert!(chans[1].visible(1.0));
    }

    #[test]
    fn keeps_neighbours_keying_independently() {
        let mut chans = vec![
            channel(1, 1000.0, |i| (i / 15) % 2 == 0),
            channel(2, 1100.0, |i| (i / 22) % 3 == 0),
        ];
        mark_duplicates(&mut chans, 0.0, DEFAULT_DECODER_SPACING_HZ);
        assert!(chans.iter().all(|c| c.visible(0.0)));
    }
}
