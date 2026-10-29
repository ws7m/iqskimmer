//! I/Q sample sources. Each source runs on its own thread(s) and pushes
//! `SourceEvent`s into a channel consumed by the DSP engine.

pub mod flex;
pub mod synth;
pub mod vita;
pub mod wav;

use crossbeam_channel::Sender;
use rustfft::num_complex::Complex32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub enum SourceEvent {
    Samples { rate: u32, data: Vec<Complex32> },
    /// Absolute frequency (Hz) of the I/Q stream's center.
    Center(f64),
    Status(String),
    /// The radio's slice A, if one is open.
    SliceA(Option<SliceState>),
    /// Sent by the engine itself when switching sources, to discard state from the old one.
    Reset,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SliceState {
    pub hz: f64,
    pub mode: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceConfig {
    Flex { addr: String, daxiq_channel: u8, rate: Option<u32> },
    Wav { path: PathBuf, center_hz: f64 },
    Synth { rate: u32, center_hz: f64, stations: usize, min_snr_db: f32 },
}

pub struct SourceHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    radio: Option<Sender<flex::RadioCmd>>,
}

impl SourceHandle {
    /// Tune the radio's slice A to `hz` in CW. Returns false if this source has no radio control.
    pub fn tune_cw(&self, hz: f64) -> bool {
        self.radio.as_ref().is_some_and(|r| r.send(flex::RadioCmd::TuneCw(hz)).is_ok())
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub fn start(cfg: SourceConfig, tx: Sender<SourceEvent>) -> Result<SourceHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (threads, radio) = match cfg {
        SourceConfig::Flex { addr, daxiq_channel, rate } => {
            let (t, r) = flex::start(&addr, daxiq_channel, rate, tx, stop.clone())?;
            (t, Some(r))
        }
        SourceConfig::Wav { path, center_hz } => (wav::start(path, center_hz, tx, stop.clone())?, None),
        SourceConfig::Synth { rate, center_hz, stations, min_snr_db } => {
            (synth::start(rate, center_hz, stations, min_snr_db, tx, stop.clone()), None)
        }
    };
    Ok(SourceHandle { stop, threads, radio })
}

/// Paces a source to real time: call `wait(n)` after producing `n` samples.
pub(crate) struct Pacer {
    start: std::time::Instant,
    rate: f64,
    produced: u64,
}

impl Pacer {
    pub fn new(rate: u32) -> Self {
        Self { start: std::time::Instant::now(), rate: rate as f64, produced: 0 }
    }

    pub fn wait(&mut self, n: usize) {
        self.produced += n as u64;
        let due = std::time::Duration::from_secs_f64(self.produced as f64 / self.rate);
        let elapsed = self.start.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
    }
}
