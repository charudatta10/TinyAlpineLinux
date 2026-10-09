//! Shell builtins.

use crate::exec::{self, Flow, Io, Prepared};
use crate::shell::Shell;
use std::path::{Path, PathBuf};

pub fn run(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let name = args[0].as_str();
    let rest = &args[1..];
    match name {
        ":" | "true" => Ok(0),
        "false" => Ok(1),
        "cd" => cd(sh, rest, io),
        "pwd" => pwd(sh, rest, io),
        "export" => export(sh, rest, io),
        "unset" => unset(sh, rest, io),
        "echo" => echo(rest, io),
        "printf" => printf(rest, io),
        "test" | "[" => Ok(test(sh, rest)),
        "read" => read(sh, rest, io),
        "exec" => exec_builtin(sh, rest, io),
        "exit" => {
            let code = match rest.first() {
                Some(a) => a.parse().unwrap_or(sh.status),
                None => sh.status,
            };
            Err(Flow::Exit(code))
        }
        "return" => {
            let code = match rest.first() {
                Some(a) => a.parse().unwrap_or(sh.status),
                None => sh.status,
            };
            Err(Flow::Return(code))
        }
        "break" => Err(Flow::Break(parse_count(rest, 1))),
        "continue" => Err(Flow::Continue(parse_count(rest, 1))),
        "set" => Ok(set(sh, rest, io)),
        "shift" => Ok(shift(sh, rest, io)),
        "wait" => {
            let last = exec::wait_all();
            sh.status = last;
            Ok(0)
        }
        "source" | "." => source(sh, rest, io),
        "eval" => eval(sh, rest, io),
        "local" => Ok(local(sh, rest, io)),
        "umask" => Ok(umask(sh, rest, io)),
        "command" => command(sh, rest, io),
        "type" => type_of(sh, rest, io),
        "hash" | "times" => Ok(0),
        "jobs" => Ok(0),
        "let" => let_cmd(sh, rest, io),
        "vars" => Ok(vars(sh, io)),
        other => {
            io.error(&format!("sh: {other}: not a builtin"));
            Ok(127)
        }
    }
}

fn parse_count(rest: &[String], default: u32) -> u32 {
    rest.first()
        .and_then(|a| a.parse().ok())
        .unwrap_or(default)
        .max(1)
}

// ---------------------------------------------------------------- basics

fn cd(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let target: PathBuf = match args.first() {
        None => match sh.get("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => {
                io.error("sh: cd: HOME not set");
                return Ok(1);
            }
        },
        Some(t) if t == "-" => match sh.get("OLDPWD") {
            Some(d) => PathBuf::from(d),
            None => {
                io.error("sh: cd: OLDPWD not set");
                return Ok(1);
            }
        },
        Some(t) => sh.resolve(t),
    };
    let meta = match std::fs::metadata(&target) {
        Ok(m) => m,
        Err(_) => {
            io.error(&format!("sh: cd: {}: No such directory", target.display()));
            return Ok(1);
        }
    };
    if !meta.is_dir() {
        io.error(&format!("sh: cd: {}: Not a directory", target.display()));
        return Ok(1);
    }
    let old = sh.cwd.clone();
    let new = std::fs::canonicalize(&target).unwrap_or(target);
    let new = PathBuf::from(crate::shell::strip_unc(&new.to_string_lossy()));
    sh.cwd = new.clone();
    sh.set_var("OLDPWD", old.to_string_lossy().into_owned());
    sh.set_var("PWD", new.to_string_lossy().into_owned());
    sh.exported.insert("OLDPWD".into());
    sh.exported.insert("PWD".into());
    if args.first().map(|s| s.as_str()) == Some("-") {
        let _ = io.stdout.write_str(&format!("{}\n", sh.cwd.display()));
    }
    Ok(0)
}

fn pwd(sh: &Shell, _args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let _ = io.stdout.write_str(&format!("{}\n", sh.cwd.display()));
    Ok(0)
}

fn export(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    if args.is_empty() {
        let mut keys: Vec<&String> = sh.exported.iter().collect();
        keys.sort();
        for k in keys {
            let v = sh.vars.get(k).cloned().unwrap_or_default();
            let _ = io.stdout.write_str(&format!("export {k}={:?}\n", v));
        }
        return Ok(0);
    }
    for a in args {
        if let Some((k, v)) = a.split_once('=') {
            if is_name(k) {
                sh.set_var(k, v.to_string());
                sh.exported.insert(k.to_string());
            } else {
                io.error(&format!("sh: export: '{k}': not a valid identifier"));
                return Ok(1);
            }
        } else if is_name(a) {
            sh.exported.insert(a.clone());
        } else {
            io.error(&format!("sh: export: '{a}': not a valid identifier"));
            return Ok(1);
        }
    }
    Ok(0)
}

