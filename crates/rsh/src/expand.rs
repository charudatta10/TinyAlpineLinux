//! Word expansion: tilde, parameter, command substitution, arithmetic,
//! field splitting, pathname expansion (globbing) and quote removal.
//!
//! 1. Walk the word's [`Part`]s and emit *cells* — characters tagged with a
//!    "quoted" flag, plus forced field boundaries produced by `$@`.
//! 2. Split cells into fields using `$IFS` (only unquoted separators split).
//! 3. Glob fields that contain unquoted pattern characters.

use crate::arith;
use crate::ast::{Part, Word};
use crate::exec;
use crate::shell::Shell;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
enum Cell {
    /// A character plus whether it was quoted.
    Ch(char, bool),
    /// Forced field boundary (from `$@`).
    Sep,
}

#[derive(Debug)]
pub struct ExpandErr(pub String);

impl std::fmt::Display for ExpandErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

type E<T> = Result<T, ExpandErr>;

/// Expand a word into fields (split + glob): used for command arguments.
pub fn fields(sh: &mut Shell, w: &Word) -> E<Vec<String>> {
    if w.is_empty() {
        return Ok(Vec::new());
    }
    let mut cells: Vec<Cell> = Vec::new();
    let mut any_quoted = false;
    for (i, part) in w.iter().enumerate() {
        push_part(sh, part, false, i, w, &mut cells, &mut any_quoted)?;
    }
    finish(cells, any_quoted, sh)
}

/// Expand a word into a single string (no split/glob): redirect targets,
/// assignment values, `case` subjects, here-doc delimiters.
pub fn string(sh: &mut Shell, w: &Word) -> E<String> {
    if w.is_empty() {
        return Ok(String::new());
    }
    let mut cells: Vec<Cell> = Vec::new();
    let mut any_quoted = false;
    for (i, part) in w.iter().enumerate() {
        push_part(sh, part, true, i, w, &mut cells, &mut any_quoted)?;
    }
    let mut out = String::new();
    let mut pending_sep = false;
    for c in &cells {
        match c {
            Cell::Ch(ch, _) => {
                if pending_sep {
                    out.push(' ');
                    pending_sep = false;
                }
                out.push(*ch);
            }
            Cell::Sep => pending_sep = true,
        }
    }
    if pending_sep && !out.is_empty() {
        out.push(' ');
    }
    Ok(out)
}

