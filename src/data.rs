use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Claude,
    Codex,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Claude => "Claude",
            Backend::Codex => "Codex",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub backend: Backend,
    /// Id passed to `claude --resume` / `codex resume` (filename stem or in-file id).
    pub id: String,
    pub title: String,
    pub cwd: String,
    /// The "space": the folder the cwd lives under (e.g. `proj`, `toolbox`).
    pub space: String,
    /// Epoch seconds of last activity; drives sort order and the "when" column.
    pub last_activity: i64,
    pub messages: Vec<Message>,
    pub msg_count: usize,
    /// Pre-lowercased haystack for substring search.
    pub search_blob: String,
}

impl Session {
    // A `bool` return keeps the door open for swapping in `nucleo` fuzzy ranking later.
    pub fn matches(&self, needle_lower: &str) -> bool {
        needle_lower.is_empty() || self.search_blob.contains(needle_lower)
    }
}

/// The space a working directory belongs to: the component right under `Desktop`
/// if present, otherwise the directory's own name.
pub fn space_of(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(i) = parts.iter().position(|p| *p == "Desktop")
        && let Some(space) = parts.get(i + 1)
    {
        return (*space).to_string();
    }
    parts
        .last()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "~".to_string())
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an RFC-3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS...`) to epoch seconds.
/// Returns `None` on anything unrecognized so callers can fall back to mtime.
pub fn parse_iso_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    if b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn humanize_since(then: i64, now: i64) -> String {
    if then <= 0 {
        return "?".to_string();
    }
    let secs = (now - then).max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_epoch_known_value() {
        assert_eq!(
            parse_iso_epoch("2026-06-30T19:20:35.301Z"),
            Some(1_782_847_235)
        );
    }

    #[test]
    fn iso_epoch_unix_epoch() {
        assert_eq!(parse_iso_epoch("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn iso_epoch_rejects_garbage() {
        assert_eq!(parse_iso_epoch("not-a-date"), None);
        assert_eq!(parse_iso_epoch(""), None);
        assert_eq!(parse_iso_epoch("2026/06/30"), None);
    }

    #[test]
    fn iso_strings_sort_chronologically() {
        let mut v = [
            "2026-06-30T19:20:35.301Z",
            "2024-01-01T00:00:00.000Z",
            "2026-06-30T19:20:36.000Z",
        ];
        v.sort();
        assert_eq!(v[0], "2024-01-01T00:00:00.000Z");
        assert_eq!(v[2], "2026-06-30T19:20:36.000Z");
    }

    #[test]
    fn humanize_buckets() {
        assert_eq!(humanize_since(1000, 1030), "just now");
        assert_eq!(humanize_since(1000, 1000 + 120), "2m ago");
        assert_eq!(humanize_since(1000, 1000 + 7200), "2h ago");
        assert_eq!(humanize_since(1000, 1000 + 2 * 86_400), "2d ago");
    }

    #[test]
    fn space_from_desktop_layout() {
        assert_eq!(space_of("/Users/u/Desktop/proj/web"), "proj");
        assert_eq!(space_of("/Users/u/Desktop/tools/space"), "tools");
        assert_eq!(space_of("/Users/u/Desktop/loose-repo"), "loose-repo");
        assert_eq!(space_of("/tmp/whatever/proj"), "proj");
    }
}
