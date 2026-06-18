//! Odds and ends shared across the crate: a self-healing error type and some
//! UTC-time helpers we roll by hand to avoid pulling in a date crate.

use std::fmt;
use std::path::Path;

/// An error with a human-readable message and, optionally, a short copy-pasteable
/// `hint`. When something fails the CLI prints that hint, so the caller (often an
/// LLM) can fix the call in one go (see desc.md §4 "Ошибки самовосстанавливаются").
#[derive(Debug)]
pub struct AppError {
    pub msg: String,
    pub hint: Option<String>,
}

impl AppError {
    pub fn new(msg: impl Into<String>) -> Self {
        AppError {
            msg: msg.into(),
            hint: None,
        }
    }
    pub fn with_hint(msg: impl Into<String>, hint: impl Into<String>) -> Self {
        AppError {
            msg: msg.into(),
            hint: Some(hint.into()),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for AppError {}

// These From impls let command code just sprinkle `?` everywhere.
impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::new(format!("storage error: {e}"))
    }
}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::new(format!("io error: {e}"))
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::new(format!("json error: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

/// "Now" as (epoch seconds, ISO-8601 UTC string).
pub fn now() -> (i64, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (secs, iso_utc(secs))
}

/// Turn epoch seconds into `YYYY-MM-DDTHH:MM:SSZ` (UTC) without any date crate.
/// The calendar math is Howard Hinnant's civil-from-days algorithm.
pub fn iso_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 back to (year, month, day), proleptic Gregorian.
fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

/// 64-bit FNV-1a over arbitrary bytes. We use it as a cheap content fingerprint
/// for the `reindex` sync table (has this file changed since last time?). It's
/// not cryptographic, but for old-vs-new bytes of a single file collisions just
/// don't happen in practice.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `content_hash` as a 16-char lowercase hex string (stored in the sync table).
pub fn content_hash_hex(bytes: &[u8]) -> String {
    format!("{:016x}", content_hash(bytes))
}

/// File mtime as epoch seconds (0 if we can't read it). It's only a cheap hint
/// for `reindex` to skip work; the content hash is what actually decides.
pub fn mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A short, single-line preview of a note body for `list`-style output.
pub fn preview(text: &str, max_chars: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        one_line
    } else {
        let mut s: String = one_line.chars().take(max_chars).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_utc_epoch_zero() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso_utc_known_timestamps_and_leap() {
        assert_eq!(iso_utc(86_399), "1970-01-01T23:59:59Z");
        // Leap day 2000-02-29 (2000 is a leap year).
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
        // A round epoch value people tend to recognize.
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn iso_utc_negative_epoch_before_1970() {
        // div_euclid / rem_euclid keep the date one second before the epoch.
        assert_eq!(iso_utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn preview_no_truncation_when_short() {
        // Whitespace collapses; nothing is cut, no ellipsis.
        assert_eq!(preview("hello   world", 100), "hello world");
        assert!(!preview("hello world", 100).contains('…'));
    }

    #[test]
    fn preview_truncates_by_chars_with_ellipsis() {
        assert_eq!(preview("abcdef", 3), "abc…");
    }

    #[test]
    fn preview_unicode_does_not_split_codepoint() {
        let out = preview("приветмир", 6);
        assert_eq!(out, "привет…");
        // Still valid UTF-8: we take whole chars, never cut one in half.
        assert_eq!(out.chars().count(), 7);
    }

    #[test]
    fn preview_collapses_all_whitespace_to_one_line() {
        assert_eq!(preview("a\n\nb  c\td", 100), "a b c d");
        assert_eq!(preview("   \n  \t ", 100), "");
    }

    #[test]
    fn preview_zero_max_chars_boundary() {
        assert_eq!(preview("x", 0), "…");
        assert_eq!(preview("", 0), "");
    }

    #[test]
    fn apperror_display_shows_only_msg() {
        let e = AppError::with_hint("the message", "the hint");
        assert_eq!(format!("{e}"), "the message");
    }

    #[test]
    fn apperror_new_has_no_hint() {
        let e = AppError::new("m");
        assert_eq!(e.msg, "m");
        assert!(e.hint.is_none());
    }

    #[test]
    fn apperror_with_hint_carries_both() {
        let e = AppError::with_hint("m", "h");
        assert_eq!(e.msg, "m");
        assert_eq!(e.hint.as_deref(), Some("h"));
    }

    #[test]
    fn content_hash_deterministic_and_sensitive() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hellp")); // one byte differs
        assert_eq!(content_hash(b""), 0xcbf2_9ce4_8422_2325); // FNV-1a offset basis
                                                              // Hex form is 16 lowercase hex chars.
        let h = content_hash_hex(b"abc");
        assert_eq!(h.len(), 16);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(h, format!("{:016x}", content_hash(b"abc")));
    }

    #[test]
    fn apperror_from_io_prefixes_and_no_hint() {
        let io = std::io::Error::other("boom");
        let e: AppError = io.into();
        assert!(e.msg.starts_with("io error:"));
        assert!(e.msg.contains("boom"));
        assert!(e.hint.is_none());
    }
}