/// Expand an entire word list into argv fields.
pub fn argv(sh: &mut Shell, ws: &[Word]) -> E<Vec<String>> {
    let mut out = Vec::new();
    for w in ws {
        out.extend(fields(sh, w)?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn push_part(
    sh: &mut Shell,
    part: &Part,
    quoted: bool,
    index: usize,
    whole: &Word,
    cells: &mut Vec<Cell>,
    any_quoted: &mut bool,
) -> E<()> {
    match part {
        Part::Lit(text, q) => {
            let q = *q || quoted;
            if q && text.is_empty() && index == 0 && whole.len() == 1 {
                *any_quoted = true;
            }
            for ch in text.chars() {
                cells.push(Cell::Ch(ch, q));
            }
            if q {
                *any_quoted = true;
            }
        }
        Part::Dq(inner) => {
            // Inside double quotes nothing splits; but `"$@"` still yields one
            // field per positional parameter (and zero fields when there are
            // none).
            let before = cells.len();
            let mut saw_content = false;
            for (i, p) in inner.iter().enumerate() {
                if let Part::Special('@') = p {
                    push_at(sh, true, cells);
                    if !sh.positional.is_empty() {
                        saw_content = true;
                    } else if inner.len() == 1 {
                        // `"$@"` with no parameters: no field at all.
                        let _ = i;
                    }
                } else {
                    push_part(sh, p, true, i, inner, cells, any_quoted)?;
                    saw_content = true;
                }
            }
            if saw_content || cells.len() > before {
                *any_quoted = true;
            }
        }
        Part::Var(name) => {
            let val = match sh.get(name) {
                Some(v) => v,
                None => {
                    if sh.nounset {
                        return Err(ExpandErr(format!("{name}: unbound variable")));
                    }
                    String::new()
                }
            };
            if quoted {
                *any_quoted = true;
            }
            for ch in val.chars() {
                cells.push(Cell::Ch(ch, quoted));
            }
        }
        Part::Special(c) => match c {
            '?' => push_str(&sh.status.to_string(), quoted, cells),
            '$' => push_str(&sh.pid.to_string(), quoted, cells),
            '#' => push_str(&sh.positional.len().to_string(), quoted, cells),
            '!' => push_str(&sh.last_bg, quoted, cells),
            '@' => push_at(sh, quoted, cells),
            '*' => {
                let ifs0 = first_ifs(sh);
                let joined = sh.positional.join(&ifs0.to_string());
                push_str(&joined, quoted, cells);
            }
            other => {
                return Err(ExpandErr(format!("unsupported ${other}")));
            }
        },
        Part::Pos(i) => {
            let val = if *i == 0 {
                sh.arg0.clone()
            } else {
                sh.positional
                    .get(i - 1)
                    .cloned()
                    .unwrap_or_default()
            };
            if sh.nounset && *i > 0 && *i > sh.positional.len() {
                return Err(ExpandErr(format!("${i}: unbound variable")));
            }
            if quoted {
                *any_quoted = true;
            }
            for ch in val.chars() {
                cells.push(Cell::Ch(ch, quoted));
            }
        }
        Part::CmdSub(script) => {
            let out = exec::run_capture(sh, script)
                .map_err(|e| ExpandErr(e))?;
            let out = strip_trailing_newlines(&out);
            if quoted {
                *any_quoted = true;
            }
            for ch in out.chars() {
                cells.push(Cell::Ch(ch, quoted));
            }
        }
        Part::Arith(expr) => {
            let val = eval_arith(sh, expr)?;
            if quoted {
                *any_quoted = true;
            }
            for ch in val.chars() {
                cells.push(Cell::Ch(ch, quoted));
            }
        }
        Part::Default(name, body, assign) => {
            let cur = sh.get(name);
            let needs = match &cur {
                None => true,
                Some(v) => v.is_empty(),
            };
            if needs && *assign {
                // ${name:=body} — assign the result.
                let val = string(sh, body)?;
                sh.set_var(name, val.clone());
                if sh.exported.contains(name) {
                    // keep exporting
                }
                if quoted {
                    *any_quoted = true;
                }
                for ch in val.chars() {
                    cells.push(Cell::Ch(ch, quoted));
                }
            } else if needs {
                for (i, p) in body.iter().enumerate() {
                    push_part(sh, p, quoted, i, body, cells, any_quoted)?;
                }
            } else {
                let val = cur.unwrap();
                if quoted {
                    *any_quoted = true;
                }
                for ch in val.chars() {
                    cells.push(Cell::Ch(ch, quoted));
                }
            }
        }
        Part::Length(name) => {
            let len = sh.get(name).map(|v| v.chars().count()).unwrap_or(0);
            push_str(&len.to_string(), quoted, cells);
        }
        Part::Tilde => {
            if index == 0 && tilde_applies(whole) {
                let home = sh.get("HOME").unwrap_or_default();
                if home.is_empty() {
                    cells.push(Cell::Ch('~', false));
                } else {
                    for ch in home.chars() {
                        cells.push(Cell::Ch(ch, true));
                    }
                    *any_quoted = true;
                }
            } else {
                cells.push(Cell::Ch('~', false));
            }
        }
        Part::Dq(_) => {
            // Handled above; kept for exhaustiveness symmetry.
            debug_assert!(false, "unreachable");
        }
    }
    Ok(())
}

fn push_str(s: &str, quoted: bool, cells: &mut Vec<Cell>) {
    for ch in s.chars() {
        cells.push(Cell::Ch(ch, quoted));
    }
}

/// `$@` as separate fields. Quoted and unquoted both split per parameter;
/// quoted parameters are protected from further splitting/globbing.
fn push_at(sh: &Shell, quoted: bool, cells: &mut Vec<Cell>) {
    if sh.positional.is_empty() {
        return;
    }
    for (i, arg) in sh.positional.iter().enumerate() {
        if i > 0 {
            cells.push(Cell::Sep);
        }
        for ch in arg.chars() {
            cells.push(Cell::Ch(ch, quoted));
        }
    }
}

fn tilde_applies(w: &Word) -> bool {
    match w.get(1) {
        None => true,
        Some(Part::Lit(t, _)) => t.starts_with('/'),
        _ => false,
    }
}

fn first_ifs(sh: &Shell) -> char {
    sh.get("IFS")
        .and_then(|v| v.chars().next())
        .unwrap_or(' ')
}

fn strip_trailing_newlines(s: &str) -> String {
    s.trim_end_matches('\n').to_string()
}

fn eval_arith(sh: &mut Shell, expr: &str) -> E<String> {
    let result = {
        let lookup = |n: &str| -> i64 {
            sh.get(n)
                .and_then(|v| parse_int(&v))
                .unwrap_or(0)
        };
        arith::eval(expr, &lookup)
    };
    match result {
        Ok(v) => Ok(v.to_string()),
        Err(e) => Err(ExpandErr(format!("arithmetic error: {e}"))),
    }
}

fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| {
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i64::from_str_radix(h, 16).ok()
        } else if s.starts_with('0') && s.len() > 1 {
            i64::from_str_radix(&s[1..], 8).ok()
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------- split

fn finish(cells: Vec<Cell>, any_quoted: bool, sh: &Shell) -> E<Vec<String>> {
    if cells.is_empty() {
        return Ok(if any_quoted {
            vec![String::new()]
        } else {
            Vec::new()
        });
    }
    let ifs: Vec<char> = sh.get("IFS").unwrap_or_else(|| " \t\n".into()).chars().collect();
    let ws_mode = ifs.iter().all(|c| c.is_whitespace()) || ifs.is_empty();

    // --- field split
    let mut fields: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut pending_ws = false;
    for cell in cells {
        match cell {
            Cell::Sep => {
                if pending_ws {
                    pending_ws = false;
                    if !cur.is_empty() || !fields.is_empty() {
                        // whitespace before boundary: commit current field
                    }
                }
                if !cur.is_empty() {
                    fields.push(std::mem::take(&mut cur));
                } else if ws_mode {
                    // `a $@ b` with empty cur: keep boundary anyway
                    fields.push(Vec::new());
                } else {
                    fields.push(Vec::new());
                }
            }
            Cell::Ch(c, quoted) => {
                if !quoted && ifs.contains(&c) {
                    if ws_mode {
                        if !cur.is_empty() {
                            fields.push(std::mem::take(&mut cur));
                        }
                        // collapse whitespace runs; leading/trailing dropped
                    } else {
                        fields.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(Cell::Ch(c, quoted));
                }
            }
        }
    }
    if !cur.is_empty() {
        fields.push(cur);
    }
    let _ = pending_ws;

    // A word that is entirely IFS whitespace yields no fields (POSIX), but a
    // quoted empty expansion must still produce one empty field.
    if fields.is_empty() {
        return Ok(if any_quoted {
            vec![String::new()]
        } else {
            Vec::new()
        });
    }

    // --- globbing + quote removal
    let mut out: Vec<String> = Vec::with_capacity(fields.len());
    for f in fields {
        let text: String = f
            .iter()
            .filter_map(|c| match c {
                Cell::Ch(ch, _) => Some(*ch),
                Cell::Sep => None,
            })
            .collect();
        if patterned(&f) {
            match glob(sh, &f, &text) {
                Some(matches) => out.extend(matches),
                None => out.push(text),
            }
        } else {
            out.push(text);
        }
    }
    Ok(out)
}

/// Does this field contain an *unquoted* glob metacharacter?
fn patterned(f: &[Cell]) -> bool {
    f.iter().any(|c| match c {
        Cell::Ch(ch, false) => matches!(ch, '*' | '?' | '['),
        _ => false,
    })
}

// ---------------------------------------------------------------- glob

fn glob(sh: &Shell, cells: &[Cell], text: &str) -> Option<Vec<String>> {
    // Split into directory prefix (up to the last unquoted '/') and pattern.
    let mut split_at: Option<usize> = None;
    for (i, c) in cells.iter().enumerate() {
        if let Cell::Ch('/', false) = c {
            split_at = Some(i);
        }
    }
    let (dir_str, pat_cells) = match split_at {
        Some(i) => {
            let dir: String = cells[..i]
                .iter()
                .filter_map(|c| match c {
                    Cell::Ch(ch, _) => Some(*ch),
                    _ => None,
                })
                .collect();
            (dir, cells[i + 1..].to_vec())
        }
        None => (".".to_string(), cells.to_vec()),
    };
    let dir_path = resolve_dir(sh, &dir_str);
    let rd = std::fs::read_dir(&dir_path).ok()?;
    let mut names: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches_pattern(&pat_cells, &name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return None;
    }
    names.sort();
    let prefix = if dir_str == "." {
        String::new()
    } else if dir_str.ends_with('/') {
        dir_str.clone()
    } else {
        format!("{dir_str}/")
    };
    Some(names.into_iter().map(|n| format!("{prefix}{n}")).collect())
}

fn resolve_dir(sh: &Shell, dir: &str) -> PathBuf {
    let p = Path::new(dir);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        sh.cwd.join(p)
    }
}

/// Glob matcher honouring quoted cells (quoted meta-characters are literal).
fn matches_pattern(cells: &[Cell], name: &str) -> bool {
    let target: Vec<char> = name.chars().collect();
    match_from(cells, 0, &target, 0)
}

fn match_from(pat: &[Cell], pi: usize, name: &[char], ni: usize) -> bool {
    if pi >= pat.len() {
        return ni >= name.len();
    }
    match &pat[pi] {
        Cell::Sep => match_from(pat, pi + 1, name, ni),
        Cell::Ch('*', quoted) => {
            if *quoted {
                if ni >= name.len() || name[ni] != '*' {
                    return false;
                }
                return match_from(pat, pi + 1, name, ni + 1);
            }
            // Greedy with backtracking.
            let mut k = ni;
            loop {
                if match_from(pat, pi + 1, name, k) {
                    return true;
                }
                if k >= name.len() {
                    return false;
                }
                k += 1;
            }
        }
        Cell::Ch('?', quoted) => {
            if *quoted {
                return ni < name.len()
                    && name[ni] == '?'
                    && match_from(pat, pi + 1, name, ni + 1);
            }
            ni < name.len() && match_from(pat, pi + 1, name, ni + 1)
        }
        Cell::Ch('[', false) => {
            // Find the closing bracket.
            let mut end = pi + 1;
            let mut neg = false;
            if end < pat.len() {
                if let Cell::Ch('^', false) = pat[end] {
                    neg = true;
                    end += 1;
                } else if let Cell::Ch('!', false) = pat[end] {
                    neg = true;
                    end += 1;
                }
            }
            let mut close = None;
            let mut i = end;
            while i < pat.len() {
                if let Cell::Ch(']', false) = pat[i] {
                    close = Some(i);
                    break;
                }
                i += 1;
            }
            let close = match close {
                Some(c) => c,
                None => {
                    // Unterminated '[' is literal.
                    return ni < name.len()
                        && name[ni] == '['
                        && match_from(pat, pi + 1, name, ni + 1);
                }
            };
            if ni >= name.len() {
                return false;
            }
            let ch = name[ni];
            let mut hit = false;
            let mut i = end;
            while i < close {
                let a = match &pat[i] {
                    Cell::Ch(c, _) => *c,
                    Cell::Sep => {
                        i += 1;
                        continue;
                    }
                };
                if i + 2 < close && matches!(&pat[i + 1], Cell::Ch('-', false)) {
                    if let Cell::Ch(b, _) = &pat[i + 2] {
                        if a <= ch && ch <= *b {
                            hit = true;
                        }
                        i += 3;
                        continue;
                    }
                }
                if a == ch {
                    hit = true;
                }
                i += 1;
            }
            if hit != neg {
                match_from(pat, close + 1, name, ni + 1)
            } else {
                false
            }
        }
        Cell::Ch(expected, _) => {
            ni < name.len() && name[ni] == *expected && match_from(pat, pi + 1, name, ni + 1)
        }
    }
}

// ---------------------------------------------------------------- raw text

/// Expand `$...` occurrences inside raw text (here-document bodies).
/// This is a small scanner; anything that is not a recognised expansion is
/// copied through literally.
pub fn raw(sh: &mut Shell, text: &str) -> E<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut out = String::with_capacity(text.len());
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        if i + 1 >= chars.len() {
            out.push('$');
            break;
        }
        let next = chars[i + 1];
        match next {
            '{' => {
                let mut j = i + 2;
                let mut name = String::new();
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    name.push(chars[j]);
                    j += 1;
                }
                if name.is_empty() || j >= chars.len() {
                    out.push('$');
                    i += 1;
                    continue;
                }
                if chars[j] == '}' {
                    i = j + 1;
                    out.push_str(&sh.get(&name).unwrap_or_default());
                    continue;
                }
                // ${name:-default} / ${name:=default}
                let mut assign = false;
                let mut j = j;
                if chars.get(j) == Some(&':') {
                    j += 1;
                }
                match chars.get(j) {
                    Some('-') => j += 1,
                    Some('=') => {
                        assign = true;
                        j += 1;
                    }
                    _ => {
                        // Unsupported form: copy through.
                        out.push('$');
                        i += 1;
                        continue;
                    }
                }
                let start = j;
                let mut depth = 1usize;
                while j < chars.len() {
                    if chars[j] == '{' {
                        depth += 1;
                    } else if chars[j] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                let default: String = chars[start..j.min(chars.len())].iter().collect();
                let cur = sh.get(&name).unwrap_or_default();
                let val = if cur.is_empty() { raw(sh, &default)? } else { cur };
                if assign && sh.get(&name).map(|v| v.is_empty()).unwrap_or(true) {
                    sh.set_var(&name, val.clone());
                }
                out.push_str(&val);
                i = (j + 1).min(chars.len());
            }
            '(' => {
                if chars.get(i + 2) == Some(&'(') {
                    // arithmetic
                    let mut j = i + 3;
                    let mut depth = 1usize;
                    while j < chars.len() {
                        if chars[j] == '(' {
                            depth += 1;
                        } else if chars[j] == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }
                    let expr: String = chars[i + 3..j].iter().collect();
                    let val = eval_arith(sh, &expr)?;
                    out.push_str(&val);
                    i = (j + 2).min(chars.len());
                } else {
                    // command substitution — find matching ')'
                    let mut j = i + 2;
                    let mut depth = 1usize;
                    let mut in_s = false;
                    let mut in_d = false;
                    while j < chars.len() {
                        let ch = chars[j];
                        if in_s {
                            if ch == '\'' {
                                in_s = false;
                            }
                        } else if in_d {
                            if ch == '\\' {
                                j += 1;
                            } else if ch == '"' {
                                in_d = false;
                            }
                        } else {
                            match ch {
                                '\'' => in_s = true,
                                '"' => in_d = true,
                                '(' => depth += 1,
                                ')' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        j += 1;
                    }
                    let script: String = chars[i + 2..j].iter().collect();
                    let got = crate::exec::run_capture(sh, &script).map_err(ExpandErr)?;
                    out.push_str(&strip_trailing_newlines(&got));
                    i = (j + 1).min(chars.len());
                }
            }
            '`' => {
                let mut j = i + 1;
                let mut script = String::new();
                while j < chars.len() && chars[j] != '`' {
                    if chars[j] == '\\' && chars.get(j + 1) == Some(&'`') {
                        j += 1;
                        script.push('`');
                        j += 1;
                        continue;
                    }
                    script.push(chars[j]);
                    j += 1;
                }
                let got = crate::exec::run_capture(sh, &script).map_err(ExpandErr)?;
                out.push_str(&strip_trailing_newlines(&got));
                i = (j + 1).min(chars.len());
            }
            '?' | '$' | '#' | '@' | '*' | '!' => {
                let val = match next {
                    '?' => sh.status.to_string(),
                    '$' => sh.pid.to_string(),
                    '#' => sh.positional.len().to_string(),
                    '@' | '*' => sh.positional.join(" "),
                    '!' => sh.last_bg.clone(),
                    _ => unreachable!(),
                };
                out.push_str(&val);
                i += 2;
            }
            d if d.is_ascii_digit() => {
                let idx = d as usize - '0' as usize;
                let val = if idx == 0 {
                    sh.arg0.clone()
                } else {
                    sh.positional.get(idx - 1).cloned().unwrap_or_default()
                };
                out.push_str(&val);
                i += 2;
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i + 1;
                let mut name = String::new();
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    name.push(chars[j]);
                    j += 1;
                }
                out.push_str(&sh.get(&name).unwrap_or_default());
                i = j;
            }
            other => {
                out.push('$');
                out.push(other);
                i += 2;
            }
        }
    }
    Ok(out)
}

/// Case-pattern matching: expands the pattern (keeping quote info) and
/// matches it against the already-expanded subject. No globbing/splitting.
pub fn case_match(sh: &mut Shell, pat: &Word, subject: &str) -> E<bool> {
    let mut cells: Vec<Cell> = Vec::new();
    let mut any_quoted = false;
    for (i, part) in pat.iter().enumerate() {
        push_part(sh, part, false, i, pat, &mut cells, &mut any_quoted)?;
    }
    Ok(matches_pattern(&cells, subject))
}

/// Decode C-style escapes (used by `echo -e` and `printf` formats).
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
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
            Some('\\') => out.push('\\'),
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
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn sh() -> Shell {
        let mut s = Shell::new("rsh", false);
        s.set_var("NAME".into(), "world".into());
        s.set_var("EMPTY".into(), String::new());
        s.set_var("MULTI".into(), "a b".into());
        s.set_var("STARS".into(), "*.md".into());
        s
    }

    fn first_arg(script: &crate::ast::Script) -> Word {
        match &script.nodes[0] {
            crate::ast::Node::Simple { words, .. } => words[1].clone(),
            _ => panic!("expected a simple command"),
        }
    }

    fn expand(src: &str) -> Vec<String> {
        let script = parse(src).expect("parse");
        let w = first_arg(&script);
        let mut s = sh();
        fields(&mut s, &w).expect("expand")
    }

    #[test]
    fn literals_and_variables() {
        assert_eq!(expand("echo hello"), vec!["hello"]);
        assert_eq!(expand("echo $NAME"), vec!["world"]);
        assert_eq!(expand("echo ${NAME}!"), vec!["world!"]);
        assert_eq!(expand("echo pre$NAME"), vec!["preworld"]);
    }

    #[test]
    fn field_splitting() {
        assert_eq!(expand("echo $MULTI"), vec!["a", "b"]);
        // Quoted expansion must not split.
        assert_eq!(expand("echo \"$MULTI\""), vec!["a b"]);
    }

    #[test]
    fn empty_expansions() {
        assert_eq!(expand("echo $EMPTY"), Vec::<String>::new());
        assert_eq!(expand("echo x$EMPTY"), vec!["x"]);
        assert_eq!(expand("echo \"$EMPTY\""), vec![""]);
        assert_eq!(expand("echo \"\""), vec![""]);
        assert_eq!(expand("echo $NOPE"), Vec::<String>::new());
    }

    #[test]
    fn quoting_rules() {
        assert_eq!(expand("echo 'a b'"), vec!["a b"]);
        assert_eq!(expand("echo a\\ b"), vec!["a b"]);
        assert_eq!(expand("echo \"a\\tb\""), vec!["a\\tb"]);
        assert_eq!(expand("echo \"a\\nb\""), vec!["a\\nb"]);
    }

    #[test]
    fn tilde_and_defaults() {
        let mut s = sh();
        s.set_var("HOME".into(), "/home/u".into());
        let w = first_arg(&parse("echo ~/x").unwrap());
        assert_eq!(fields(&mut s, &w).unwrap(), vec!["/home/u/x"]);

        // ${UNSET:-fallback}
        let w = first_arg(&parse("echo ${UNSET:-fallback}").unwrap());
        assert_eq!(fields(&mut s, &w).unwrap(), vec!["fallback"]);
    }

    #[test]
    fn arithmetic_expansion() {
        let mut s = sh();
        s.set_var("N".into(), "4".into());
        for (src, want) in [
            ("echo $((1+2*3))", "7"),
            ("echo $(($N+1))", "5"),
            ("echo $((${N}*2))", "8"),
        ] {
            let w = first_arg(&parse(src).unwrap());
            assert_eq!(fields(&mut s, &w).unwrap(), vec![want], "{src}");
        }
    }

    #[test]
    fn globbing_matches_and_falls_back() {
        // `*.md` must either expand to matching names or stay literal.
        let got = expand("echo *.md");
        assert!(!got.is_empty());
        for g in &got {
            if g.contains('*') {
                assert_eq!(g, "*.md", "unmatched glob must stay literal");
            }
        }
    }

    #[test]
    fn matcher_basics() {
        fn m(pat: &str, name: &str) -> bool {
            let cells: Vec<Cell> = pat
                .chars()
                .map(|c| Cell::Ch(c, false))
                .collect();
            matches_pattern(&cells, name)
        }
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.c"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("[abc]x", "bx"));
        assert!(!m("[abc]x", "dx"));
        assert!(m("a[0-9]b", "a5b"));
        assert!(m("*", "anything"));
    }
}
