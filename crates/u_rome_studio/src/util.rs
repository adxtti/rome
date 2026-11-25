//! Small formatting helpers with no UI dependencies.

use std::time::{SystemTime, UNIX_EPOCH};

/// Broken-down UTC time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UtcTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

impl UtcTime {
    pub fn now() -> Self {
        Self::from_system(SystemTime::now())
    }

    pub fn from_system(t: SystemTime) -> Self {
        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self::from_unix_millis(d.as_millis().min(i64::MAX as u128) as i64)
    }

    /// Converts milliseconds since the Unix epoch (days-from-civil inverse,
    /// after H. Hinnant's public-domain `civil_from_days`).
    pub fn from_unix_millis(ms: i64) -> Self {
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000) as u32;
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year,
            month,
            day,
            hour: (sod / 3600) as u32,
            minute: (sod % 3600 / 60) as u32,
            second: (sod % 60) as u32,
            millis,
        }
    }

    /// `2026-11-24 17:22:05Z`
    pub fn iso(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `20261124-172205`, safe for file names.
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `17:22:05.123`
    pub fn clock(&self) -> String {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            self.hour, self.minute, self.second, self.millis
        )
    }
}

/// Human-readable byte size using binary units (`1.50 MiB`).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Number with thin thousands separators (`123,456`).
pub fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Truncates `s` to at most `max` chars, appending an ellipsis marker.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

/// Deterministic 64-bit mixer (SplitMix64 finaliser). Used by the mock
/// backend and tests; not cryptographic.
pub const fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(UtcTime::from_unix_millis(0).iso(), "1970-01-01 00:00:00Z");
        // 2024-02-29 12:34:56.789 UTC (leap day)
        let t = UtcTime::from_unix_millis(1_709_210_096_789);
        assert_eq!(t.iso(), "2024-02-29 12:34:56Z");
        assert_eq!(t.clock(), "12:34:56.789");
        assert_eq!(t.file_stamp(), "20240229-123456");
        // 2000-03-01 00:00:00
        assert_eq!(
            UtcTime::from_unix_millis(951_868_800_000).iso(),
            "2000-03-01 00:00:00Z"
        );
        // Negative input does not panic.
        assert_eq!(UtcTime::from_unix_millis(-1).year, 1969);
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.50 KiB");
        assert_eq!(format_size(160 * 1024 * 1024), "160 MiB");
        assert_eq!(format_size(u64::MAX), "16384 PiB");
    }

    #[test]
    fn counts_and_truncate() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdefghijkl", 8), "abcde...");
    }
}
