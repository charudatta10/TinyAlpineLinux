//! Text and small utility applets: echo, printf, wc, head, tail, sort, cut,
//! tr, grep, which, clear, seq, test.

use crate::helpers::{error, expand_escapes, parse_usize};
use crate::regexlite::Regex;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

// ---------------------------------------------------------------- echo

pub fn echo(args: &[String]) -> i32 {
    let mut newline = true;
    let mut expand = false;
    let mut i = 0;
    // Leading flags only (POSIX echo treats later -n as data).
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') || a == "-" {
            break;
        }
        let body = &a[1..];
        if !body.chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            break;
        }
        for c in body.chars() {
            match c {
                'n' => newline = false,
                'e' => expand = true,
                'E' => expand = false,
                _ => {}
            }
        }
        i += 1;
    }
    let mut out = String::new();
    for (j, a) in args[i..].iter().enumerate() {
        if j > 0 {
            out.push(' ');
        }
        out.push_str(a);
    }
    if expand {
        let (text, _) = expand_escapes(&out);
        out = text;
    }
    if newline {
        out.push('\n');
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
    0
}

// ---------------------------------------------------------------- printf

pub fn printf(args: &[String]) -> i32 {
    if args.is_empty() {
        return error("printf", "missing operand");
    }
    let fmt = &args[0];
    let mut rest: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
    let mut out = Vec::new();
    loop {
        let before = rest.len();
        format_into(&mut out, fmt, &mut rest);
        if rest.is_empty() || rest.len() == before {
            break; // operands satisfied, or the format consumed nothing
        }
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
    0
}

fn format_into(out: &mut Vec<u8>, fmt: &str, args: &mut Vec<&str>) {
    // POSIX printf: backslash escapes in the format operand are expanded.
    let fmt = expand_escapes(fmt).0;
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            push_char(out, chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            push_char(out, '%');
            i += 1;
            continue;
        }
        // flags
        let mut left = false;
        let mut zero = false;
        while i < chars.len() && matches!(chars[i], '-' | '0' | ' ' | '+' | '#') {
            if chars[i] == '-' {
                left = true;
            }
            if chars[i] == '0' {
                zero = true;
            }
            i += 1;
        }
        // width
        let mut width: i64 = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            width = width * 10 + (chars[i] as i64 - '0' as i64);
            i += 1;
        }
        // precision
        let mut prec: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p: usize = 0;
            while i < chars.len() && chars[i].is_ascii_digit() {
                p = p * 10 + (chars[i] as usize - '0' as usize);
                i += 1;
            }
            prec = Some(p);
        }
        // length modifiers (ignored: Rust ints are already wide enough)
        while i < chars.len() && matches!(chars[i], 'l' | 'h' | 'z' | 'j' | 't') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let conv = chars[i];
        i += 1;
        let arg = match args.first_mut() {
            Some(a) => *a,
            None => {
                // Missing arguments behave as empty strings / zeros.
                ""
            }
        };
        let consumed = !args.is_empty();
        if consumed {
            args.remove(0);
        }
        let rendered = match conv {
            's' | 'b' => {
                let mut s = if conv == 'b' {
                    expand_escapes(arg).0
                } else {
                    arg.to_string()
                };
                if let Some(p) = prec {
                    let truncated: String = s.chars().take(p).collect();
                    s = truncated;
                }
                s
            }
            'c' => arg.chars().next().map(|c| c.to_string()).unwrap_or_default(),
            'd' | 'i' => {
                let n: i64 = arg.trim().parse().unwrap_or(0);
                let s = n.to_string();
                pad(&s, width, left, zero && !left)
            }
            'u' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                let s = n.to_string();
                pad(&s, width, left, zero && !left)
            }
            'o' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                pad(&format!("{n:o}"), width, left, zero && !left)
            }
            'x' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                pad(&format!("{n:x}"), width, left, zero && !left)
            }
            'X' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                pad(&format!("{n:X}"), width, left, zero && !left)
            }
            'f' | 'F' => {
                let n: f64 = arg.trim().parse().unwrap_or(0.0);
                let s = match prec {
                    Some(p) => format!("{n:.p$}"),
                    None => format!("{n:.6}"),
                };
                pad(&s, width, left, zero && !left)
            }
            'n' => continue, // %n: no-op (we never write back)
            other => {
                push_char(out, '%');
                push_char(out, other);
                continue;
            }
        };
        // Padding already applied for numeric conversions.
        if matches!(conv, 's' | 'c') {
            let padded = pad(&rendered, width, left, false);
            push_str(out, &padded);
        } else {
            push_str(out, &rendered);
        }
    }
}

