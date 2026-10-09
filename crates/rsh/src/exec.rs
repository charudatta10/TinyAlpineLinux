//! Execution engine: IO plumbing, pipelines, redirections, subshells, loops.

use crate::ast::*;
use crate::builtins;
use crate::expand::{self, ExpandErr};
use crate::parse;
use crate::shell::{FuncDef, Shell};
use os_pipe::{PipeReader, PipeWriter};
use std::fs::{File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

// ---------------------------------------------------------------- IO

#[derive(Debug)]
pub enum Sink {
    /// The shell's own stdout.
    Stdout,
    /// The shell's own stderr.
    Stderr,
    File(File),
    Pipe(PipeWriter),
    Null,
}

#[derive(Debug)]
pub enum Source {
    Inherit,
    File(File),
    Pipe(PipeReader),
    Cursor(Cursor<Vec<u8>>),
    Null,
}

impl Sink {
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Sink::Stdout => io::stdout().lock().write_all(buf),
            Sink::Stderr => io::stderr().lock().write_all(buf),
            Sink::File(f) => f.write_all(buf),
            Sink::Pipe(p) => p.write_all(buf),
            Sink::Null => Ok(()),
        }
    }

    pub fn write_str(&mut self, s: &str) -> io::Result<()> {
        self.write_all(s.as_bytes())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Stdout => io::stdout().lock().flush(),
            Sink::Stderr => io::stderr().lock().flush(),
            Sink::File(f) => f.flush(),
            Sink::Pipe(p) => p.flush(),
            Sink::Null => Ok(()),
        }
    }

    pub fn try_clone(&self) -> io::Result<Sink> {
        Ok(match self {
            Sink::Stdout => Sink::Stdout,
            Sink::Stderr => Sink::Stderr,
            Sink::Null => Sink::Null,
            Sink::File(f) => Sink::File(f.try_clone()?),
            Sink::Pipe(p) => Sink::Pipe(p.try_clone()?),
        })
    }

    pub fn to_stdio(&self) -> io::Result<Stdio> {
        Ok(match self {
            Sink::Stdout | Sink::Stderr => Stdio::inherit(),
            Sink::Null => Stdio::null(),
            Sink::File(f) => Stdio::from(f.try_clone()?),
            Sink::Pipe(p) => Stdio::from(p.try_clone()?),
        })
    }
}

impl Source {
    pub fn try_clone(&self) -> io::Result<Source> {
        Ok(match self {
            Source::Inherit => Source::Inherit,
            Source::Null => Source::Null,
            Source::File(f) => Source::File(f.try_clone()?),
            Source::Pipe(p) => Source::Pipe(p.try_clone()?),
            Source::Cursor(c) => Source::Cursor(Cursor::new(c.get_ref().clone())),
        })
    }

    pub fn to_stdio(&self) -> io::Result<Stdio> {
        match self {
            Source::Inherit => Ok(Stdio::inherit()),
            Source::Null => Ok(Stdio::null()),
            Source::File(f) => Ok(Stdio::from(f.try_clone()?)),
            Source::Pipe(p) => Ok(Stdio::from(p.try_clone()?)),
            Source::Cursor(c) => {
                // Feed in-memory data (here-documents) through a pipe written
                // by a helper thread so large bodies cannot deadlock.
                let (r, mut w) = os_pipe::pipe()?;
                let data = c.get_ref().clone();
                std::thread::spawn(move || {
                    let _ = w.write_all(&data);
                });
                Ok(Stdio::from(r))
            }
        }
    }

    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        match self {
            Source::Inherit => {
                let mut line = String::new();
                let n = io::stdin().read_line(&mut line)?;
                if n == 0 {
                    Ok(None)
                } else {
                    Ok(Some(line))
                }
            }
            Source::Null => Ok(None),
            Source::File(f) => read_line_from(f),
            Source::Pipe(p) => read_line_from(p),
            Source::Cursor(c) => read_line_from(c),
        }
    }
}

#[derive(Debug)]
pub struct Io {
    pub stdin: Source,
    pub stdout: Sink,
    pub stderr: Sink,
}

fn read_line_from<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte)?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

impl Io {
    pub fn stdio() -> Io {
        Io {
            stdin: Source::Inherit,
            stdout: Sink::Stdout,
            stderr: Sink::Stderr,
        }
    }

    pub fn try_clone(&self) -> io::Result<Io> {
        Ok(Io {
            stdin: self.stdin.try_clone()?,
            stdout: self.stdout.try_clone()?,
            stderr: self.stderr.try_clone()?,
        })
    }

