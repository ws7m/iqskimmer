//! Saving each decoder's text to its own file in Documents/IQSkimmer.

use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// A pause this long starts a new line, stamped with the time.
const NEW_LINE_AFTER_S: f64 = 10.0;

/// Documents/IQSkimmer, or `IQSKIMMER_LOG_DIR` if set.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IQSKIMMER_LOG_DIR") {
        return dir.into();
    }
    dirs::document_dir().or_else(dirs::home_dir).unwrap_or_default().join("IQSkimmer")
}

pub struct TextLog {
    file: File,
    /// Stream time of the last write, for starting a new line after a pause.
    last_t: Option<f64>,
}

impl TextLog {
    /// Creates `<date>_<time>_<frequency kHz>.txt`, e.g. `20261028_193512_7024.77.txt`.
    pub fn create(freq_hz: f64) -> std::io::Result<Self> {
        let dir = log_dir();
        std::fs::create_dir_all(&dir)?;
        let now = Local::now();
        let path = dir.join(format!("{}_{:.2}.txt", now.format("%Y%m%d_%H%M%S"), freq_hz / 1e3));
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "IQ Skimmer decode, {:.2} kHz, started {}", freq_hz / 1e3, now.format("%Y-%m-%d %H:%M:%S"))?;
        Ok(Self { file, last_t: None })
    }

    /// Append newly decoded text. `now` is stream time in seconds.
    pub fn append(&mut self, text: &str, now: f64) -> std::io::Result<()> {
        let new_line = self.last_t.is_none_or(|t| now - t > NEW_LINE_AFTER_S);
        if new_line {
            write!(self.file, "\n[{}] ", Local::now().format("%H:%M:%S"))?;
            self.file.write_all(text.trim_start().as_bytes())?;
        } else {
            self.file.write_all(text.as_bytes())?;
        }
        self.last_t = Some(now);
        Ok(())
    }
}