fn pad(s: &str, width: i64, left: bool, zero: bool) -> String {
    let len = s.chars().count() as i64;
    if len >= width {
        return s.to_string();
    }
    let fill_n = (width - len) as usize;
    if left {
        format!("{s}{}", " ".repeat(fill_n))
    } else if zero {
        format!("{}{s}", "0".repeat(fill_n))
    } else {
        format!("{}{s}", " ".repeat(fill_n))
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

// ---------------------------------------------------------------- wc

pub fn wc(args: &[String]) -> i32 {
    let mut want_l = false;
    let mut want_w = false;
    let mut want_c = false;
    let mut files: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            for c in a[1..].chars() {
                match c {
                    'l' => want_l = true,
                    'w' => want_w = true,
                    'c' => want_c = true,
                    'm' => want_c = true,
                    other => return error("wc", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            files.push(a.clone());
        }
    }
    if !want_l && !want_w && !want_c {
        want_l = true;
        want_w = true;
        want_c = true;
    }
    let mut code = 0;
    let mut total = (0u64, 0u64, 0u64);
    let mut any = false;
    let inputs: Vec<(String, Vec<u8>)> = if files.is_empty() {
        let mut buf = Vec::new();
        if let Err(e) = io::stdin().read_to_end(&mut buf) {
            return error("wc", &e.to_string());
        }
        vec![("<stdin>".to_string(), buf)]
    } else {
        let mut v = Vec::new();
        for f in &files {
            match std::fs::read(f) {
                Ok(b) => v.push((f.clone(), b)),
                Err(e) => {
                    eprintln!("wc: {f}: {}", e.to_string());
                    code = 1;
                }
            }
        }
        v
    };
    for (name, buf) in inputs {
        let (l, w, c) = count(&buf);
        total.0 += l;
        total.1 += w;
        total.2 += c;
        any = true;
        let mut parts: Vec<String> = Vec::new();
        if want_l {
            parts.push(format!("{l:>7}"));
        }
        if want_w {
            parts.push(format!("{w:>7}"));
        }
        if want_c {
            parts.push(format!("{c:>7}"));
        }
        println!("{} {name}", parts.join(" "));
    }
    if any && inputs_len(&files) > 1 {
        let mut parts: Vec<String> = Vec::new();
        if want_l {
            parts.push(format!("{:>7}", total.0));
        }
        if want_w {
            parts.push(format!("{:>7}", total.1));
        }
        if want_c {
            parts.push(format!("{:>7}", total.2));
        }
        println!("{} total", parts.join(" "));
    }
    code
}

fn inputs_len(files: &[String]) -> usize {
    files.len()
}

fn count(buf: &[u8]) -> (u64, u64, u64) {
    let mut lines = 0u64;
    let mut words = 0u64;
    let mut in_word = false;
    for &b in buf {
        if b == b'\n' {
            lines += 1;
        }
        let is_ws = matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
        if is_ws {
            in_word = false;
        } else if !in_word {
            in_word = true;
            words += 1;
        }
    }
    (lines, words, buf.len() as u64)
}

// ---------------------------------------------------------------- head / tail

fn parse_count(args: &[String], applet: &str, default: usize) -> Result<(usize, Vec<String>), i32> {
    let mut n = default;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-n" || a == "-c" {
            if i + 1 >= args.len() {
                return Err(error(applet, "option requires an argument"));
            }
            let v = args[i + 1].trim_start_matches('+');
            n = parse_usize(applet, "-n", v)?;
            i += 2;
            continue;
        }
        if (a.starts_with("-n") || a.starts_with("-c")) && a.len() > 2 {
            let v = a[2..].trim_start_matches('+');
            if v.chars().all(|c| c.is_ascii_digit()) {
                n = parse_usize(applet, "-n", v)?;
                i += 1;
                continue;
            }
        }
        if a.starts_with('-') && a.len() > 1 && a[1..].chars().all(|c| c.is_ascii_digit()) {
            n = a[1..].parse().unwrap_or(default);
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 && !a.starts_with("--") {
            for c in a[1..].chars() {
                match c {
                    'q' | 'v' => {}
                    other => return Err(error(applet, &format!("invalid option -- '{other}'"))),
                }
            }
            i += 1;
            continue;
        }
        files.push(a.clone());
        i += 1;
    }
    Ok((n, files))
}

fn read_input(file: Option<&str>) -> io::Result<Vec<u8>> {
    match file {
        Some(f) => std::fs::read(f),
        None => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

pub fn head(args: &[String]) -> i32 {
    let (n, files) = match parse_count(args, "head", 10) {
        Ok(v) => v,
        Err(c) => return c,
    };
    let mut code = 0;
    let multi = files.len() > 1;
    if files.is_empty() {
        let stdin = io::stdin();
        let mut taken = 0;
        for line in stdin.lock().lines().map_while(Result::ok) {
            if taken >= n {
                break;
            }
            println!("{line}");
            taken += 1;
        }
        return 0;
    }
    for f in &files {
        if multi {
            println!("==> {f} <==");
        }
        match read_input(Some(f)) {
            Ok(buf) => {
                let mut taken = 0;
                let mut start = 0;
                while taken < n {
                    if start >= buf.len() {
                        break;
                    }
                    let end = buf[start..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map(|p| start + p + 1)
                        .unwrap_or(buf.len());
                    io::stdout().write_all(&buf[start..end]).unwrap_or(());
                    if !buf[start..end].ends_with(b"\n") {
                        println!();
                    }
                    start = end;
                    taken += 1;
                }
            }
            Err(e) => {
                eprintln!("head: {f}: {}", e.to_string());
                code = 1;
            }
        }
        if multi {
            println!();
        }
    }
    code
}

pub fn tail(args: &[String]) -> i32 {
    let mut follow = false;
    let mut cleaned: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-f" || a == "--follow" {
            follow = true;
            i += 1;
            continue;
        }
        if a == "-n" {
            if i + 1 >= args.len() {
                return error("tail", "option requires an argument");
            }
            let mut v = args[i + 1].clone();
            if let Some(stripped) = v.strip_prefix('+') {
                v = stripped.to_string();
            }
            cleaned.push(format!("-n{v}"));
            i += 2;
            continue;
        }
        cleaned.push(a.clone());
        i += 1;
    }
    let (n, files) = match parse_count(&cleaned, "tail", 10) {
        Ok(v) => v,
        Err(c) => return c,
    };
    let target = files.first().map(|s| s.as_str());
    let buf = match read_input(target) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tail: {}: {}", target.unwrap_or("<stdin>"), e.to_string());
            return 1;
        }
    };
    let lines = split_last_lines(&buf, n);
    let mut stdout = io::stdout();
    let _ = stdout.write_all(&lines);
    let _ = stdout.flush();
    if !follow {
        return 0;
    }
    // Follow mode: poll for appended data.
    let mut pos = buf.len();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let cur = match read_input(target) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if cur.len() < pos {
            pos = 0; // truncated
        }
        if cur.len() > pos {
            let _ = stdout.write_all(&cur[pos..]);
            let _ = stdout.flush();
            pos = cur.len();
        }
    }
}

