//! Cross-platform pseudo-terminal: ConPTY on Windows, posix PTY elsewhere.
//!
//! The pty is split into three halves so the emulator can block on reads in
//! one thread while the UI thread sends keystrokes and resizes:
//! a reader, a writer, and a control handle (resize / running / kill).

use std::io;
use std::sync::{Arc, Mutex};

pub trait Reader: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

pub trait Writer: Send {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
}

pub trait Ctl: Send {
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
    fn running(&mut self) -> bool;
    fn kill(&mut self) -> io::Result<()>;
}

pub struct Pty {
    pub reader: Box<dyn Reader>,
    pub writer: Box<dyn Writer>,
    pub ctl: Arc<Mutex<Box<dyn Ctl>>>,
}

#[cfg(unix)]
#[path = "pty_unix.rs"]
mod backend;

#[cfg(windows)]
#[path = "pty_windows.rs"]
mod backend;

/// Spawn `argv` on a new pseudo-terminal of the given size, with `cwd` as
/// working directory and `envs` as extra environment variables.
pub fn spawn(
    argv: &[String],
    envs: &[(String, String)],
    cwd: Option<&str>,
    cols: u16,
    rows: u16,
) -> io::Result<Pty> {
    backend::spawn(argv, envs, cwd, cols, rows)
}
