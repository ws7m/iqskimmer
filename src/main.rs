// A GUI app on Windows: no console window when started from Explorer. See attach_console.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod alerts;
mod dsp;
mod engine;
mod gui;
mod source;
mod textlog;

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
    /// DAX IQ sample rate (24000, 48000, 96000, 192000; default 96000), or synthetic source rate (default 48000)
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
    /// Save each decoder's text to Documents/IQSkimmer (date_time_frequency.txt)
    #[arg(long)]
    save_text: bool,
    /// Run without a GUI for N seconds, printing spots and decoded text
    #[arg(long, value_name = "SECONDS")]
    headless: Option<f64>,
    /// List FlexRadios on the LAN and exit
    #[arg(long)]
    discover: bool,
}

/// The radio's own default (48 kHz) covers only part of a typical panadapter.
const DEFAULT_FLEX_RATE: u32 = 96_000;

/// A Windows GUI-subsystem program has no console, so `--help`, `--discover` and `--headless`
/// would print nothing when run from a terminal. Borrow the terminal's console if there is one.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain Win32 call with no pointers; fails harmlessly when there is no parent console.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() -> Result<(), String> {
    attach_console();
    let args = Args::parse();
    if args.discover {
        for r in source::flex::discover(Duration::from_secs(3))? {
            println!("{}", r.describe());
        }
        return Ok(());
    }
    let settings = Settings {
        threshold_db: args.threshold,
        swap_iq: args.swap_iq,
        log_text: args.save_text,
        ..Default::default()
    };
    let cfg = if let Some(addr) = &args.flex {
        let addr = if addr == "auto" {
            let radios = source::flex::discover(Duration::from_secs(3))?;
            radios.first().ok_or("no FlexRadio found on the LAN")?.ip.clone()
        } else {
            addr.clone()
        };
        Some(SourceConfig::Flex { addr, daxiq_channel: args.daxiq, rate: Some(args.rate.unwrap_or(DEFAULT_FLEX_RATE)) })
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
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_title(concat!("IQ Skimmer v", env!("CARGO_PKG_VERSION"))),
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
            "--- t={:.0}s  {} decoders ({} CW)  load {:.0}%  {}",
            s.stream_time_s,
            s.decoders.len(),
            s.decoders.iter().filter(|d| d.cw).count(),
            s.load * 100.0,
            s.status
        );
        if !s.slices.is_empty() {
            let list: Vec<String> =
                s.slices.iter().map(|sl| format!("{} {:.3} kHz {}", sl.letter, sl.hz / 1e3, sl.mode)).collect();
            println!("    slices: {}", list.join(", "));
        }
        if std::env::var_os("IQDBG").is_some() {
            // Every decoder, including silent ones, for diagnosing detection.
            for d in &s.decoders {
                eprintln!(
                    "dbg {:>11.3} kHz snr {:>5.1} wpm {:>4.0} idle {:>5.1}s chars {:>4} key {} busy {:.2} ratio {:.2} short {:.2} junk {:.2}",
                    (center + d.offset_hz) / 1e3,
                    d.snr_db,
                    d.wpm,
                    d.idle_s,
                    d.text.trim().len(),
                    d.key_down as u8,
                    d.neighbor_busy,
                    d.rhythm.0,
                    d.rhythm.1,
                    d.junk
                );
            }
        }
        for d in s.decoders.iter().filter(|d| d.cw && !d.text.trim().is_empty()) {
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