fn split_last_lines(buf: &[u8], n: usize) -> Vec<u8> {
    if n == 0 {
        return Vec::new();
    }
    let mut starts = Vec::with_capacity(n + 1);
    starts.push(0);
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' && i + 1 < buf.len() {
            starts.push(i + 1);
        }
    }
    let keep = starts.len().saturating_sub(n);
    let begin = if starts.len() > n { starts[keep] } else { 0 };
    buf[begin.min(buf.len())..].to_vec()
}

// ---------------------------------------------------------------- sort

pub fn sort(args: &[String]) -> i32 {
    let mut numeric = false;
    let mut reverse = false;
    let mut unique = false;
    let mut fold = false;
    let mut files: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'n' => numeric = true,
                    'r' => reverse = true,
                    'u' => unique = true,
                    'f' => fold = true,
                    'b' | 'g' | 'k' | 'M' | 'R' | 'o' | 'c' => {}
                    other => return error("sort", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            files.push(a.clone());
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let inputs: Vec<String> = if files.is_empty() {
        let mut s = String::new();
        if io::stdin().read_to_string(&mut s).is_err() {
            return error("sort", "failed to read stdin");
        }
        s.lines().map(|l| l.to_string()).collect()
    } else {
        let mut v = Vec::new();
        for f in &files {
            match std::fs::read_to_string(f) {
                Ok(s) => v.extend(s.lines().map(|l| l.to_string())),
                Err(e) => {
                    eprintln!("sort: {f}: {}", e.to_string());
                    return 1;
                }
            }
        }
        v
    };
    lines.extend(inputs);
    let key = |s: &String| if fold { s.to_lowercase() } else { s.clone() };
    if numeric {
        lines.sort_by(|a, b| cmp_numeric(a, b).then_with(|| key(a).cmp(&key(b))));
    } else {
        lines.sort_by(|a, b| key(a).cmp(&key(b)));
    }
    if reverse {
        lines.reverse();
    }
    if unique {
        lines.dedup();
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for l in &lines {
        let _ = writeln!(out, "{l}");
    }
    0
}

fn cmp_numeric(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.trim(), b.trim());
    match (a.parse::<i128>(), b.parse::<i128>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        },
    }
}

