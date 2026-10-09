//! POSIX pseudo-terminal backend (Linux/macOS/BSD).
//!
//! Linux uses `posix_openpt`; other unixes use `openpty(3)`.

use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

use super::{Ctl, Pty, Reader, Writer};

#[repr(C)]
#[derive(Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

use libc::{TIOCSCTTY, TIOCSWINSZ};

pub struct FdIo(File);

impl Reader for FdIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&mut self.0).read(buf)
    }
}

impl Writer for FdIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&mut self.0).write(buf)
    }
}

pub struct FdCtl {
    master: File,
    child: Child,
}

impl Ctl for FdCtl {
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let r = unsafe { libc::ioctl(self.master.as_raw_fd(), TIOCSWINSZ, &ws as *const Winsize) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

/// Open a master/slave pty pair with the given window size.
#[cfg(target_os = "linux")]
fn open_master(rows: u16, cols: u16) -> io::Result<(File, libc::c_int)> {
    unsafe extern "C" {
        fn posix_openpt(flags: libc::c_int) -> libc::c_int;
        fn grantpt(fd: libc::c_int) -> libc::c_int;
        fn unlockpt(fd: libc::c_int) -> libc::c_int;
        fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: usize) -> libc::c_int;
    }
    unsafe {
        let master = posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        let fail = |e| {
            libc::close(master);
            Err(e)
        };
        if grantpt(master) != 0 || unlockpt(master) != 0 {
            return fail(io::Error::last_os_error());
        }
        let mut buf = [0i8; 128];
        if ptsname_r(master, buf.as_mut_ptr(), buf.len()) != 0 {
            return fail(io::Error::last_os_error());
        }
        let bytes: Vec<u8> = buf.iter().map(|&c| c as u8).take_while(|&b| b != 0).collect();
        let name = std::ffi::CString::new(bytes).unwrap();
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if slave < 0 {
            return fail(io::Error::last_os_error());
        }
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = libc::ioctl(master, TIOCSWINSZ, &ws as *const Winsize);
        Ok((File::from_raw_fd(master), slave))
    }
}

#[cfg(not(target_os = "linux"))]
fn open_master(rows: u16, cols: u16) -> io::Result<(File, libc::c_int)> {
    use std::os::fd::RawFd;
    #[link(name = "util")]
    extern "C" {
        fn openpty(
            amaster: *mut RawFd,
            aslave: *mut RawFd,
            name: *mut libc::c_char,
            termp: *const libc::c_void,
            winp: *const Winsize,
        ) -> libc::c_int;
    }
    unsafe {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        if openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws as *const Winsize,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((File::from_raw_fd(master), slave))
    }
}

pub fn spawn(
    argv: &[String],
    envs: &[(String, String)],
    cwd: Option<&str>,
    cols: u16,
    rows: u16,
) -> io::Result<Pty> {
    let (master, slave_fd) = open_master(rows, cols)?;

    // Three independent fds, one for each stdio slot, each owned by its Stdio.
    let slave0 = unsafe { File::from_raw_fd(slave_fd) };
    let slave1 = slave0.try_clone()?;
    let slave2 = slave0.try_clone()?;

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    unsafe {
        cmd.stdin(Stdio::from(slave0));
        cmd.stdout(Stdio::from(slave1));
        cmd.stderr(Stdio::from(slave2));
        let slave_tty = slave_fd;
        cmd.pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave_tty, TIOCSCTTY, 0);
            Ok(())
        });
    }

    // The child (or its stdio owners) own the slave fd; nothing to close here.
    let child = cmd.spawn()?;

    // The master fd is dup'd per half; each half owns and closes its own fd.
    let reader = FdIo(master.try_clone()?);
    let writer = FdIo(master.try_clone()?);
    let ctl = FdCtl { master, child };
    Ok(Pty {
        reader: Box::new(reader),
        writer: Box::new(writer),
        ctl: Arc::new(Mutex::new(Box::new(ctl) as Box<dyn Ctl>)),
    })
}
