//! Directory listings: MLSD first, `LIST` only as a fallback.
//!
//! `MLSD` (RFC 3659) is the only listing an FTP client can parse without
//! guessing: every line is `fact=value;...; name`, the facts are named, and the
//! timestamp is UTC in a fixed format. `LIST` is whatever the server's `ls`
//! prints - locale month names, a year that turns into a clock after six
//! months, and no way to tell a file called `total 8` from the header. So MLSD
//! is used when the server has it (nearly all do today) and `LIST` output is
//! handed to the parser in `suppaftp::list`, which handles the common UNIX and
//! DOS shapes, only when it does not.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mydrives_core::types::{Attr, DirEntry, FileKind};

/// What a single MLSD line turned out to be.
pub enum Parsed {
    Entry(DirEntry),
    /// `.` and `..` - MLSD lists them with `type=cdir`/`type=pdir`.
    SelfOrParent,
    Unusable,
}

/// Parses one MLSD line: `type=file;size=11;modify=20260101T...; name`.
pub fn parse_mlsd_line(line: &str) -> Parsed {
    // The name is everything after the first space that follows the facts, and
    // it may itself contain spaces and semicolons.
    let Some((facts, name)) = line.split_once(' ') else {
        return Parsed::Unusable;
    };
    let name = name.trim_end_matches(['\r', '\n']);
    if name.is_empty() {
        return Parsed::Unusable;
    }

    let mut kind = None;
    let mut size = 0u64;
    let mut modify = None;
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => match value.to_ascii_lowercase().as_str() {
                "file" => kind = Some(FileKind::File),
                "dir" => kind = Some(FileKind::Dir),
                "cdir" | "pdir" => return Parsed::SelfOrParent,
                // OS/2 and link types: nothing a mount can do with them.
                _ => return Parsed::Unusable,
            },
            "size" | "sizd" => size = value.parse().unwrap_or(0),
            "modify" => modify = parse_timeval(value),
            _ => {}
        }
    }

    let Some(kind) = kind else {
        return Parsed::Unusable;
    };
    let mtime = modify.unwrap_or(UNIX_EPOCH);
    Parsed::Entry(DirEntry {
        name: name.to_string(),
        attr: Attr::new(kind, if kind == FileKind::Dir { 0 } else { size }, mtime),
    })
}

/// RFC 3659 `time-val`: `YYYYMMDDHHMMSS[.sss]`, always UTC.
pub fn parse_timeval(value: &str) -> Option<SystemTime> {
    let digits = value.split('.').next()?;
    if digits.len() < 14 || !digits.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    let num = |from: usize, to: usize| digits[from..to].parse::<i64>().ok();
    let secs = days_from_civil(num(0, 4)?, num(4, 6)?, num(6, 8)?) * 86_400
        + num(8, 10)? * 3600
        + num(10, 12)? * 60
        + num(12, 14)?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// The same `time-val`, for sending to the server in `MFMT`.
pub fn format_timeval(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    Some(format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    ))
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse, so a timestamp can be written back out.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `LIST` fallback, via suppaftp's own `ls`/DOS parser.
pub fn parse_list_line(line: &str) -> Option<DirEntry> {
    use std::str::FromStr;
    let file = suppaftp::list::File::from_str(line).ok()?;
    let name = file.name().to_string();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    // Symlinks are followed by the server on access; listing them as files is
    // the best a mount can do without a stat of the target.
    let kind = if file.is_directory() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    let size = if file.is_directory() {
        0
    } else {
        file.size() as u64
    };
    Some(DirEntry {
        name,
        attr: Attr::new(kind, size, file.modified()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mlsd_file_line_carries_size_and_time() {
        let line = "type=file;size=11;modify=20260110123456;perm=adfrw; hello.txt";
        let Parsed::Entry(e) = parse_mlsd_line(line) else {
            panic!("expected an entry");
        };
        assert_eq!(e.name, "hello.txt");
        assert_eq!(e.attr.kind, FileKind::File);
        assert_eq!(e.attr.size, 11);
        assert_eq!(
            e.attr.mtime.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_768_048_496
        );
    }

    #[test]
    fn mlsd_directories_report_no_size() {
        let line = "type=dir;sizd=4096;modify=20260110123456; 사진 모음";
        let Parsed::Entry(e) = parse_mlsd_line(line) else {
            panic!("expected an entry");
        };
        assert_eq!(e.name, "사진 모음");
        assert_eq!(e.attr.kind, FileKind::Dir);
        assert_eq!(e.attr.size, 0);
    }

    #[test]
    fn mlsd_skips_the_directory_itself_and_its_parent() {
        assert!(matches!(
            parse_mlsd_line("type=cdir;modify=20260110123456; /pub"),
            Parsed::SelfOrParent
        ));
        assert!(matches!(
            parse_mlsd_line("type=pdir;modify=20260110123456; .."),
            Parsed::SelfOrParent
        ));
    }

    #[test]
    fn a_name_may_contain_spaces_and_semicolons() {
        let Parsed::Entry(e) = parse_mlsd_line("type=file;size=1; a; b c.txt") else {
            panic!("expected an entry");
        };
        assert_eq!(e.name, "a; b c.txt");
    }

    #[test]
    fn timestamps_round_trip_through_the_wire_format() {
        let stamp = "20260110123456";
        let t = parse_timeval(stamp).expect("parses");
        assert_eq!(format_timeval(t).as_deref(), Some(stamp));
    }

    #[test]
    fn a_line_without_a_type_fact_is_not_usable() {
        assert!(matches!(
            parse_mlsd_line("size=11;modify=20260110123456; hello.txt"),
            Parsed::Unusable
        ));
    }

    #[test]
    fn unix_list_output_is_the_fallback() {
        let e = parse_list_line("-rw-r--r-- 1 owner group          11 Jan 10 12:34 hello.txt")
            .expect("parses");
        assert_eq!(e.name, "hello.txt");
        assert_eq!(e.attr.kind, FileKind::File);
        assert_eq!(e.attr.size, 11);

        let d = parse_list_line("drwxr-xr-x 2 owner group        4096 Jan 10 12:34 photos")
            .expect("parses");
        assert_eq!(d.attr.kind, FileKind::Dir);
    }
}
