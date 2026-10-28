//! Heuristic callsign extraction from decoded text, for the spot labels.

const NOT_CALLS: &[&str] = &["5NN", "ENN", "599", "579", "589", "73", "88", "TU", "QRL"];

pub fn is_callsign(tok: &str) -> bool {
    let base = tok.split('/').max_by_key(|p| p.len()).unwrap_or(tok);
    if !(3..=8).contains(&base.len()) || NOT_CALLS.contains(&base) || base.starts_with("5NN") {
        return false;
    }
    let b = base.as_bytes();
    if !b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return false;
    }
    // prefix (1-2 letters, letter+digit, or digit+letter), area digit, 1-4 letter suffix
    let suffix = b.iter().rev().take_while(|c| c.is_ascii_uppercase()).count();
    if !(1..=4).contains(&suffix) || b.len() < suffix + 2 {
        return false;
    }
    let head = &b[..b.len() - suffix];
    let (prefix, digit) = head.split_at(head.len() - 1);
    digit[0].is_ascii_digit()
        && match prefix {
            [a] => a.is_ascii_uppercase(),
            [a, b] => a.is_ascii_uppercase() || b.is_ascii_uppercase(),
            _ => false,
        }
}

pub struct Spot {
    pub call: String,
    pub cq: bool,
}

/// Find the most relevant callsign in recent decoded text.
pub fn spot(text: &str) -> Option<Spot> {
    let toks: Vec<&str> = text.split_whitespace().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
    let mut best: Option<(usize, &str, u32)> = None;
    for (i, t) in toks.iter().enumerate() {
        if !is_callsign(t) {
            continue;
        }
        // Prefer calls introduced by CQ/DE/TEST and calls that repeat.
        let mut score = toks.iter().filter(|x| *x == t).count() as u32;
        if i > 0 && matches!(toks[i - 1], "DE" | "CQ" | "TEST") {
            score += 3;
        }
        if toks.get(i + 1).is_some_and(|n| matches!(*n, "TEST" | "K" | "CQ" | "DE")) {
            score += 3;
        }
        if best.is_none_or(|(_, _, s)| score >= s) {
            best = Some((i, t, score));
        }
    }
    let (i, call, score) = best?;
    if score < 2 {
        // A single hit with no CQ/DE/TEST context is too often decoding noise.
        return None;
    }
    let lo = i.saturating_sub(8);
    let cq = toks[lo..i].iter().any(|t| matches!(*t, "CQ" | "TEST"));
    Some(Spot { call: call.to_string(), cq })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_calls() {
        assert!(is_callsign("EA3AKY"));
        assert!(is_callsign("9A1A"));
        assert!(is_callsign("W1AW/P"));
        assert!(!is_callsign("5NN"));
        assert!(!is_callsign("TEST"));
        assert!(!is_callsign("599"));
        let s = spot("TU EA3AKY TEST EA3AKY").unwrap();
        assert_eq!(s.call, "EA3AKY");
        assert!(s.cq);
        let s = spot("R TU 5NN 14 DE W2UP").unwrap();
        assert_eq!(s.call, "W2UP");
        assert!(!s.cq);
        assert!(spot("HEESIDIE IS5S").is_none());
        assert!(!is_callsign("ESES5EH"));
        assert!(is_callsign("HB9GCH") && is_callsign("S5CP") && is_callsign("OH0ZMY"));
    }
}