    pub fn error(&mut self, msg: &str) {
        let _ = self.stderr.write_str(msg);
        let _ = self.stderr.write_str("\n");
        let _ = self.stderr.flush();
    }
}

// ---------------------------------------------------------------- control flow

#[derive(Debug, Clone)]
pub enum Flow {
    Normal,
    Exit(i32),
    Return(i32),
    Break(u32),
    Continue(u32),
}

// ---------------------------------------------------------------- prepared commands

pub struct RRedirect {
    pub fd: i32,
    pub op: RedirOp,
    /// Target text (path, or fd number for `n>&m`).
    pub text: String,
    /// Here-document body (already expanded when required).
    pub body: String,
}

pub struct Prepared {
    pub assigns: Vec<(String, String)>,
    pub args: Vec<String>,
    pub redirs: Vec<RRedirect>,
}

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// Expand a simple command's words and redirections.
pub fn prepare(
    sh: &mut Shell,
    assigns: &[(String, Word)],
    words: &[Word],
    redirs: &[Redirect],
) -> Result<Prepared, String> {
    let mut out_assigns = Vec::new();
    for (k, w) in assigns {
        let v = expand::string(sh, w).map_err(e)?;
        out_assigns.push((k.clone(), v));
    }
    let args = expand::argv(sh, words).map_err(e)?;
    let mut out_redirs = Vec::new();
    for r in redirs {
        match r.op {
            RedirOp::HereDoc(idx, do_expand) => {
                let body = sh
                    .heredocs
                    .get(idx)
                    .map(|h| h.body.clone())
                    .unwrap_or_default();
                let body = if do_expand {
                    expand::raw(sh, &body).map_err(e)?
                } else {
                    body
                };
                out_redirs.push(RRedirect {
                    fd: r.fd,
                    op: r.op,
                    text: String::new(),
                    body,
                });
            }
            _ => {
                let text = expand::string(sh, r.word.as_ref().unwrap_or(&Vec::new())).map_err(e)?;
                out_redirs.push(RRedirect {
                    fd: r.fd,
                    op: r.op,
                    text,
                    body: String::new(),
                });
            }
        }
    }
    Ok(Prepared {
        assigns: out_assigns,
        args,
        redirs: out_redirs,
    })
}

/// Apply redirections to a *copy* of the current IO (the caller drops the
/// copy afterwards, which closes the opened descriptors again).
fn redirect_io(sh: &Shell, redirs: &[RRedirect], base: &Io) -> Result<Io, String> {
    let mut io = base.try_clone().map_err(|er| er.to_string())?;
    for r in redirs {
        match r.op {
            RedirOp::Read => {
                let path = sh.resolve(&r.text);
                let f = File::open(&path)
                    .map_err(|er| format!("{}: {}", path.display(), strip_os(&er)))?;
                io.stdin = Source::File(f);
            }
            RedirOp::Write => {
                let path = sh.resolve(&r.text);
                let f = File::create(&path)
                    .map_err(|er| format!("{}: {}", path.display(), strip_os(&er)))?;
                set_sink(&mut io, r.fd, Sink::File(f))?;
            }
            RedirOp::Append => {
                let path = sh.resolve(&r.text);
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|er| format!("{}: {}", path.display(), strip_os(&er)))?;
                set_sink(&mut io, r.fd, Sink::File(f))?;
            }
            RedirOp::Dup => {
                if r.text == "-" {
                    set_sink(&mut io, r.fd, Sink::Null)?;
                    continue;
                }
                let from: i32 = r
                    .text
                    .parse()
                    .map_err(|_| format!("bad file descriptor: '{}'", r.text))?;
                if r.fd == 0 && from != 0 {
                    return Err(format!("unsupported duplication '<&{from}'"));
                }
                if r.fd == 0 {
                    continue;
                }
                let sink = get_sink(&io, from)
                    .ok_or_else(|| format!("bad file descriptor: {from}"))?
                    .try_clone()
                    .map_err(|er| er.to_string())?;
                set_sink(&mut io, r.fd, sink)?;
            }
            RedirOp::HereDoc(_, _) => {
                io.stdin = Source::Cursor(Cursor::new(r.body.clone().into_bytes()));
            }
        }
    }
    Ok(io)
}

