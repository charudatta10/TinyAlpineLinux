//! Small shared helpers for the applets.

use std::io;

/// Print an `applet: message` style error to stderr and return exit code 1.
pub fn error(applet: &str, msg: &str) -> i32 {
    eprintln!("{applet}: {msg}");
    1
}

/// Print an io error with context: `applet: path: reason`
pub fn ioerr(applet: &str, path: &str, e: &io::Error) -> i32 {
    eprintln!("{applet}: {path}: {}", ioerrmsg(e));
    1
}

pub fn ioerrmsg(e: &io::Error) -> String {
    e.to_string()
}

/// Parse an unsigned integer argument, reporting a friendly error.
pub fn parse_usize(applet: &str, opt: &str, val: &str) -> Result<usize, i32> {
    match val.replace('_', "").parse::<usize>() {
        Ok(n) => Ok(n),
        Err(_) => Err(error(
            applet,
            &format!("invalid number for {opt}: '{val}'"),
        )),
    }
}

/// Render a byte count the way `ls -h` does (1.5K, 2.3M, ...).
pub fn human_size(n: u64) -> String {
    const UNITS: [char; 4] = ['K', 'M', 'G', 'T'];
    if n < 1024 {
        return format!("{n}");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{v:.1}{}", UNITS[i])
    } else {
        format!("{v:.0}{}", UNITS[i])
    }
}

/// Expand escape sequences used by `echo -e` / `printf`.
/// Supported: \\ \a \b \c(stop) \e \f \n \r \t \v \0NNN \xHH \\NNN(octal)
/// Returns the expanded text and whether a `\c` terminator was hit.
pub fn expand_escapes(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut stopped = false;
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some('e') => out.push('\u{1b}'),
            Some('c') => {
                stopped = true;
                break;
            }
            Some('0') => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 {
                    match chars.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            chars.next();
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(v).unwrap_or('\0'));
            }
            Some('x') => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 {
                    match chars.peek().and_then(|d| d.to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            chars.next();
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(v).unwrap_or('\0'));
            }
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    (out, stopped)
}
