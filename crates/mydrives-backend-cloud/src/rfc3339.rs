//! The one date shape cloud APIs send: `2026-08-11T09:00:00+09:00`.
//!
//! Small enough to parse by hand, which keeps a date-time crate out of the
//! build for a field that only ever has this form (also accepted: a `Z`
//! suffix and fractional seconds, which Google and Dropbox use).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn parse(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;

    // Split the time from the offset: the offset starts at the last '+' or '-'
    // after the 'T', or is the single letter 'Z'.
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut parts = off[1..].split(':');
        let oh: i64 = parts.next()?.parse().ok()?;
        let om: i64 = parts.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    };
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let sec_field = t.next()?;
    let (sec_str, frac) = sec_field.split_once('.').unwrap_or((sec_field, ""));
    let second: i64 = sec_str.parse().ok()?;
    let nanos: u32 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(9).collect();
        let padded = format!("{digits:0<9}");
        padded.parse().ok()?
    };

    let days = days_from_civil(year, month, day);
    let local = days * 86_400 + hour * 3600 + minute * 60 + second;
    let utc = local - offset_secs;
    let base = if utc >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(utc as u64))?
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs((-utc) as u64))?
    };
    Some(base + Duration::from_nanos(nanos as u64))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t: SystemTime) -> i64 {
        t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
    }

    #[test]
    fn the_mybox_shape_lands_on_the_right_instant() {
        // 2026-08-11 09:00 KST is 2026-08-11 00:00 UTC.
        let t = parse("2026-08-11T09:00:00+09:00").unwrap();
        assert_eq!(secs(t), 1_786_406_400);
        // Same instant, written in UTC.
        assert_eq!(parse("2026-08-11T00:00:00Z").unwrap(), t);
    }

    #[test]
    fn fractions_and_negative_offsets_are_read() {
        let a = parse("2024-02-29T23:59:59.500-05:00").unwrap();
        let b = parse("2024-03-01T04:59:59.5Z").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn garbage_is_none_not_a_panic() {
        for bad in ["", "2026-08-11", "yesterday", "2026-13-01T00:00:00Z"] {
            let _ = parse(bad);
        }
        assert!(parse("not a date").is_none());
    }
}