// ---------------------------------------------------------------- cut

pub fn cut(args: &[String]) -> i32 {
    let mut delim: Option<char> = None;
    let mut fields: Option<Vec<(usize, usize)>> = None;
    let mut chars: Option<Vec<(usize, usize)>> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        // Option with a separate value: -d VALUE / --delimiter VALUE
        if matches!(a.as_str(), "-d" | "--delimiter" | "-f" | "--fields" | "-c" | "-b" | "--bytes" | "--characters") {
            i += 1;
            let val = match args.get(i) {
                Some(v) => v.clone(),
                None => return error("cut", "option requires an argument"),
            };
            if let Err(msg) = apply_cut_opt(&a, &val, &mut delim, &mut fields, &mut chars) {
                return error("cut", &msg);
            }
            i += 1;
            continue;
        }
        // Attached value: -d: / -f1,3 / --delimiter=:
        if a.starts_with('-') && a.len() > 1 && !a.starts_with("--") {
            let mut cs = a.chars();
            cs.next(); // the '-'
            let flag: String = cs.next().into_iter().collect();
            let val: String = cs.collect();
            if !val.is_empty() {
                if let Err(msg) = apply_cut_opt(&format!("-{flag}"), &val, &mut delim, &mut fields, &mut chars)
                {
                    return error("cut", &msg);
                }
                i += 1;
                continue;
            }
        }
        if let Some((flag, val)) = a.split_once('=') {
            if flag.starts_with("--") {
                if let Err(msg) = apply_cut_opt(flag, val, &mut delim, &mut fields, &mut chars) {
                    return error("cut", &msg);
                }
                i += 1;
                continue;
            }
        }
        if a.starts_with('-') && a.len() > 1 {
            return error("cut", &format!("invalid option -- '{}'", &a[1..]));
        }
        i += 1;
    }
    if fields.is_none() && chars.is_none() {
        return error("cut", "a field (-f) or byte range (-c) is required");
    }
    let delim = delim.unwrap_or('\t');
    let stdin = io::stdin();
    let mut code = 0;
    for line in stdin.lock().lines().map_while(Result::ok) {
        if let Some(ranges) = &chars {
            let mut out = String::new();
            let v: Vec<char> = line.chars().collect();
            for (a, b) in ranges {
                for idx in *a..=*b {
                    if let Some(c) = v.get(idx - 1) {
                        out.push(*c);
                    }
                }
            }
            println!("{out}");
        } else if let Some(ranges) = &fields {
            let parts: Vec<&str> = line.split(delim).collect();
            let mut out: Vec<&str> = Vec::new();
            for (a, b) in ranges {
                for idx in *a..=*b {
                    if let Some(p) = parts.get(idx - 1) {
                        out.push(p);
                    }
                }
            }
            println!("{}", out.join(&delim.to_string()));
        } else {
            code = 1;
        }
    }
    code
}