fn unset(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let mut mode = 'v';
    let mut names: Vec<String> = Vec::new();
    for a in args {
        if a == "-v" {
            mode = 'v';
        } else if a == "-f" {
            mode = 'f';
        } else {
            names.push(a.clone());
        }
    }
    for n in names {
        if mode == 'f' {
            sh.functions.remove(&n);
        } else {
            sh.unset_var(&n);
        }
    }
    let _ = io;
    Ok(0)
}

fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_alphabetic() || c == '_' => cs.all(|c| c.is_alphanumeric() || c == '_'),
        _ => false,
    }
}

// ---------------------------------------------------------------- io helpers

fn echo(args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let mut newline = true;
    let mut expand_esc = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') || a == "-" {
            break;
        }
        if !a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'n' => newline = false,
                'e' => expand_esc = true,
                'E' => expand_esc = false,
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
    if expand_esc {
        out = crate::expand::unescape(&out);
    }
    if newline {
        out.push('\n');
    }
    let _ = io.stdout.write_str(&out);
    let _ = io.stdout.flush();
    Ok(0)
}

fn printf(args: &[String], io: &mut Io) -> Result<i32, Flow> {
    if args.is_empty() {
        io.error("sh: printf: missing operand");
        return Ok(1);
    }
    let fmt = crate::expand::unescape(&args[0]);
    let mut rest: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
    let mut out = String::new();
    loop {
        let before = rest.len();
        format_into(&mut out, &fmt, &mut rest);
        if rest.is_empty() || rest.len() == before {
            break;
        }
    }
    let _ = io.stdout.write_str(&out);
    let _ = io.stdout.flush();
    Ok(0)
}

fn format_into(out: &mut String, fmt: &str, args: &mut Vec<&str>) {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
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
        let mut width: i64 = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            width = width * 10 + (chars[i] as i64 - '0' as i64);
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p = 0usize;
            while i < chars.len() && chars[i].is_ascii_digit() {
                p = p * 10 + (chars[i] as usize - '0' as usize);
                i += 1;
            }
            prec = Some(p);
        }
        while i < chars.len() && matches!(chars[i], 'l' | 'h' | 'z' | 'j' | 't') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let conv = chars[i];
        i += 1;
        let arg = match args.first_mut() {
            Some(a) => {
                let v = *a;
                args.remove(0);
                v
            }
            None => "",
        };
        match conv {
            's' => {
                let mut s = arg.to_string();
                if let Some(p) = prec {
                    s = s.chars().take(p).collect();
                }
                out.push_str(&pad(&s, width, left, false));
            }
            'c' => {
                let c = arg.chars().next().unwrap_or('\0');
                out.push_str(&pad(&c.to_string(), width, left, false));
            }
            'd' | 'i' => {
                let n: i64 = arg.trim().parse().unwrap_or(0);
                out.push_str(&pad(&n.to_string(), width, left, zero && !left));
            }
            'u' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                out.push_str(&pad(&n.to_string(), width, left, zero && !left));
            }
            'x' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                out.push_str(&pad(&format!("{n:x}"), width, left, zero && !left));
            }
            'X' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                out.push_str(&pad(&format!("{n:X}"), width, left, zero && !left));
            }
            'o' => {
                let n: u64 = arg.trim().parse().unwrap_or(0);
                out.push_str(&pad(&format!("{n:o}"), width, left, zero && !left));
            }
            'f' | 'F' => {
                let n: f64 = arg.trim().parse().unwrap_or(0.0);
                let s = match prec {
                    Some(p) => format!("{n:.p$}"),
                    None => format!("{n:.6}"),
                };
                out.push_str(&pad(&s, width, left, false));
            }
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
}

fn pad(s: &str, width: i64, left: bool, zero: bool) -> String {
    let len = s.chars().count() as i64;
    if len >= width {
        return s.to_string();
    }
    let n = (width - len) as usize;
    if left {
        format!("{s}{}", " ".repeat(n))
    } else if zero {
        format!("{}{s}", "0".repeat(n))
    } else {
        format!("{}{s}", " ".repeat(n))
    }
}

// ---------------------------------------------------------------- read

