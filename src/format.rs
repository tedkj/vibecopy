//! Numbers, sizes, speeds and times as the window says them. Every number is the measured one:
//! counts are exact (grouped with a no-break space, "3 412"), sizes are VibeFTP's (`812 B`,
//! `2.1 MB`), times are local.

/// Thousands grouped by a no-break space: "3 412", "1 204 000".
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    out
}

/// `812 B`, `14 KB`, `2.1 MB`, `140 GB`: one decimal below 10, whole numbers above.
pub fn bytes(n: u64) -> String {
    let n = n as f64;
    if n < 1023.5 {
        return format!("{} B", n.round() as u64);
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    let mut v = n / 1024.0;
    let mut i = 0;
    while v >= 1023.5 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 9.95 { format!("{v:.1} {}", UNITS[i]) } else { format!("{} {}", v.round() as u64, UNITS[i]) }
}

/// "1 file", "3 412 files".
pub fn files(n: u64) -> String {
    plural(n, "file", "files")
}

/// "1 folder", "3 folders".
pub fn folders(n: u64) -> String {
    plural(n, "folder", "folders")
}

pub fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{} {many}", count(n)) }
}

/// Bytes per second as a speed: "84 MB/s", "3.2 GB/s", "512 KB/s".
pub fn speed(bps: f64) -> String {
    if !bps.is_finite() || bps < 1.0 {
        return "0 B/s".into();
    }
    format!("{}/s", bytes(bps.round() as u64))
}

/// Time left in words, as rough as it is honest: "a few seconds", "40 s", "3 min", "1 h 20 min".
pub fn eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return String::new();
    }
    let s = secs.round() as u64;
    if s < 5 {
        "a few seconds".into()
    } else if s < 60 {
        format!("{} s", s.div_ceil(5) * 5)
    } else if s < 3600 {
        format!("{} min", s.div_ceil(60))
    } else {
        let m = s.div_ceil(60);
        if m.is_multiple_of(60) { format!("{} h", m / 60) } else { format!("{} h {} min", m / 60, m % 60) }
    }
}

/// Cut from the middle to `max` characters: "C:\Users\ted\…\Photos\2026". Paths keep their
/// beginning (the drive) and their end (the folder that matters).
pub fn elide_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[n - tail..]);
    out
}

/// A local date and time from FILETIME ticks: "2026-10-18 13:54" (the time zone's rules for
/// that date, so a summer file reads in summer time).
pub fn stamp(ticks: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ticks == 0 {
        return String::new();
    }
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc) == 0 {
            return String::new();
        }
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            local = utc;
        }
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
}
