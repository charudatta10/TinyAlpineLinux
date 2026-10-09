//! rsh — a small dash/ash-style POSIX shell.
//!
//! Usage:
//!
//! ```text
//! rsh -c 'command' [name [args...]]
//! rsh [script [args...]]
//! rsh                # interactive when stdin is a terminal
//! ```

mod arith;
mod ast;
mod builtins;
mod exec;
mod expand;
mod parse;
mod shell;

use exec::Io;
use shell::Shell;
use std::io::BufRead;

enum Mode {
    Command(String),
    Script(String),
    Stdin,
    Repl,
}

fn main() {
    install_signals();
    let args: Vec<String> = std::env::args().collect();

    let mut mode: Option<Mode> = None;
    let mut errexit = false;
    let mut xtrace = false;
    let mut nounset = false;
    let mut i = 1;
    let mut consumed = 0; // how many of args[i..] were flags
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-c" => {
                mode = Some(Mode::Command(
                    args.get(i + 1).cloned().unwrap_or_default(),
                ));
                i += 2;
                break;
            }
            "-l" | "--login" | "-s" | "-v" | "-f" | "-n" => {
                i += 1;
            }
            "-e" => {
                errexit = true;
                i += 1;
            }
            "-x" => {
                xtrace = true;
                i += 1;
            }
            "-u" => {
                nounset = true;
                i += 1;
            }
            "--" => {
                i += 1;
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                i += 1;
            }
            _ => break,
        }
        consumed = i;
    }
    let _ = consumed;
    let remaining: Vec<String> = args[i..].to_vec();

    if mode.is_none() {
        if let Some(file) = remaining.first() {
            if file == "-" {
                mode = Some(Mode::Stdin);
            } else {
                mode = Some(Mode::Script(file.clone()));
            }
        } else if is_tty() {
            mode = Some(Mode::Repl);
        } else {
            mode = Some(Mode::Stdin);
        }
    }

    let mut io = Io::stdio();
    match mode.unwrap() {
        Mode::Command(src) => {
            let name = remaining.first().cloned().unwrap_or_else(|| "rsh".into());
            let pos = if remaining.is_empty() {
                Vec::new()
            } else {
                remaining[1..].to_vec()
            };
            let mut sh = Shell::new(&name, false);
            apply_flags(&mut sh, errexit, xtrace, nounset);
            sh.set_positional(pos);
            run_source(&mut sh, &src, &mut io);
        }
        Mode::Script(file) => {
            let path = std::path::Path::new(&file);
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(_) => {
                    eprintln!("sh: {file}: No such file");
                    std::process::exit(127);
                }
            };
            let mut sh = Shell::new(&file, false);
            apply_flags(&mut sh, errexit, xtrace, nounset);
            let pos: Vec<String> = remaining.iter().skip(1).cloned().collect();
            sh.set_positional(pos);
            run_source(&mut sh, &text, &mut io);
        }
        Mode::Stdin => {
            let text = read_stdin();
            let mut sh = Shell::new("rsh", false);
            apply_flags(&mut sh, errexit, xtrace, nounset);
            let pos: Vec<String> = remaining.iter().skip(1).cloned().collect();
            sh.set_positional(pos);
            run_source(&mut sh, &text, &mut io);
        }
        Mode::Repl => {
            let mut sh = Shell::new("rsh", true);
            apply_flags(&mut sh, errexit, xtrace, nounset);
            repl(&mut sh, &mut io);
        }
    }
}

fn apply_flags(sh: &mut Shell, e: bool, x: bool, u: bool) {
    sh.errexit = sh.errexit || e;
    sh.xtrace = sh.xtrace || x;
    sh.nounset = sh.nounset || u;
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s
}

/// Parse and execute a complete source unit, then exit.
fn run_source(sh: &mut Shell, src: &str, io: &mut Io) -> ! {
    let code = match parse::parse(src) {
        Ok(mut script) => {
            sh.adopt(&mut script);
            exec::exec_script(sh, &script, io).unwrap_or(sh.status)
        }
        Err(er) => {
            io.error(&format!("sh: syntax error: {er}"));
            2
        }
    };
    let _ = io.stdout.flush();
    let _ = io.stderr.flush();
    std::process::exit(code);
}

fn repl(sh: &mut Shell, io: &mut Io) -> ! {
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    loop {
        let ps = if buffer.is_empty() {
            sh.get("PS1").unwrap_or_else(|| "$ ".into())
        } else {
            sh.get("PS2").unwrap_or_else(|| "> ".into())
        };
        if !ps.is_empty() {
            let _ = io.stdout.write_str(&ps);
            let _ = io.stdout.flush();
        }
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                let _ = io.stdout.flush();
                std::process::exit(sh.status);
            }
            Ok(_) => {}
            Err(_) => std::process::exit(1),
        }
        buffer.push_str(&line);
        if buffer.trim().is_empty() {
            buffer.clear();
            continue;
        }
        match parse::parse(&buffer) {
            Ok(mut script) => {
                sh.adopt(&mut script);
                if let Some(code) = exec::exec_script(sh, &script, io) {
                    let _ = io.stdout.flush();
                    std::process::exit(code);
                }
                buffer.clear();
            }
            Err(er) => {
                if is_incomplete(&er) {
                    continue; // keep reading (PS2)
                }
                io.error(&format!("sh: syntax error: {er}"));
                sh.status = 2;
                buffer.clear();
            }
        }
    }
}

/// Errors that mean "keep reading lines".
fn is_incomplete(msg: &str) -> bool {
    msg.contains("unexpected end of input")
        || msg.contains("unterminated")
        || msg.contains("expected '}'")
        || msg.contains("expected ')'")
}

fn is_tty() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::isatty(0) == 1
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            !h.is_null() && h as isize != -1 && GetConsoleMode(h, &mut mode) != 0
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn install_signals() {
    #[cfg(unix)]
    unsafe {
        extern "C" fn handler(_sig: libc::c_int) {}
        // Install real handlers (not SIG_IGN) so that exec'd children reset
        // to the default disposition and still die on Ctrl+C.
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        libc::signal(libc::SIGQUIT, handler as libc::sighandler_t);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::TRUE;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        extern "system" fn handler(_ctrl_type: u32) -> i32 {
            TRUE // consume the event: the shell survives Ctrl+C
        }
        SetConsoleCtrlHandler(Some(handler), TRUE);
    }
}