fn read(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let mut names: Vec<String> = Vec::new();
    for a in args {
        if a == "-r" || a == "-n" || a.starts_with('-') {
            continue;
        }
        names.push(a.clone());
    }
    if names.is_empty() {
        names.push("REPLY".into());
    }
    let line = match io.stdin.read_line() {
        Ok(Some(l)) => l,
        Ok(None) => return Ok(1),
        Err(er) => {
            io.error(&format!("sh: read: {}", er));
            return Ok(1);
        }
    };
    // Strip the line terminator (both LF and CRLF).
    let line = line.trim_end_matches('\n').trim_end_matches('\r').to_string();
    let ifs = sh.get("IFS").unwrap_or_else(|| " \t\n".into());
    let ifs_ws = ifs.chars().all(|c| c.is_whitespace());
    let fields: Vec<&str> = if names.len() > 1 {
        if ifs_ws {
            line.split_whitespace().collect()
        } else {
            line.split(|c: char| ifs.contains(c)).collect()
        }
    } else {
        vec![line.as_str()]
    };
    for (i, n) in names.iter().enumerate() {
        let val = if i + 1 == names.len() {
            // Last variable gets the rest of the line.
            if names.len() == 1 {
                line.clone()
            } else {
                fields
                    .iter()
                    .skip(i)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(&ifs.chars().next().unwrap_or(' ').to_string())
            }
        } else {
            fields.get(i).map(|s| (*s).to_string()).unwrap_or_default()
        };
        sh.set_var(n, val);
    }
    Ok(0)
}

// ---------------------------------------------------------------- exec / eval / source

fn exec_builtin(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    if args.is_empty() {
        return Ok(0);
    }
    let p = Prepared {
        assigns: Vec::new(),
        args: args.to_vec(),
        redirs: Vec::new(),
    };
    match exec::run_external(sh, p, io) {
        Ok(()) => Err(Flow::Exit(sh.status)),
        Err(f) => Err(f),
    }
}

fn eval(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let src = args.join(" ");
    let mut parsed = match crate::parse::parse(&src) {
        Ok(s) => s,
        Err(er) => {
            io.error(&format!("sh: eval: syntax error: {er}"));
            sh.status = 2;
            return Ok(2);
        }
    };
    sh.adopt(&mut parsed);
    let mut locals = Vec::new();
    match exec::exec_list(sh, &parsed.nodes, io, &mut locals) {
        Ok(()) => Ok(sh.status),
        Err(f) => Err(f),
    }
}

fn source(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let file = match args.first() {
        Some(f) => f.clone(),
        None => {
            io.error("sh: source: filename argument required");
            return Ok(2);
        }
    };
    let path = sh.resolve(&file);
    let text = std::fs::read_to_string(&path).or_else(|_| {
        exec::find_in_path(sh, &file)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
    });
    let text = match text {
        Ok(t) => t,
        Err(_) => {
            io.error(&format!("sh: source: {file}: cannot open"));
            return Ok(1);
        }
    };
    let mut parsed = match crate::parse::parse(&text) {
        Ok(s) => s,
        Err(er) => {
            io.error(&format!("sh: {file}: syntax error: {er}"));
            sh.status = 2;
            return Ok(2);
        }
    };
    sh.adopt(&mut parsed);
    let mut locals = Vec::new();
    match exec::exec_list(sh, &parsed.nodes, io, &mut locals) {
        Ok(()) => Ok(sh.status),
        Err(f) => Err(f),
    }
}

// ---------------------------------------------------------------- set / shift / local

fn set(sh: &mut Shell, args: &[String], io: &mut Io) -> i32 {
    if args.is_empty() {
        let mut keys: Vec<&String> = sh.vars.keys().collect();
        keys.sort();
        for k in keys {
            let v = sh.vars.get(k).cloned().unwrap_or_default();
            let _ = io.stdout.write_str(&format!("{k}={v}\n"));
        }
        return 0;
    }
    if args[0] == "--" {
        sh.positional = args[1..].to_vec();
        return 0;
    }
    for a in args {
        if let Some(flags) = a.strip_prefix('-') {
            for c in flags.chars() {
                match c {
                    'e' => sh.errexit = true,
                    'x' => sh.xtrace = true,
                    'u' => sh.nounset = true,
                    'v' => {}
                    'f' => {}
                    'h' | 'n' | 'C' => {}
                    other => {
                        let _ = io.error(&format!("sh: set: -{other}: invalid option"));
                        return 2;
                    }
                }
            }
        } else if let Some(flags) = a.strip_prefix('+') {
            for c in flags.chars() {
                match c {
                    'e' => sh.errexit = false,
                    'x' => sh.xtrace = false,
                    'u' => sh.nounset = false,
                    _ => {}
                }
            }
        }
    }
    0
}