fn strip_os(er: &io::Error) -> String {
    let s = er.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn get_sink(io: &Io, fd: i32) -> Option<&Sink> {
    match fd {
        1 => Some(&io.stdout),
        2 => Some(&io.stderr),
        0 => None,
        _ => None,
    }
}

fn set_sink(io: &mut Io, fd: i32, sink: Sink) -> Result<(), String> {
    match fd {
        1 => io.stdout = sink,
        2 => io.stderr = sink,
        0 => return Err("can't redirect input with '>&'".into()),
        other => return Err(format!("bad file descriptor: {other}")),
    }
    Ok(())
}

// ---------------------------------------------------------------- running

/// Execute a prepared simple command.
pub fn run_prepared(sh: &mut Shell, p: Prepared, base: &Io) -> Result<(), Flow> {
    let mut io = match redirect_io(sh, &p.redirs, base) {
        Ok(io) => io,
        Err(msg) => {
            let mut io = base.try_clone().unwrap_or_else(|_| Io::stdio());
            io.error(&format!("sh: {msg}"));
            sh.status = 1;
            return Ok(());
        }
    };

    if p.args.is_empty() {
        // Redirections alone: opening/truncating is the observable effect.
        for (k, v) in p.assigns {
            sh.set_var(&k, v);
        }
        drop(io);
        sh.status = 0;
        return Ok(());
    }

    if sh.xtrace {
        let _ = io
            .stderr
            .write_str(&format!("+ {}\n", p.args.join(" ")));
        let _ = io.stderr.flush();
    }

    let name = p.args[0].clone();
    if !name.contains('/') && !name.contains('\\') && sh.functions.contains_key(&name) {
        return run_function(sh, &name, p, &mut io);
    }
    if !name.contains('/') && !name.contains('\\') && Shell::is_builtin(&name) {
        // Assignments are visible to the builtin, then restored (POSIX-like).
        let saved = save_vars(sh, &p.assigns);
        for (k, v) in &p.assigns {
            sh.set_var(k, v.clone());
        }
        let res = builtins::run(sh, &p.args, &mut io);
        restore_vars(sh, saved);
        return match res {
            Ok(code) => {
                sh.status = code;
                Ok(())
            }
            Err(f) => Err(f),
        };
    }
    run_external(sh, p, &mut io)
}

fn save_vars(sh: &Shell, assigns: &[(String, String)]) -> Vec<(String, Option<String>, bool)> {
    assigns
        .iter()
        .map(|(k, _)| {
            let was = sh.vars.get(k).cloned();
            let exported = sh.exported.contains(k);
            (k.clone(), was, exported)
        })
        .collect()
}

fn restore_vars(sh: &mut Shell, saved: Vec<(String, Option<String>, bool)>) {
    for (k, was, exported) in saved {
        match was {
            Some(v) => {
                sh.vars.insert(k.clone(), v);
            }
            None => {
                sh.vars.remove(&k);
            }
        }
        if exported {
            sh.exported.insert(k);
        }
    }
}

fn run_function(sh: &mut Shell, name: &str, p: Prepared, io: &mut Io) -> Result<(), Flow> {
    let def = match sh.functions.get(name) {
        Some(d) => d.clone(),
        None => {
            io.error(&format!("sh: {name}: function not found"));
            sh.status = 127;
            return Ok(());
        }
    };
    if sh.depth >= 128 {
        io.error("sh: function call nesting too deep");
        sh.status = 1;
        return Ok(());
    }
    let saved_pos = std::mem::replace(&mut sh.positional, p.args[1..].to_vec());
    sh.scopes.push(Vec::new());
    sh.depth += 1;

    let result = exec_list(sh, &def.nodes, io, &mut Vec::new());
    sh.depth -= 1;
    if let Some(scope) = sh.scopes.pop() {
        for (name, prev) in scope {
            match prev {
                Some(v) => {
                    sh.vars.insert(name.clone(), v);
                }
                None => {
                    sh.vars.remove(&name);
                    sh.exported.remove(&name);
                }
            }
        }
    }
    sh.positional = saved_pos;
    match result {
        Ok(()) => Ok(()),
        Err(Flow::Return(n)) => {
            sh.status = n;
            Ok(())
        }
        Err(f) => Err(f),
    }
}

pub(crate) fn run_external(sh: &mut Shell, p: Prepared, io: &mut Io) -> Result<(), Flow> {
    let name = p.args[0].clone();
    let prog = if name.contains('/') || name.contains('\\') {
        let path = sh.resolve(&name);
        path.to_string_lossy().into_owned()
    } else {
        match find_in_path(sh, &name) {
            Some(found) => found,
            None => {
                io.error(&format!("sh: {name}: command not found"));
                sh.status = 127;
                return Ok(());
            }
        }
    };

    let mut cmd = Command::new(&prog);
    cmd.args(&p.args[1..]);
    cmd.current_dir(&sh.cwd);
    cmd.env_clear();
    for (k, v) in sh.child_env() {
        cmd.env(k, v);
    }
    for (k, v) in &p.assigns {
        cmd.env(k, v);
    }
    cmd.stdin(io.stdin.to_stdio().map_err(|er| {
        io.error(&format!("sh: {name}: {}", strip_os(&er)));
        Flow::Exit(1)
    })?);
    cmd.stdout(io.stdout.to_stdio().map_err(|er| {
        io.error(&format!("sh: {name}: {}", strip_os(&er)));
        Flow::Exit(1)
    })?);
    cmd.stderr(io.stderr.to_stdio().map_err(|er| {
        io.error(&format!("sh: {name}: {}", strip_os(&er)));
        Flow::Exit(1)
    })?);
    restore_sigpipe(&mut cmd);

    match cmd.spawn() {
        Ok(mut child) => match child.wait() {
            Ok(st) => {
                sh.status = exit_code(&st);
                Ok(())
            }
            Err(er) => {
                io.error(&format!("sh: {name}: {}", strip_os(&er)));
                sh.status = 1;
                Ok(())
            }
        },
        Err(er) => {
            let code = match er.kind() {
                io::ErrorKind::NotFound => 127,
                _ => 126,
            };
            io.error(&format!(
                "sh: {name}: {}",
                if code == 127 {
                    "command not found".to_string()
                } else {
                    strip_os(&er)
                }
            ));
            sh.status = code;
            Ok(())
        }
    }
}

/// Children should get the default SIGPIPE disposition, not Rust's ignore.
fn restore_sigpipe(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

pub fn exit_code(st: &ExitStatus) -> i32 {
    if let Some(code) = st.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return 128 + sig;
        }
    }
    1
}

