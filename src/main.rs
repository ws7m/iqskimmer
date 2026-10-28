mod dsp;
mod engine;
mod gui;
mod source;

use clap::Parser;
use engine::{Engine, Settings};
use source::SourceConfig;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Multi-channel CW skimmer for FlexRadio DAX IQ streams.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// FlexRadio IP address, or "auto" to use the first radio discovered on the LAN
    #[arg(long)]
    flex: Option<String>,
    /// DAX IQ channel (assign it to a panadapter in SmartSDR)
    #[arg(long, default_value_t = 1)]
    daxiq: u8,
    /// Request a DAX IQ sample rate (24000, 48000, 96000, 192000); synthetic source rate
    #[arg(long)]
    rate: Option<u32>,
    /// Play a stereo I/Q WAV file (left = I, right = Q)
    #[arg(long)]
    wav: Option<PathBuf>,
    /// Generate a synthetic band of CW stations for testing
    #[arg(long)]
    synth: bool,
    /// Center frequency in Hz for WAV/synthetic sources
    #[arg(long, default_value_t = 7_025_000.0)]
    center: f64,
    /// Number of synthetic stations
    #[arg(long, default_value_t = 20)]
    stations: usize,
    /// Minimum synthetic station SNR (dB in 500 Hz)
    #[arg(long, default_value_t = 6.0)]
    snr: f32,
    /// Detection threshold, dB above noise
    #[arg(long, default_value_t = 12.0)]
    threshold: f32,
    /// Swap I and Q (fixes a spectrum that appears mirrored)
    #[arg(long)]
    swap_iq: bool,
    /// Run without a GUI for N seconds, printing spots and decoded text
    #[arg(long, value_name = "SECONDS")]
    headless: Option<f64>,
    /// List FlexRadios on the LAN and exit
    #[arg(long)]
    discover: bool,
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    if args.discover {
        for r in source::flex::discover(Duration::from_secs(3))? {
            println!("{}", r.describe());
        }
        return Ok(());
    }
    let settings = Settings { threshold_db: args.threshold, swap_iq: args.swap_iq, ..Default::default() };
    let cfg = if let Some(addr) = &args.flex {
        let addr = if addr == "auto" {
            let radios = source::flex::discover(Duration::from_secs(3))?;
            radios.first().ok_or("no FlexRadio found on the LAN")?.ip.clone()
        } else {
            addr.clone()
        };
        Some(SourceConfig::Flex { addr, daxiq_channel: args.daxiq, rate: args.rate })
    } else if let Some(path) = &args.wav {
        Some(SourceConfig::Wav { path: path.clone(), center_hz: args.center })
    } else if args.synth || args.headless.is_some() {
        Some(SourceConfig::Synth {
            rate: args.rate.unwrap_or(48_000),
            center_hz: args.center,
            stations: args.stations,
            min_snr_db: args.snr,
        })
    } else {
        None
    };

    if let Some(secs) = args.headless {
        return headless(cfg.expect("source"), settings, secs);
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]).with_title("IQ Skimmer"),
        ..Default::default()
    };
    eframe::run_native("IQ Skimmer", options, Box::new(move |cc| Ok(Box::new(gui::App::new(cc, settings, cfg)))))
        .map_err(|e| e.to_string())
}

fn headless(cfg: SourceConfig, settings: Settings, secs: f64) -> Result<(), String> {
    let mut engine = Engine::new(settings);
    engine.start(cfg)?;
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_secs(5).min(Duration::from_secs_f64(secs)));
        let s = engine.shared.lock();
        let center = s.center_hz;
        println!(
            "--- t={:.0}s  {} decoders  load {:.0}%  {}",
            s.stream_time_s,
            s.decoders.len(),
            s.load * 100.0,
            s.status
        );
        for d in s.decoders.iter().filter(|d| !d.text.trim().is_empty()) {
            let text: String = d.text.chars().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
            println!(
                "{:>11.2} kHz {:>3.0} WPM {:>3.0} dB  {:<12} | {}",
                (center + d.offset_hz) / 1e3,
                d.wpm,
                d.snr_db,
                d.label,
                text.trim()
            );
        }
    }
    engine.stop();
    Ok(())
}