/// Apply one cut option (already split into flag + value).
fn apply_cut_opt(
    flag: &str,
    val: &str,
    delim: &mut Option<char>,
    fields: &mut Option<Vec<(usize, usize)>>,
    chars: &mut Option<Vec<(usize, usize)>>,
) -> Result<(), String> {
    match flag {
        "-d" | "--delimiter" => {
            let c = val.chars().next().ok_or("delimiter cannot be empty")?;
            *delim = Some(c);
            Ok(())
        }
        "-f" | "--fields" => {
            *fields = Some(parse_ranges(val)?);
            Ok(())
        }
        "-c" | "-b" | "--bytes" | "--characters" => {
            *chars = Some(parse_ranges(val)?);
            Ok(())
        }
        other => Err(format!("invalid option -- '{other}'")),
    }
}

/// Parse `1,3-5,-7` into inclusive 1-based ranges.
fn parse_ranges(spec: &str) -> Result<Vec<(usize, usize)>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            None => {
                let n: usize = part
                    .parse()
                    .map_err(|_| format!("invalid range: '{part}'"))?;
                if n == 0 {
                    return Err("fields are 1-based".into());
                }
                out.push((n, n));
            }
            Some(("", b)) => {
                let b: usize = b
                    .parse()
                    .map_err(|_| format!("invalid range: '{part}'"))?;
                out.push((1, b.max(1)));
            }
            Some((a, "")) => {
                let a: usize = a
                    .parse()
                    .map_err(|_| format!("invalid range: '{part}'"))?;
                out.push((a, usize::MAX / 4));
            }
            Some((a, b)) => {
                let a: usize = a
                    .parse()
                    .map_err(|_| format!("invalid range: '{part}'"))?;
                let b: usize = b
                    .parse()
                    .map_err(|_| format!("invalid range: '{part}'"))?;
                out.push((a, b));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("invalid ranges: '{spec}'"));
    }
    Ok(out)
}

// ---------------------------------------------------------------- tr

pub fn tr(args: &[String]) -> i32 {
    let mut delete = false;
    let mut squeeze = false;
    let mut sets: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'd' => delete = true,
                    's' => squeeze = true,
                    'c' | 'C' | 't' => {}
                    other => return error("tr", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            sets.push(a.clone());
        }
    }
    if sets.is_empty() || (delete && sets.len() != 1) || (!delete && sets.len() < 1) {
        return error("tr", "usage: tr [-d] [-s] set1 [set2]");
    }
    let set1 = match parse_set(&sets[0]) {
        Ok(v) => v,
        Err(e) => return error("tr", &e),
    };
    let set2 = if sets.len() > 1 {
        match parse_set(&sets[1]) {
            Ok(v) => Some(v),
            Err(e) => return error("tr", &e),
        }
    } else {
        None
    };

    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return error("tr", "failed to read stdin");
    }

    let mut out: Vec<char> = Vec::with_capacity(input.len());
    if delete {
        let del: Vec<char> = set1.clone();
        for c in input.chars() {
            if !del.contains(&c) {
                out.push(c);
            }
        }
        if squeeze {
            out = squeeze_chars(out, &set1);
        }
    } else {
        let map_from: Vec<char> = set1.clone();
        let map_to: Option<&Vec<char>> = set2.as_ref();
        for c in input.chars() {
            if let Some(idx) = map_from.iter().position(|&x| x == c) {
                let repl = match map_to {
                    Some(t) => t
                        .get(idx)
                        .copied()
                        .or_else(|| t.last().copied())
                        .unwrap_or(c),
                    None => c,
                };
                out.push(repl);
            } else {
                out.push(c);
            }
        }
        if squeeze {
            let sq = set2.as_ref().unwrap_or(&set1);
            out = squeeze_chars(out, sq);
        }
    }
    let text: String = out.into_iter().collect();
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(text.as_bytes());
    let _ = lock.flush();
    0
}

fn squeeze_chars(v: Vec<char>, set: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(v.len());
    let mut prev: Option<char> = None;
    for c in v {
        if Some(c) == prev && set.contains(&c) {
            continue;
        }
        prev = Some(c);
        out.push(c);
    }
    out
}