pub fn find_in_path(sh: &Shell, name: &str) -> Option<String> {
    let path = sh.get("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ';' } else { ':' };
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
    for dir in path.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let base = sh.resolve(dir).join(name);
        if base.is_file() {
            return Some(base.to_string_lossy().into_owned());
        }
        for ext in &exts {
            let cand = base.with_file_name(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

// ---------------------------------------------------------------- nodes

pub fn exec_script(sh: &mut Shell, script: &Script, io: &mut Io) -> Option<i32> {
    let mut locals = Vec::new();
    match exec_list(sh, &script.nodes, io, &mut locals) {
        Ok(()) => None,
        Err(Flow::Exit(code)) => Some(code),
        Err(Flow::Return(code)) => {
            // `return` outside a function: set the status and keep going.
            sh.status = code;
            None
        }
        Err(_) => Some(sh.status),
    }
}

pub fn exec_list(
    sh: &mut Shell,
    nodes: &[Node],
    io: &mut Io,
    _locals: &mut Vec<String>,
) -> Result<(), Flow> {
    for n in nodes {
        exec_node(sh, n, io)?;
        if sh.errexit
            && sh.cond_depth == 0
            && sh.status != 0
            && !matches!(n, Node::Pipeline { negated: true, .. })
        {
            return Err(Flow::Exit(sh.status));
        }
    }
    Ok(())
}

pub fn exec_node(sh: &mut Shell, node: &Node, base: &mut Io) -> Result<(), Flow> {
    match node {
        Node::Nop => {
            sh.status = 0;
            Ok(())
        }
        Node::Simple {
            assigns,
            words,
            redirs,
        } => {
            match prepare(sh, assigns, words, redirs) {
                Ok(p) => run_prepared(sh, p, base),
                Err(msg) => {
                    base.error(&format!("sh: {msg}"));
                    sh.status = 2;
                    Ok(())
                }
            }
        }
        Node::RedirOnly { redirs } => {
            let prepared = Prepared {
                assigns: Vec::new(),
                args: Vec::new(),
                redirs: redirs
                    .iter()
                    .map(|r| RRedirect {
                        fd: r.fd,
                        op: r.op,
                        text: r
                            .word
                            .as_ref()
                            .map(|w| expand::string(sh, w).unwrap_or_default())
                            .unwrap_or_default(),
                        body: String::new(),
                    })
                    .collect(),
            };
            match redirect_io(sh, &prepared.redirs, base) {
                Ok(io) => {
                    drop(io);
                    sh.status = 0;
                }
                Err(msg) => {
                    base.error(&format!("sh: {msg}"));
                    sh.status = 1;
                }
            }
            Ok(())
        }
        Node::Pipeline { negated, stages } => exec_pipeline(sh, *negated, stages, base),
        Node::And(a, b) => {
            sh.cond_depth += 1;
            let r = exec_node(sh, a, base);
            sh.cond_depth -= 1;
            r?;
            if sh.status == 0 {
                exec_node(sh, b, base)
            } else {
                Ok(())
            }
        }
        Node::Or(a, b) => {
            sh.cond_depth += 1;
            let r = exec_node(sh, a, base);
            sh.cond_depth -= 1;
            r?;
            if sh.status != 0 {
                exec_node(sh, b, base)
            } else {
                Ok(())
            }
        }
        Node::Seq(list) => {
            let mut locals = Vec::new();
            exec_list(sh, list, base, &mut locals)
        }
        Node::Background(inner) => exec_background(sh, inner, base),
        Node::If {
            cond,
            then_body,
            else_body,
        } => {
            sh.cond_depth += 1;
            let r = exec_node(sh, cond, base);
            sh.cond_depth -= 1;
            r?;
            let mut locals = Vec::new();
            if sh.status == 0 {
                exec_list(sh, then_body, base, &mut locals)
            } else if !else_body.is_empty() {
                exec_list(sh, else_body, base, &mut locals)
            } else {
                sh.status = 0;
                Ok(())
            }
        }
        Node::For { var, items, body } => {
            let list = match expand::argv(sh, items) {
                Ok(l) => l,
                Err(er) => {
                    base.error(&format!("sh: {}", er.0));
                    sh.status = 2;
                    return Ok(());
                }
            };
            let mut locals = Vec::new();
            for item in list {
                sh.set_var(var, item);
                match exec_list(sh, body, base, &mut locals) {
                    Ok(()) => {}
                    Err(Flow::Continue(n)) => {
                        if n > 1 {
                            return Err(Flow::Continue(n - 1));
                        }
                        // Body already aborted at `continue`: next item.
                    }
                    Err(Flow::Break(n)) => {
                        if n > 1 {
                            return Err(Flow::Break(n - 1));
                        }
                        break;
                    }
                    Err(f) => return Err(f),
                }
            }
            sh.status = 0;
            Ok(())
        }
        Node::While { cond, body, until } => {
            let mut locals = Vec::new();
            let mut last = 0;
            loop {
                sh.cond_depth += 1;
                let r = exec_node(sh, cond, base);
                sh.cond_depth -= 1;
                r?;
                let truth = sh.status == 0;
                if truth == *until {
                    break;
                }
                match exec_list(sh, body, base, &mut locals) {
                    Ok(()) => {
                        last = sh.status;
                    }
                    Err(Flow::Continue(n)) => {
                        if n > 1 {
                            return Err(Flow::Continue(n - 1));
                        }
                        // Body aborted at `continue`: re-check the condition.
                    }
                    Err(Flow::Break(n)) => {
                        if n > 1 {
                            return Err(Flow::Break(n - 1));
                        }
                        sh.status = last;
                        return Ok(());
                    }
                    Err(f) => return Err(f),
                }
            }
            sh.status = last;
            Ok(())
        }
        Node::Case { word, arms } => {
            let subject = match expand::string(sh, word) {
                Ok(s) => s,
                Err(er) => {
                    base.error(&format!("sh: {}", er.0));
                    sh.status = 2;
                    return Ok(());
                }
            };
            for arm in arms {
                for pat in &arm.patterns {
                    let m = match expand::case_match(sh, pat, &subject) {
                        Ok(m) => m,
                        Err(er) => {
                            base.error(&format!("sh: {}", er.0));
                            false
                        }
                    };
                    if m {
                        let mut locals = Vec::new();
                        return exec_list(sh, &arm.body, base, &mut locals);
                    }
                }
            }
            sh.status = 0;
            Ok(())
        }
        Node::Subshell(body) => {
            let mut sub = sh.clone();
            let mut io = base.try_clone().map_err(|er| {
                base.error(&format!("sh: {}", strip_os(&er)));
                Flow::Normal
            })?;
            let mut locals = Vec::new();
            let r = exec_list(&mut sub, body, &mut io, &mut locals);
            sh.status = sub.status;
            r
        }
        Node::Group(body) => {
            let mut locals = Vec::new();
            exec_list(sh, body, base, &mut locals)
        }
        Node::Func { name, body } => {
            sh.functions
                .insert(name.clone(), FuncDef { nodes: body.clone() });
            sh.status = 0;
            Ok(())
        }
        Node::Redirected { inner, redirs } => {
            let resolved = resolve_redirs(sh, redirs);
            match resolved {
                Ok(rr) => match redirect_io(sh, &rr, base) {
                    Ok(mut io) => exec_node(sh, inner, &mut io),
                    Err(msg) => {
                        base.error(&format!("sh: {msg}"));
                        sh.status = 1;
                        Ok(())
                    }
                },
                Err(msg) => {
                    base.error(&format!("sh: {msg}"));
                    sh.status = 2;
                    Ok(())
                }
            }
        }
    }
}

fn resolve_redirs(sh: &mut Shell, redirs: &[Redirect]) -> Result<Vec<RRedirect>, String> {
    let mut out = Vec::new();
    for r in redirs {
        match r.op {
            RedirOp::HereDoc(idx, do_expand) => {
                let body = sh
                    .heredocs
                    .get(idx)
                    .map(|h| h.body.clone())
                    .unwrap_or_default();
                let body = if do_expand {
                    expand::raw(sh, &body).map_err(e)?
                } else {
                    body
                };
                out.push(RRedirect {
                    fd: r.fd,
                    op: r.op,
                    text: String::new(),
                    body,
                });
            }
            _ => {
                let text = expand::string(sh, r.word.as_ref().unwrap_or(&Vec::new())).map_err(e)?;
                out.push(RRedirect {
                    fd: r.fd,
                    op: r.op,
                    text,
                    body: String::new(),
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- pipelines

enum StagePlan {
    External(Prepared),
    InProcess(Prepared),
    Node(Node),
}

fn plan_stage(sh: &mut Shell, stage: &Node) -> Result<StagePlan, String> {
    match stage {
        Node::Simple {
            assigns,
            words,
            redirs,
        } => {
            let p = prepare(sh, assigns, words, redirs)?;
            if p.args.is_empty() {
                return Ok(StagePlan::InProcess(p));
            }
            let name = &p.args[0];
            let pathlike = name.contains('/') || name.contains('\\');
            if !pathlike && (Shell::is_builtin(name) || sh.functions.contains_key(name)) {
                Ok(StagePlan::InProcess(p))
            } else {
                Ok(StagePlan::External(p))
            }
        }
        other => Ok(StagePlan::Node(other.clone())),
    }
}

fn exec_pipeline(sh: &mut Shell, negated: bool, stages: &[Node], io: &mut Io) -> Result<(), Flow> {
    if stages.len() == 1 && !negated {
        return exec_node(sh, &stages[0], io);
    }
    let n = stages.len();
    let mut readers: Vec<Option<PipeReader>> = Vec::new();
    let mut writers: Vec<Option<PipeWriter>> = Vec::new();
    for _ in 0..n.saturating_sub(1) {
        let (r, w) = os_pipe::pipe().map_err(|er| {
            io.error(&format!("sh: pipe: {}", strip_os(&er)));
            Flow::Normal
        })?;
        readers.push(Some(r));
        writers.push(Some(w));
    }

    let mut children: Vec<(usize, Child)> = Vec::new();
    let mut threads: Vec<(usize, JoinHandle<i32>)> = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        let mut sio = Io {
            stdin: if i == 0 {
                io.stdin.try_clone().map_err(|er| {
                    io.error(&format!("sh: {}", strip_os(&er)));
                    Flow::Normal
                })?
            } else {
                Source::Pipe(readers[i - 1].take().unwrap())
            },
            stdout: if i == n - 1 {
                io.stdout.try_clone().map_err(|er| {
                    io.error(&format!("sh: {}", strip_os(&er)));
                    Flow::Normal
                })?
            } else {
                Sink::Pipe(writers[i].take().unwrap())
            },
            stderr: io.stderr.try_clone().map_err(|er| {
                io.error(&format!("sh: {}", strip_os(&er)));
                Flow::Normal
            })?,
        };

        let plan = match plan_stage(sh, stage) {
            Ok(p) => p,
            Err(msg) => {
                io.error(&format!("sh: {msg}"));
                sh.status = 2;
                continue;
            }
        };
        match plan {
            StagePlan::External(p) => {
                // Spawn without waiting so no stage can deadlock the pipe.
                let mut sub = sh.clone();
                match spawn_child(&mut sub, p, &mut sio) {
                    Ok(child) => children.push((i, child)),
                    Err(code) => {
                        sh.status = code;
                        sio.error(&format!("sh: pipeline stage {i} failed"));
                    }
                }
            }
            StagePlan::InProcess(p) => {
                let mut sub = sh.clone();
                let h = std::thread::spawn(move || {
                    match run_prepared(&mut sub, p, &mut sio) {
                        Ok(()) => sub.status,
                        Err(Flow::Exit(c)) => c,
                        Err(Flow::Return(c)) => c,
                        Err(_) => sub.status,
                    }
                });
                threads.push((i, h));
            }
            StagePlan::Node(node) => {
                let mut sub = sh.clone();
                let h = std::thread::spawn(move || {
                    let mut locals = Vec::new();
                    match exec_list(&mut sub, std::slice::from_ref(&node), &mut sio, &mut locals) {
                        Ok(()) => sub.status,
                        Err(Flow::Exit(c)) => c,
                        Err(Flow::Return(c)) => c,
                        Err(_) => sub.status,
                    }
                });
                threads.push((i, h));
            }
        }
    }

    let mut status = 0;
    for (i, mut child) in children {
        match child.wait() {
            Ok(st) => {
                if i == n - 1 {
                    status = exit_code(&st);
                }
            }
            Err(_) => {
                if i == n - 1 {
                    status = 1;
                }
            }
        }
    }
    for (i, h) in threads {
        let s = h.join().unwrap_or(1);
        if i == n - 1 {
            status = s;
        }
    }
    sh.status = if negated {
        if status == 0 {
            1
        } else {
            0
        }
    } else {
        status
    };
    Ok(())
}

/// Spawn a child for a prepared command without waiting (used by pipelines
/// and background jobs so `$!` can report the pid).
pub fn spawn_child(sh: &mut Shell, p: Prepared, io: &mut Io) -> Result<Child, i32> {
    let mut child_io = match redirect_io(sh, &p.redirs, io) {
        Ok(v) => v,
        Err(msg) => {
            io.error(&format!("sh: {msg}"));
            return Err(1);
        }
    };
    if p.args.is_empty() {
        for (k, v) in p.assigns {
            sh.set_var(&k, v);
        }
        // Should not be reached for arg-less commands.
        return Err(0);
    }
    let name = p.args[0].clone();
    let prog = if name.contains('/') || name.contains('\\') {
        sh.resolve(&name).to_string_lossy().into_owned()
    } else {
        match find_in_path(sh, &name) {
            Some(f) => f,
            None => {
                io.error(&format!("sh: {name}: command not found"));
                return Err(127);
            }
        }
    };
    let mut cmd = Command::new(prog);
    cmd.args(&p.args[1..]);
    cmd.current_dir(&sh.cwd);
    cmd.env_clear();
    for (k, v) in sh.child_env() {
        cmd.env(k, v);
    }
    for (k, v) in &p.assigns {
        cmd.env(k, v);
    }
    let stdin = match child_io.stdin.to_stdio() {
        Ok(s) => s,
        Err(er) => {
            io.error(&format!("sh: {name}: {}", strip_os(&er)));
            return Err(1);
        }
    };
    let stdout = match child_io.stdout.to_stdio() {
        Ok(s) => s,
        Err(er) => {
            io.error(&format!("sh: {name}: {}", strip_os(&er)));
            return Err(1);
        }
    };
    let stderr = match child_io.stderr.to_stdio() {
        Ok(s) => s,
        Err(er) => {
            io.error(&format!("sh: {name}: {}", strip_os(&er)));
            return Err(1);
        }
    };
    cmd.stdin(stdin).stdout(stdout).stderr(stderr);
    restore_sigpipe(&mut cmd);
    match cmd.spawn() {
        Ok(c) => Ok(c),
        Err(er) => {
            let code = if er.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            io.error(&format!(
                "sh: {name}: {}",
                if code == 127 {
                    "command not found".to_string()
                } else {
                    strip_os(&er)
                }
            ));
            Err(code)
        }
    }
}

// ---------------------------------------------------------------- background

enum Job {
    Proc(Child),
    Thread(JoinHandle<i32>),
}

fn jobs() -> &'static Mutex<Vec<Job>> {
    static JOBS: OnceLock<Mutex<Vec<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

fn exec_background(sh: &mut Shell, inner: &Node, io: &mut Io) -> Result<(), Flow> {
    if let Node::Simple {
        assigns,
        words,
        redirs,
    } = inner
    {
        match prepare(sh, assigns, words, redirs) {
            Ok(p) if !p.args.is_empty() => {
                let name = p.args[0].clone();
                let pathlike = name.contains('/') || name.contains('\\');
                if !pathlike && (Shell::is_builtin(&name) || sh.functions.contains_key(&name)) {
                    // Builtins/functions run on a thread with their own state.
                    let mut sub = sh.clone();
                    let sio = io.try_clone().map_err(|er| {
                        io.error(&format!("sh: {}", strip_os(&er)));
                        Flow::Normal
                    })?;
                    let h = std::thread::spawn(move || {
                        let mut sio = sio;
                        match run_prepared(&mut sub, p, &mut sio) {
                            Ok(()) => sub.status,
                            Err(Flow::Exit(c)) => c,
                            Err(_) => sub.status,
                        }
                    });
                    if let Ok(mut j) = jobs().lock() {
                        j.push(Job::Thread(h));
                    }
                    sh.status = 0;
                    return Ok(());
                }
                let mut sio = io.try_clone().map_err(|er| {
                    io.error(&format!("sh: {}", strip_os(&er)));
                    Flow::Normal
                })?;
                let mut sub = sh.clone();
                match spawn_child(&mut sub, p, &mut sio) {
                    Ok(child) => {
                        sh.last_bg = child.id().to_string();
                        if let Ok(mut j) = jobs().lock() {
                            j.push(Job::Proc(child));
                        }
                        sh.status = 0;
                        Ok(())
                    }
                    Err(code) => {
                        sh.status = code;
                        Ok(())
                    }
                }
            }
            Ok(_p) => {
                sh.status = 0;
                Ok(())
            }
            Err(msg) => {
                io.error(&format!("sh: {msg}"));
                sh.status = 2;
                Ok(())
            }
        }
    } else {
        let mut sub = sh.clone();
        let node = inner.clone();
        let sio = io.try_clone().map_err(|er| {
            io.error(&format!("sh: {}", strip_os(&er)));
            Flow::Normal
        })?;
        let h = std::thread::spawn(move || {
            let mut sio = sio;
            let mut locals = Vec::new();
            match exec_list(&mut sub, std::slice::from_ref(&node), &mut sio, &mut locals) {
                Ok(()) => sub.status,
                Err(Flow::Exit(c)) => c,
                Err(_) => sub.status,
            }
        });
        if let Ok(mut j) = jobs().lock() {
            j.push(Job::Thread(h));
        }
        sh.status = 0;
        Ok(())
    }
}

/// Wait for every background job (the `wait` builtin).
pub fn wait_all() -> i32 {
    let drained: Vec<Job> = match jobs().lock() {
        Ok(mut j) => std::mem::take(&mut *j),
        Err(_) => Vec::new(),
    };
    let mut last = 0;
    for job in drained {
        match job {
            Job::Proc(mut c) => {
                if let Ok(st) = c.wait() {
                    last = exit_code(&st);
                }
            }
            Job::Thread(h) => {
                if let Ok(v) = h.join() {
                    last = v;
                }
            }
        }
    }
    last
}

// ---------------------------------------------------------------- command substitution

/// Run `script` with stdout captured, returning its output with trailing
/// newlines stripped by the caller. Runs in a subshell (cloned state).
pub fn run_capture(sh: &mut Shell, script: &str) -> Result<String, String> {
    if sh.depth >= 128 {
        return Err("command substitution nesting too deep".into());
    }
    let mut parsed = parse::parse(script).map_err(|er| format!("syntax error: {er}"))?;
    sh.adopt(&mut parsed);
    let (r, w) = os_pipe::pipe().map_err(|er| format!("pipe: {}", strip_os(&er)))?;
    let mut sub = sh.clone();
    sub.depth += 1;
    let mut io = Io {
        stdin: Source::Inherit,
        stdout: Sink::Pipe(w),
        stderr: Sink::Stderr,
    };
    let mut locals = Vec::new();
    let _ = exec_list(&mut sub, &parsed.nodes, &mut io, &mut locals);
    sh.status = sub.status;
    drop(io);
    let mut out = String::new();
    let mut reader = r;
    reader
        .read_to_string(&mut out)
        .map_err(|er| format!("read: {}", strip_os(&er)))?;
    Ok(out)
}

// ---------------------------------------------------------------- misc helpers

/// Monotonic counter used for unique temp names if needed later.
pub static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn expand_err_msg(er: &ExpandErr) -> String {
    er.0.clone()
}
