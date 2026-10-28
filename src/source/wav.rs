//! Stereo WAV I/Q file playback (left = I, right = Q), paced to real time and looped.

use super::{Pacer, SourceEvent};
use crossbeam_channel::Sender;
use rustfft::num_complex::Complex32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub fn start(
    path: PathBuf,
    center_hz: f64,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
) -> Result<Vec<JoinHandle<()>>, String> {
    let reader = hound::WavReader::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    if spec.channels != 2 {
        return Err(format!("{}: need a 2-channel (I/Q) WAV, got {} channel(s)", path.display(), spec.channels));
    }
    drop(reader);
    let rate = spec.sample_rate;
    let t = std::thread::Builder::new()
        .name("wav-source".into())
        .spawn(move || {
            let _ = tx.send(SourceEvent::Center(center_hz));
            let _ = tx.send(SourceEvent::Status(format!("Playing {} ({} Hz)", path.display(), rate)));
            let block = (rate / 50) as usize;
            let mut pacer = Pacer::new(rate);
            while !stop.load(Ordering::Relaxed) {
                let Ok(mut reader) = hound::WavReader::open(&path) else { return };
                let mut flat: Box<dyn Iterator<Item = f32>> = match spec.sample_format {
                    hound::SampleFormat::Float => Box::new(reader.samples::<f32>().map(|s| s.unwrap_or(0.0))),
                    hound::SampleFormat::Int => {
                        let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
                        Box::new(reader.samples::<i32>().map(move |s| s.unwrap_or(0) as f32 * scale))
                    }
                };
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut data = Vec::with_capacity(block);
                    while data.len() < block {
                        match (flat.next(), flat.next()) {
                            (Some(i), Some(q)) => data.push(Complex32::new(i, q)),
                            _ => break,
                        }
                    }
                    if data.is_empty() {
                        break; // EOF: loop the file
                    }
                    let n = data.len();
                    if tx.send(SourceEvent::Samples { rate, data }).is_err() {
                        return;
                    }
                    pacer.wait(n);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(vec![t])
}