fn parse_set(spec: &str) -> Result<Vec<char>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            match chars[i] {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                'a' => '\u{7}',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'v' => '\u{b}',
                '0' => {
                    let mut v = 0u32;
                    let mut n = 0;
                    while n < 3 && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                        i += 1;
                        v = v * 8 + (chars[i] as u32 - '0' as u32);
                        n += 1;
                    }
                    char::from_u32(v).unwrap_or('\0')
                }
                other => other,
            }
        } else {
            chars[i]
        };
        // Range?
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != '-' {
            let hi = chars[i + 2];
            if (c as u32) <= (hi as u32) {
                for cp in (c as u32)..=(hi as u32) {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                }
            } else {
                for cp in (hi as u32)..=(c as u32) {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                }
                out.reverse();
            }
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    if out.is_empty() {
        return Err("empty set".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------- grep

pub fn grep(args: &[String]) -> i32 {
    let mut ignore_case = false;
    let mut invert = false;
    let mut number = false;
    let mut count_only = false;
    let mut word = false;
    let mut whole = false;
    let mut fixed = false;
    let mut quiet = false;
    let mut show_name = false;
    let mut patterns: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            files.extend(args[i + 1..].iter().cloned());
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            if a == "-e" || a == "-f" {
                i += 1;
                match args.get(i) {
                    Some(p) if a == "-e" => patterns.push(p.clone()),
                    Some(p) if a == "-f" => {
                        // Read patterns from a file (one per line).
                        match std::fs::read_to_string(p) {
                            Ok(s) => patterns.extend(s.lines().map(|l| l.to_string())),
                            Err(e) => {
                                eprintln!("grep: {p}: {}", e.to_string());
                                return 2;
                            }
                        }
                    }
                    _ => return error("grep", "option requires an argument"),
                }
                i += 1;
                continue;
            }
            for c in a[1..].chars() {
                match c {
                    'i' => ignore_case = true,
                    'v' => invert = true,
                    'n' => number = true,
                    'c' => count_only = true,
                    'w' => word = true,
                    'x' => whole = true,
                    'F' => fixed = true,
                    'E' => {} // ERE is the default here
                    'q' | 'Q' | 'H' | 'h' => {
                        if c == 'q' {
                            quiet = true;
                        }
                        if c == 'H' {
                            show_name = true;
                        }
                    }
                    'l' | 'L' | 'm' | 'o' | 's' | 'a' | 'r' | 't' | 'u' | 'y' => {}
                    other => {
                        eprintln!("grep: invalid option -- '{other}'");
                        return 2;
                    }
                }
            }
            i += 1;
            continue;
        }
        files.push(a.clone());
        i += 1;
    }
    if patterns.is_empty() {
        if files.is_empty() {
            return error("grep", "missing pattern");
        }
        patterns.push(files.remove(0));
    }
    if patterns.is_empty() {
        return error("grep", "missing pattern");
    }

    // Compile patterns.
    let compiled: Vec<Regex> = if fixed {
        Vec::new()
    } else {
        let mut v = Vec::new();
        for p in &patterns {
            let p = if ignore_case {
                p // handled during match below
            } else {
                p
            };
            match Regex::new(p) {
                Ok(r) => v.push(r),
                Err(e) => {
                    eprintln!("grep: {p}: {e}");
                    return 2;
                }
            }
        }
        v
    };

    let mut code = 1; // 1 = no match
    let multi = files.len() > 1;
    let inputs: Vec<(String, String)> = if files.is_empty() {
        let mut s = String::new();
        if io::stdin().read_to_string(&mut s).is_err() {
            return error("grep", "failed to read stdin");
        }
        vec![("<stdin>".to_string(), s)]
    } else {
        let mut v = Vec::new();
        for f in &files {
            match std::fs::read_to_string(f) {
                Ok(s) => v.push((f.clone(), s)),
                Err(e) => {
                    eprintln!("grep: {f}: {}", e.to_string());
                    code = 2;
                }
            }
        }
        v
    };

    for (name, content) in inputs {
        let mut matched = 0usize;
        for (idx, line) in content.lines().enumerate() {
            let hay = if ignore_case {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            let hit = if fixed {
                patterns.iter().any(|p| {
                    let needle = if ignore_case {
                        p.to_lowercase()
                    } else {
                        p.clone()
                    };
                    hay.contains(&needle)
                })
            } else {
                compiled.iter().any(|r| {
                    if whole {
                        let chars: Vec<char> = hay.chars().collect();
                        r.find_at(&chars, 0).map(|(s, e)| s == 0 && e == chars.len()) == Some(true)
                    } else {
                        r.is_match(&hay)
                    }
                })
            };
            let hit = if word && hit && !fixed {
                // Verify a real word boundary on the matched span.
                let chars: Vec<char> = hay.chars().collect();
                let mut found = false;
                let mut from = 0;
                while let Some((s, e)) = compiled.iter().find_map(|r| r.find_at(&chars, from)) {
                    let before_ok = s == 0 || !is_word(chars[s - 1]);
                    let after_ok = e >= chars.len() || !is_word(chars[e]);
                    if before_ok && after_ok {
                        found = true;
                        break;
                    }
                    from = s + 1;
                    if from > chars.len() {
                        break;
                    }
                }
                found
            } else {
                hit
            };
            let hit = hit != invert;
            if hit {
                matched += 1;
                if count_only || quiet {
                    continue;
                }
                let mut prefix = String::new();
                if multi || show_name {
                    prefix.push_str(&format!("{name}:"));
                }
                if number {
                    prefix.push_str(&format!("{}:", idx + 1));
                }
                if !count_only {
                    println!("{prefix}{line}");
                }
                if quiet {
                    code = 0;
                    return code;
                }
            }
        }
        if count_only {
            if multi || show_name {
                println!("{name}:{matched}");
            } else {
                println!("{matched}");
            }
        }
        if matched > 0 && code != 2 {
            code = 0;
        }
    }
    code
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------- which

pub fn which(args: &[String]) -> i32 {
    if args.is_empty() {
        return error("which", "missing operand");
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let mut code = 0;
    for name in args {
        if name.contains('/') || name.contains('\\') {
            if Path::new(name).is_file() {
                println!("{name}");
            } else {
                eprintln!("which: {name} not found");
                code = 1;
            }
            continue;
        }
        match find_in_path(name, &path) {
            Some(p) => println!("{}", p.display()),
            None => {
                eprintln!("which: {name} not found");
                code = 1;
            }
        }
    }
    code
}

fn find_in_path(name: &str, path: &str) -> Option<std::path::PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let base = Path::new(dir).join(name);
        if base.is_file() {
            return Some(base);
        }
        for ext in &exts {
            let candidate = Path::new(dir).join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- clear / seq

pub fn clear(_args: &[String]) -> i32 {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(b"\x1b[2J\x1b[H");
    let _ = lock.flush();
    0
}

pub fn seq(args: &[String]) -> i32 {
    let nums: Vec<i64> = args
        .iter()
        .filter(|a| !a.starts_with('-') || a[1..].chars().all(|c| c.is_ascii_digit() || c == '.'))
        .filter_map(|a| a.parse::<i64>().ok())
        .collect();
    let (first, step, last) = match nums.len() {
        1 => (1, 1, nums[0]),
        2 => (nums[0], 1, nums[1]),
        3 => (nums[0], nums[1], nums[2]),
        _ => return error("seq", "usage: seq [first] [increment] last"),
    };
    if step == 0 {
        return error("seq", "increment cannot be zero");
    }
    let mut n = first;
    if step > 0 {
        while n <= last {
            println!("{n}");
            n += step;
        }
    } else {
        while n >= last {
            println!("{n}");
            n += step;
        }
    }
    0
}

// ---------------------------------------------------------------- test / [

pub fn test_cmd(args: &[String]) -> i32 {
    let mut toks: Vec<String> = args.to_vec();
    if toks.last().map(|s| s.as_str()) == Some("]") {
        toks.pop();
    } else if args.first().map(|s| s.as_str()) == Some("[") {
        toks.remove(0);
    }
    let mut cursor = 0usize;
    let ok = eval_or(&toks, &mut cursor);
    if ok {
        0
    } else {
        1
    }
}

fn eval_or(toks: &[String], cur: &mut usize) -> bool {
    let mut left = eval_and(toks, cur);
    while toks.get(*cur).map(|s| s.as_str()) == Some("-o") {
        *cur += 1;
        let right = eval_and(toks, cur);
        left = left || right;
    }
    left
}

fn eval_and(toks: &[String], cur: &mut usize) -> bool {
    let mut left = eval_unary(toks, cur);
    while toks.get(*cur).map(|s| s.as_str()) == Some("-a") {
        *cur += 1;
        let right = eval_unary(toks, cur);
        left = left && right;
    }
    left
}

fn eval_unary(toks: &[String], cur: &mut usize) -> bool {
    let tok = match toks.get(*cur) {
        Some(t) => t.clone(),
        None => return false,
    };
    if tok == "!" {
        *cur += 1;
        return !eval_unary(toks, cur);
    }
    if tok == "(" {
        *cur += 1;
        let v = eval_or(toks, cur);
        if toks.get(*cur).map(|s| s.as_str()) == Some(")") {
            *cur += 1;
        }
        return v;
    }
    // Binary operator form: `<a> <op> <b>`
    if let Some(op) = toks.get(*cur + 1) {
        if let Some(r) = eval_binary(&tok, op, toks.get(*cur + 2)) {
            *cur += 3;
            return r;
        }
    }
    // Unary operator form: `<op> <arg>`
    if let Some(arg) = toks.get(*cur + 1) {
        if is_unary_op(&tok) {
            *cur += 2;
            return eval_unary_op(&tok, arg);
        }
    }
    // Single argument: non-empty string.
    *cur += 1;
    !tok.is_empty()
}

fn is_unary_op(op: &str) -> bool {
    matches!(
        op,
        "-e" | "-f" | "-d" | "-r" | "-w" | "-x" | "-s" | "-h" | "-L" | "-p" | "-S" | "-b"
            | "-c" | "-g" | "-u" | "-k" | "-t" | "-G" | "-O" | "-N" | "-z" | "-n"
    )
}

fn eval_unary_op(op: &str, arg: &str) -> bool {
    match op {
        "-n" => !arg.is_empty(),
        "-z" => arg.is_empty(),
        "-t" => {
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                match arg.parse::<i32>() {
                    Ok(fd) => unsafe { libc::isatty(fd) == 1 },
                    Err(_) => false,
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (op, arg);
                false
            }
        }
        _ => file_test(op, Path::new(arg)),
    }
}

fn file_test(op: &str, p: &Path) -> bool {
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match op {
        "-e" => p.exists(),
        "-L" | "-h" => md.file_type().is_symlink(),
        "-f" => md.is_file(),
        "-d" => md.is_dir(),
        "-s" => md.len() > 0,
        "-p" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                md.file_type().is_fifo()
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        "-S" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                md.file_type().is_socket()
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        "-b" | "-c" => false,
        "-r" | "-w" | "-x" | "-g" | "-u" | "-k" | "-G" | "-O" | "-N" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let mode = md.mode();
                match op {
                    "-r" => mode & 0o400 != 0,
                    "-w" => mode & 0o200 != 0,
                    "-x" => mode & 0o100 != 0,
                    "-g" => mode & 0o2000 != 0,
                    "-u" => mode & 0o4000 != 0,
                    "-k" => mode & 0o1000 != 0,
                    _ => false,
                }
            }
            #[cfg(not(unix))]
            {
                let perms = std::fs::metadata(p).map(|m| m.permissions());
                match op {
                    "-r" => perms.as_ref().map(|p| !p.readonly()).unwrap_or(false),
                    "-w" => perms.as_ref().map(|p| !p.readonly()).unwrap_or(false),
                    _ => false,
                }
            }
        }
        _ => false,
    }
}

fn eval_binary(a: &str, op: &str, b: Option<&String>) -> Option<bool> {
    let b = b?;
    match op {
        "=" | "==" => Some(a == b),
        "!=" => Some(a != b),
        "-eq" => Some(int(a)? == int(b)?),
        "-ne" => Some(int(a)? != int(b)?),
        "-lt" => Some(int(a)? < int(b)?),
        "-le" => Some(int(a)? <= int(b)?),
        "-gt" => Some(int(a)? > int(b)?),
        "-ge" => Some(int(a)? >= int(b)?),
        _ => None,
    }
}

fn int(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}
