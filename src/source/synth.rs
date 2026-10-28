//! Synthetic band of CW stations in white noise, for testing without a radio.

use super::{Pacer, SourceEvent};
use crate::dsp::morse;
use crossbeam_channel::Sender;
use rand::Rng;
use rustfft::num_complex::Complex32;
use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

const PREFIXES: &[&str] = &[
    "K", "W", "N", "AA", "VE", "DL", "G", "F", "EA", "JA", "UA", "I", "OH", "SP", "OK", "PA", "ON", "HB9", "VK", "ZL",
    "LZ", "YU", "S5", "9A",
];

pub fn random_call(rng: &mut impl Rng) -> String {
    let mut c = PREFIXES[rng.gen_range(0..PREFIXES.len())].to_string();
    if !c.ends_with(|ch: char| ch.is_ascii_digit()) {
        c.push((b'0' + rng.gen_range(0..10)) as char);
    }
    for _ in 0..rng.gen_range(1..=3) {
        c.push((b'A' + rng.gen_range(0..26)) as char);
    }
    c
}

pub struct Station {
    pub offset_hz: f32,
    pub call: String,
    pub wpm: f32,
    amp: f32,
    phase: f32,
    dphase: f32,
    /// Keying schedule as (key down, duration in samples), repeated forever.
    schedule: Vec<(bool, usize)>,
    pos: usize,
    left: usize,
    env: f32,
}

impl Station {
    pub fn new(rate: u32, offset_hz: f32, call: String, wpm: f32, snr_db: f32, message: &str) -> Self {
        let dit = (1.2 / wpm * rate as f32) as usize;
        let mut schedule = Vec::new();
        for word in message.split_whitespace() {
            for ch in word.chars() {
                if let Some(code) = morse::encode(ch) {
                    for (i, el) in code.chars().enumerate() {
                        if i > 0 {
                            schedule.push((false, dit));
                        }
                        schedule.push((true, if el == '-' { 3 * dit } else { dit }));
                    }
                    schedule.push((false, 3 * dit));
                }
            }
            schedule.push((false, 4 * dit));
        }
        schedule.push((false, 20 * dit));
        // SNR is referenced to a 500 Hz bandwidth with unit total noise power over the full span.
        let amp = (10f32.powf(snr_db / 10.0) * 500.0 / rate as f32).sqrt();
        let left = schedule[0].1;
        Self {
            offset_hz,
            call,
            wpm,
            amp,
            phase: 0.0,
            dphase: TAU * offset_hz / rate as f32,
            schedule,
            pos: 0,
            left,
            env: 0.0,
        }
    }

    #[inline]
    fn next(&mut self, ramp: f32) -> Complex32 {
        if self.left == 0 {
            self.pos = (self.pos + 1) % self.schedule.len();
            self.left = self.schedule[self.pos].1;
        }
        self.left -= 1;
        let target = if self.schedule[self.pos].0 { 1.0 } else { 0.0 };
        self.env += (target - self.env).clamp(-ramp, ramp);
        self.phase = (self.phase + self.dphase) % TAU;
        let a = self.amp * self.env;
        Complex32::new(a * self.phase.cos(), a * self.phase.sin())
    }
}

pub struct Band {
    pub stations: Vec<Station>,
    ramp: f32,
    noise_sigma: f32,
}

impl Band {
    pub fn random(rate: u32, n: usize, min_snr_db: f32, rng: &mut impl Rng) -> Self {
        let span = rate as f32 * 0.8;
        let mut offsets: Vec<f32> = Vec::new();
        while offsets.len() < n {
            let f = rng.gen_range(-span / 2.0..span / 2.0);
            if offsets.iter().all(|o| (o - f).abs() > 250.0) {
                offsets.push(f);
            }
        }
        let stations = offsets
            .into_iter()
            .map(|f| {
                let call = random_call(rng);
                let msg = match rng.gen_range(0..3) {
                    0 => format!("CQ CQ DE {call} {call} K"),
                    1 => format!("CQ TEST {call} {call}"),
                    _ => format!("TU {call} TEST"),
                };
                let wpm = rng.gen_range(16.0..36.0);
                let snr = rng.gen_range(min_snr_db..min_snr_db + 20.0);
                Station::new(rate, f, call, wpm, snr, &msg)
            })
            .collect();
        Self::new(rate, stations)
    }

    pub fn new(rate: u32, stations: Vec<Station>) -> Self {
        Self { stations, ramp: 1.0 / (0.005 * rate as f32), noise_sigma: std::f32::consts::FRAC_1_SQRT_2 }
    }

    pub fn fill(&mut self, out: &mut [Complex32], rng: &mut impl Rng) {
        for s in out.iter_mut() {
            // Box-Muller complex Gaussian noise
            let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
            let u2: f32 = rng.r#gen();
            let r = self.noise_sigma * (-2.0 * u1.ln()).sqrt();
            let mut v = Complex32::new(r * (TAU * u2).cos(), r * (TAU * u2).sin());
            for st in &mut self.stations {
                v += st.next(self.ramp);
            }
            *s = v * 0.01;
        }
    }
}

pub fn start(
    rate: u32,
    center_hz: f64,
    n: usize,
    min_snr_db: f32,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let t = std::thread::Builder::new()
        .name("synth-source".into())
        .spawn(move || {
            let mut rng = rand::thread_rng();
            let mut band = Band::random(rate, n, min_snr_db, &mut rng);
            let mut summary: Vec<_> = band.stations.iter().map(|s| (s.offset_hz, s.call.clone(), s.wpm)).collect();
            summary.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (f, call, wpm) in &summary {
                eprintln!("synth: {:>10.1} Hz  {call:<8} {wpm:.0} WPM", center_hz + *f as f64);
            }
            let _ = tx.send(SourceEvent::Center(center_hz));
            let _ = tx.send(SourceEvent::Status(format!("Synthetic band: {n} stations @ {rate} Hz")));
            let block = (rate / 50) as usize;
            let mut pacer = Pacer::new(rate);
            while !stop.load(Ordering::Relaxed) {
                let mut data = vec![Complex32::default(); block];
                band.fill(&mut data, &mut rng);
                if tx.send(SourceEvent::Samples { rate, data }).is_err() {
                    return;
                }
                pacer.wait(block);
            }
        })
        .expect("spawn synth thread");
    vec![t]
}