fn shift(sh: &mut Shell, args: &[String], io: &mut Io) -> i32 {
    let n: usize = args
        .first()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1);
    if sh.positional.len() < n {
        io.error("sh: shift: can't shift that many");
        return 1;
    }
    sh.positional.drain(0..n);
    0
}

fn local(sh: &mut Shell, args: &[String], io: &mut Io) -> i32 {
    if sh.scopes.is_empty() {
        io.error("sh: local: can only be used in a function");
        return 1;
    }
    for a in args {
        if let Some((k, v)) = a.split_once('=') {
            if !is_name(k) {
                io.error(&format!("sh: local: '{k}': not a valid identifier"));
                return 1;
            }
            let prev = sh.vars.get(k).cloned();
            sh.set_var(k, v.to_string());
            if let Some(scope) = sh.scopes.last_mut() {
                scope.push((k.to_string(), prev));
            }
        } else if is_name(a) {
            let prev = sh.vars.get(a).cloned();
            sh.set_var(a, String::new());
            if let Some(scope) = sh.scopes.last_mut() {
                scope.push((a.clone(), prev));
            }
        } else {
            io.error(&format!("sh: local: '{a}': not a valid identifier"));
            return 1;
        }
    }
    0
}

fn umask(sh: &mut Shell, args: &[String], io: &mut Io) -> i32 {
    match args.first() {
        None => {
            let _ = io.stdout.write_str(&format!("{:04o}\n", sh.umask));
            0
        }
        Some(a) => match u32::from_str_radix(a.trim_start_matches("0"), 8) {
            Ok(v) => {
                sh.umask = v;
                0
            }
            Err(_) => {
                let _ = io.error(&format!("sh: umask: '{a}': invalid mask"));
                1
            }
        },
    }
}

// ---------------------------------------------------------------- introspection

fn command(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let mut verbose = false;
    let mut names: Vec<&String> = Vec::new();
    for a in args {
        if a == "-v" || a == "-V" {
            verbose = true;
        } else if a.starts_with('-') {
            let _ = io.error(&format!("sh: command: invalid option -- '{a}'"));
            return Ok(2);
        } else {
            names.push(a);
        }
    }
    if !verbose {
        io.error("sh: command: only '-v' is supported");
        return Ok(2);
    }
    let mut code = 0;
    for n in names {
        if Shell::is_builtin(n) {
            let _ = io.stdout.write_str(&format!("{n}\n"));
        } else if sh.functions.contains_key(n) {
            let _ = io.stdout.write_str(&format!("{n} is a shell function\n"));
        } else if exec::find_in_path(sh, n).is_some() {
            let _ = io.stdout.write_str(&format!("{n}\n"));
        } else {
            let _ = io.error(&format!("sh: command: {n}: not found"));
            code = 1;
        }
    }
    Ok(code)
}

fn type_of(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    let mut code = 0;
    for n in args {
        if n.starts_with('-') {
            continue;
        }
        if sh.functions.contains_key(n) {
            let _ = io.stdout.write_str(&format!("{n} is a shell function\n"));
        } else if Shell::is_builtin(n) {
            let _ = io.stdout.write_str(&format!("{n} is a shell builtin\n"));
        } else if let Some(p) = exec::find_in_path(sh, n) {
            let _ = io.stdout.write_str(&format!("{n} is {p}\n"));
        } else {
            let _ = io.error(&format!("sh: type: {n}: not found"));
            code = 1;
        }
    }
    Ok(code)
}

fn vars(sh: &mut Shell, io: &mut Io) -> i32 {
    let mut keys: Vec<&String> = sh.vars.keys().collect();
    keys.sort();
    for k in keys {
        let v = sh.vars.get(k).cloned().unwrap_or_default();
        let mark = if sh.exported.contains(k) { "*" } else { " " };
        let _ = io.stdout.write_str(&format!("{mark} {k}={v}\n"));
    }
    0
}

fn let_cmd(sh: &mut Shell, args: &[String], io: &mut Io) -> Result<i32, Flow> {
    if args.is_empty() {
        return Ok(0);
    }
    let mut last = 0i64;
    for a in args {
        let (name, expr) = match a.split_once('=') {
            Some((lhs, rhs)) if is_name(lhs.trim()) && !a.starts_with("==") => {
                (Some(lhs.trim().to_string()), rhs.to_string())
            }
            _ => (None, a.clone()),
        };
        let lookup = |n: &str| -> i64 {
            sh.get(n)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(0)
        };
        let val = match crate::arith::eval(&expr, &lookup) {
            Ok(v) => v,
            Err(er) => {
                let _ = io.error(&format!("sh: let: {er}"));
                return Ok(1);
            }
        };
        if let Some(n) = name {
            sh.set_var(&n, val.to_string());
        }
        last = val;
    }
    Ok(if last != 0 { 0 } else { 1 })
}

