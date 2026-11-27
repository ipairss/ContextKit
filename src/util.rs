//! Small helpers for time, paths and text that do not justify a dependency.

use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Current time as milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    system_time_ms(SystemTime::now())
}

/// Converts a [`SystemTime`] to milliseconds since the Unix epoch, saturating on overflow.
pub fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Converts a [`SystemTime`] to nanoseconds since the Unix epoch, saturating on overflow.
pub fn system_time_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// Formats Unix milliseconds as an RFC 3339 UTC timestamp, e.g. `2026-01-31T09:15:00Z`.
pub fn format_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Converts a path relative to the project root into the canonical stored form:
/// forward slashes, no `.` components. Returns `None` for paths that are absolute,
/// escape the root, or are not valid UTF-8.
pub fn normalize_rel_path(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(p) => parts.push(p.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Validates a user- or agent-supplied project-relative path and returns its canonical form.
pub fn validate_rel_path(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let candidate = trimmed.replace('\\', "/");
    normalize_rel_path(Path::new(&candidate)).ok_or_else(|| {
        Error::InvalidInput(format!(
            "path `{trimmed}` must be relative to the project root and must not contain `..`"
        ))
    })
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Collapses runs of whitespace into single spaces and trims the result.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Escapes `%`, `_` and the escape character itself for use in `LIKE ... ESCAPE '\'`.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_and_leap_day() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_767_225_599_000), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn normalizes_relative_paths() {
        assert_eq!(
            validate_rel_path("src\\a\\b.rs").ok().as_deref(),
            Some("src/a/b.rs")
        );
        assert_eq!(
            validate_rel_path("./src/a.rs").ok().as_deref(),
            Some("src/a.rs")
        );
        assert!(validate_rel_path("../etc/passwd").is_err());
        assert!(validate_rel_path("/etc/passwd").is_err());
        assert!(validate_rel_path("").is_err());
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_on_char_boundary("héllo", 2), "h");
        assert_eq!(truncate_on_char_boundary("abc", 10), "abc");
    }

    #[test]
    fn escapes_like_wildcards() {
        assert_eq!(escape_like("a_b%c\\"), "a\\_b\\%c\\\\");
    }
}
