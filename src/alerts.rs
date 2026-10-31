//! Callsign alerts: watch decoded text for callsigns the operator is looking for.

use chrono::{DateTime, Local};
use std::time::{Duration, Instant};

/// Alerts disappear this long after they were raised.
pub const ALERT_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// A watched call heard again within this distance (Hz) is the same alert.
const SAME_FREQ_HZ: f64 = 200.0;

pub struct Alert {
    pub when: DateTime<Local>,
    pub call: String,
    /// Absolute frequency (Hz).
    pub hz: f64,
    pub mode: &'static str,
    raised: Instant,
}

#[derive(Default)]
pub struct Alerts {
    watch: Vec<String>,
    /// Newest first.
    pub list: Vec<Alert>,
}

/// Callsigns from a comma (or space) separated list, upper-cased, without duplicates.
pub fn parse_watch_list(text: &str) -> Vec<String> {
    let mut calls: Vec<String> = Vec::new();
    for c in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let c = c.trim().to_ascii_uppercase();
        if !c.is_empty() && !calls.contains(&c) {
            calls.push(c);
        }
    }
    calls
}

/// Words of decoded text: runs of letters, digits and '/', so calls stuck to punctuation or
/// prosigns (`W1AW+`, `<BK>W1AW`) still count.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '/')).filter(|w| !w.is_empty())
}

/// `W1AW` matches `W1AW`, `W1AW/P` and `VE3/W1AW`, but not `W1AWX`.
fn word_is_call(word: &str, call: &str) -> bool {
    word == call || word.split('/').any(|part| part == call)
}

impl Alerts {
    pub fn set_watch_list(&mut self, text: &str) {
        self.watch = parse_watch_list(text);
    }

    pub fn watching(&self) -> bool {
        !self.watch.is_empty()
    }

    /// Raise an alert for each watched call in `text`, decoded at `hz` (absolute), unless one
    /// already exists for that call on that frequency. Returns how many were raised.
    pub fn check(&mut self, text: &str, hz: f64, now: Instant) -> usize {
        let mut raised = 0;
        for call in &self.watch {
            if !words(text).any(|w| word_is_call(w, call)) {
                continue;
            }
            if self.list.iter().any(|a| &a.call == call && (a.hz - hz).abs() < SAME_FREQ_HZ) {
                continue;
            }
            self.list.insert(0, Alert { when: Local::now(), call: call.clone(), hz, mode: "CW", raised: now });
            raised += 1;
        }
        raised
    }

    /// Drop alerts older than `ALERT_LIFETIME`.
    pub fn expire(&mut self, now: Instant) {
        self.list.retain(|a| now.duration_since(a.raised) < ALERT_LIFETIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_watch_list() {
        assert_eq!(parse_watch_list(" w1aw, K1ABC,,w1aw  VE3XYZ "), vec!["W1AW", "K1ABC", "VE3XYZ"]);
        assert!(parse_watch_list(" , ").is_empty());
    }

    #[test]
    fn matches_whole_calls_only() {
        let mut a = Alerts::default();
        a.set_watch_list("W1AW");
        let t = Instant::now();
        a.check("CQ DE W1AWX K", 7_025_000.0, t);
        assert!(a.list.is_empty());
        a.check("CQ DE W1AW/P K", 7_025_000.0, t);
        a.check("<BK>W1AW+ TU", 7_040_000.0, t);
        assert_eq!(a.list.len(), 2);
        assert_eq!(a.list[0].hz, 7_040_000.0); // newest first
    }

    #[test]
    fn no_duplicate_for_same_call_and_frequency() {
        let mut a = Alerts::default();
        a.set_watch_list("W1AW, K1ABC");
        let t = Instant::now();
        a.check("CQ DE W1AW W1AW K", 7_025_000.0, t);
        a.check("W1AW TEST", 7_025_080.0, t); // same signal, measured slightly off
        assert_eq!(a.list.len(), 1);
        a.check("W1AW TEST", 7_026_000.0, t); // a different frequency is a new alert
        a.check("K1ABC 5NN", 7_025_000.0, t); // a different call is a new alert
        assert_eq!(a.list.len(), 3);
    }

    #[test]
    fn alerts_expire_after_five_minutes() {
        let mut a = Alerts::default();
        a.set_watch_list("W1AW");
        let t = Instant::now();
        a.check("W1AW", 7_025_000.0, t);
        a.expire(t + ALERT_LIFETIME - Duration::from_secs(1));
        assert_eq!(a.list.len(), 1);
        a.expire(t + ALERT_LIFETIME);
        assert!(a.list.is_empty());
        // Once expired, hearing the call again raises a fresh alert.
        a.check("W1AW", 7_025_000.0, t + ALERT_LIFETIME);
        assert_eq!(a.list.len(), 1);
    }
}