// ---------------------------------------------------------------- test

fn test(sh: &mut Shell, args: &[String]) -> i32 {
    let mut toks: Vec<String> = args.to_vec();
    if toks.last().map(|s| s.as_str()) == Some("]") {
        toks.pop();
    }
    let mut cur = 0usize;
    if eval_or(sh, &toks, &mut cur) {
        0
    } else {
        1
    }
}

fn eval_or(sh: &mut Shell, t: &[String], cur: &mut usize) -> bool {
    let mut left = eval_and(sh, t, cur);
    while t.get(*cur).map(|s| s.as_str()) == Some("-o") {
        *cur += 1;
        let right = eval_and(sh, t, cur);
        left = left || right;
    }
    left
}

fn eval_and(sh: &mut Shell, t: &[String], cur: &mut usize) -> bool {
    let mut left = eval_unary(sh, t, cur);
    while t.get(*cur).map(|s| s.as_str()) == Some("-a") {
        *cur += 1;
        let right = eval_unary(sh, t, cur);
        left = left && right;
    }
    left
}

fn eval_unary(sh: &mut Shell, t: &[String], cur: &mut usize) -> bool {
    let tok = match t.get(*cur) {
        Some(x) => x.clone(),
        None => return false,
    };
    if tok == "!" {
        *cur += 1;
        return !eval_unary(sh, t, cur);
    }
    if tok == "(" {
        *cur += 1;
        let v = eval_or(sh, t, cur);
        if t.get(*cur).map(|s| s.as_str()) == Some(")") {
            *cur += 1;
        }
        return v;
    }
    // binary
    if let Some(op) = t.get(*cur + 1) {
        if let Some(b) = t.get(*cur + 2) {
            if let Some(r) = binary(&tok, op, b) {
                *cur += 3;
                return r;
            }
        }
    }
    // unary
    if let Some(arg) = t.get(*cur + 1) {
        if is_unop(&tok) {
            *cur += 2;
            return unop(sh, &tok, arg);
        }
    }
    *cur += 1;
    !tok.is_empty()
}

fn is_unop(op: &str) -> bool {
    matches!(
        op,
        "-e" | "-f" | "-d" | "-r" | "-w" | "-x" | "-s" | "-h" | "-L" | "-p" | "-S" | "-z" | "-n"
            | "-t" | "-G" | "-O" | "-b" | "-c" | "-g" | "-u" | "-k" | "-N"
    )
}

fn binary(a: &str, op: &str, b: &str) -> Option<bool> {
    match op {
        "=" | "==" => Some(a == b),
        "!=" => Some(a != b),
        _ => {
            let x = a.trim().parse::<i64>().ok()?;
            let y = b.trim().parse::<i64>().ok()?;
            Some(match op {
                "-eq" => x == y,
                "-ne" => x != y,
                "-lt" => x < y,
                "-le" => x <= y,
                "-gt" => x > y,
                "-ge" => x >= y,
                _ => return None,
            })
        }
    }
}

fn unop(sh: &Shell, op: &str, arg: &str) -> bool {
    match op {
        "-n" => !arg.is_empty(),
        "-z" => arg.is_empty(),
        "-t" => false, // no isatty without libc plumbing; treated as false
        _ => file_test(sh, op, Path::new(arg)),
    }
}

fn file_test(sh: &Shell, op: &str, p: &Path) -> bool {
    let path = resolve_path(sh, p);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match op {
        "-e" => path.exists(),
        "-h" | "-L" => md.file_type().is_symlink(),
        "-f" => md.is_file(),
        "-d" => md.is_dir(),
        "-s" => md.len() > 0,
        "-r" | "-w" | "-x" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let mode = md.mode();
                match op {
                    "-r" => mode & 0o400 != 0,
                    "-w" => mode & 0o200 != 0,
                    _ => mode & 0o100 != 0,
                }
            }
            #[cfg(not(unix))]
            {
                let _ = &md;
                !matches!(op, "-x")
            }
        }
        "-p" | "-S" | "-b" | "-c" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                match op {
                    "-p" => md.file_type().is_fifo(),
                    "-S" => md.file_type().is_socket(),
                    _ => false,
                }
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- helpers used by exec

/// Resolve a possibly relative path against the shell's logical cwd.
pub fn resolve_path(sh: &Shell, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        sh.cwd.join(p)
    }
}
