//! Portable civil-time helpers: epoch seconds <-> broken-down local time.
//!
//! No external time crate: dates are computed with Howard Hinnant's
//! `days_from_civil` / `civil_from_days` algorithms, and the local UTC offset
//! comes from the platform (libc `localtime_r` on Unix, `GetLocalTime` on
//! Windows).

/// Broken-down calendar time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct T {
    pub year: i64,  // e.g. 2026
    pub month: u32, // 1..=12
    pub day: u32,   // 1..=31
    pub hour: u32,  // 0..=23
    pub min: u32,   // 0..=59
    pub sec: u32,   // 0..=60 (leap second never materialises)
    pub wday: u32,  // 0 = Sunday
    pub yday: u32,  // 0 = January 1
    pub offset: i64, // seconds east of UTC (local offset actually applied)
}

#[allow(dead_code)]
pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_from_civil(y0: i64, m0: i64, d0: i64) -> i64 {
    let mut y = y0;
    let m = m0;
    let d = d0;
    if m <= 2 {
        y -= 1;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

fn utc_parts(ts: i64) -> T {
    let days = ts.div_euclid(86400);
    let rem = ts.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;
    // 1970-01-01 was a Thursday (4).
    let wday = ((days + 4).rem_euclid(7)) as u32;
    let yday = (days - days_from_civil(year, 1, 1)) as u32;
    T {
        year,
        month,
        day,
        hour,
        min,
        sec,
        wday,
        yday,
        offset: 0,
    }
}

/// Current epoch seconds.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local UTC offset in seconds for a given epoch timestamp (best effort:
/// uses the current offset, so historical DST rules are not applied).
pub fn local_offset(_ts: i64) -> i64 {
    #[cfg(unix)]
    {
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t: libc::time_t = _ts as libc::time_t;
            if libc::localtime_r(&t, &mut tm).is_null() {
                0
            } else {
                tm.tm_gmtoff as i64
            }
        }
    }
    #[cfg(windows)]
    {
        windows_offset()
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

#[cfg(windows)]
fn windows_offset() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        unsafe {
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            let as_utc = days_from_civil(st.wYear as i64, st.wMonth as i64, st.wDay as i64) * 86400
                + st.wHour as i64 * 3600
                + st.wMinute as i64 * 60
                + st.wSecond as i64;
            as_utc - now_epoch()
        }
    })
}

/// Broken-down local time for an epoch timestamp.
pub fn local_parts(ts: i64) -> T {
    let off = local_offset(ts);
    let mut t = utc_parts(ts + off);
    t.offset = off;
    t
}

/// Broken-down UTC time for an epoch timestamp.
pub fn utc(ts: i64) -> T {
    utc_parts(ts)
}

pub const ABBR_DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const FULL_DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
pub const ABBR_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub const FULL_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `ctime`-style: `Thu Oct  9 14:45:03 2026`
#[allow(dead_code)]
pub fn fmt_ctime(t: &T) -> String {
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} {}",
        ABBR_DAYS[t.wday as usize],
        ABBR_MONTHS[(t.month - 1) as usize],
        t.day,
        t.hour,
        t.min,
        t.sec,
        t.year
    )
}

/// `ls -l` style: recent files show `Oct  9 14:45`, old ones `Oct  9  2024`.
pub fn fmt_ls(ts: i64, now: i64) -> String {
    let t = local_parts(ts);
    let mon = ABBR_MONTHS[(t.month - 1) as usize];
    let recent = (now - ts).abs() < 6 * 30 * 86400;
    if recent {
        format!("{mon} {:>2} {:02}:{:02}", t.day, t.hour, t.min)
    } else {
        format!("{mon} {:>2}  {}", t.day, t.year)
    }
}

/// POSIX `date` format specifiers (`strftime` subset).
pub fn strftime(fmt: &str, t: &T) -> String {
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&t.year.to_string()),
            Some('y') => out.push_str(&format!("{:02}", t.year % 100)),
            Some('m') => out.push_str(&format!("{:02}", t.month)),
            Some('d') => out.push_str(&format!("{:02}", t.day)),
            Some('e') => out.push_str(&format!("{:>2}", t.day)),
            Some('H') => out.push_str(&format!("{:02}", t.hour)),
            Some('I') => {
                let h12 = match t.hour % 12 {
                    0 => 12,
                    h => h,
                };
                out.push_str(&format!("{h12:02}"));
            }
            Some('M') => out.push_str(&format!("{:02}", t.min)),
            Some('S') => out.push_str(&format!("{:02}", t.sec)),
            Some('j') => out.push_str(&format!("{:03}", t.yday + 1)),
            Some('p') => out.push_str(if t.hour < 12 { "AM" } else { "PM" }),
            Some('a') => out.push_str(ABBR_DAYS[t.wday as usize]),
            Some('A') => out.push_str(FULL_DAYS[t.wday as usize]),
            Some('b') | Some('h') => out.push_str(ABBR_MONTHS[(t.month - 1) as usize]),
            Some('B') => out.push_str(FULL_MONTHS[(t.month - 1) as usize]),
            Some('s') => out.push_str(&local_to_epoch(t).to_string()),
            Some('w') => out.push_str(&t.wday.to_string()),
            Some('u') => {
                let u = if t.wday == 0 { 7 } else { t.wday };
                out.push_str(&u.to_string());
            }
            Some('T') => out.push_str(&format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec)),
            Some('F') => out.push_str(&format!("{}-{:02}-{:02}", t.year, t.month, t.day)),
            Some('D') => out.push_str(&format!("{:02}/{:02}/{:02}", t.month, t.day, t.year % 100)),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Inverse of `local_parts`: broken-down local time -> epoch seconds.
pub fn local_to_epoch(t: &T) -> i64 {
    days_from_civil(t.year, t.month as i64, t.day as i64) * 86400
        + t.hour as i64 * 3600
        + t.min as i64 * 60
        + t.sec as i64
        - t.offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_thursday() {
        let t = utc(0);
        assert_eq!((t.year, t.month, t.day), (1970, 1, 1));
        assert_eq!(t.wday, 4);
        assert_eq!(t.yday, 0);
        assert_eq!((t.hour, t.min, t.sec), (0, 0, 0));
    }

    #[test]
    fn roundtrip_across_centuries() {
        for ts in [-2208988800i64, -1, 0, 1, 951782400, 1767225600, 4102444800] {
            let t = utc(ts);
            let back = days_from_civil(t.year, t.month as i64, t.day as i64) * 86400
                + t.hour as i64 * 3600
                + t.min as i64 * 60
                + t.sec as i64;
            assert_eq!(back, ts, "ts={ts}");
        }
    }

    #[test]
    fn leap_days() {
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        assert!(is_leap(2000) && !is_leap(1900));
    }

    #[test]
    fn strftime_basics() {
        let t = utc(1760000000); // 2025-10-09 08:53:20 UTC
        assert_eq!(strftime("%F %T", &t), "2025-10-09 08:53:20");
        assert_eq!(strftime("%Y", &t), "2025");
        assert_eq!(strftime("100%%", &t), "100%");
        assert_eq!(strftime("%a %b", &t), "Thu Oct");
    }
}
