//! Windows pseudo-terminal backend using ConPTY (CreatePseudoConsole).
//!
//! Available since Windows 10 1809. Uses the `windows-sys` bindings directly.

use std::io;
use std::ptr;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HeapFree};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use super::{Ctl, Pty, Reader, Writer};

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn step(name: &str, e: io::Error) -> io::Error {
    io::Error::other(format!("{name}: {e}"))
}

/// Build a CreateProcessW environment block from the current environment
/// plus the given overrides.
fn build_env_block(overrides: &[(String, String)]) -> Vec<u16> {
    let mut entries: Vec<String> = Vec::new();
    unsafe {
        let raw = windows_sys::Win32::System::Environment::GetEnvironmentStringsW();
        if !raw.is_null() {
            let mut p = raw;
            while *p != 0 {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(p, len);
                entries.push(String::from_utf16_lossy(slice));
                p = p.add(len + 1);
            }
            windows_sys::Win32::System::Environment::FreeEnvironmentStringsW(raw);
        }
    }
    for (k, v) in overrides {
        let prefix = format!("{k}=").to_lowercase();
        entries.retain(|e| !e.to_lowercase().starts_with(&prefix));
        entries.push(format!("{k}={v}"));
    }
    entries.sort_by_key(|e| e.to_lowercase());
    let mut block = Vec::new();
    for e in &entries {
        block.extend(e.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

fn coord(cols: u16, rows: u16) -> COORD {
    COORD {
        X: cols as i16,
        Y: rows as i16,
    }
}

/// Raw handles are stored as `isize` so the halves are `Send`.
pub struct PipeReader(isize);

impl Drop for PipeReader {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}

impl Reader for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut avail: u32 = 0;
        unsafe {
            PeekNamedPipe(
                self.0 as HANDLE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut avail,
                ptr::null_mut(),
            );
        }
        if avail == 0 {
            return Ok(0);
        }
        let want = (avail as usize).min(buf.len());
        let mut got: u32 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                self.0 as HANDLE,
                buf.as_mut_ptr(),
                want as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(got as usize)
    }
}

pub struct PipeWriter(isize);

impl Drop for PipeWriter {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}

impl Writer for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written: u32 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                self.0 as HANDLE,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(written as usize)
    }
}

pub struct ConPtyCtl {
    hpc: HPCON,
    process: isize,
    killed: bool,
}

impl Drop for ConPtyCtl {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
            CloseHandle(self.process as HANDLE);
        }
    }
}

impl Ctl for ConPtyCtl {
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let hr = unsafe { ResizePseudoConsole(self.hpc, coord(cols, rows)) };
        if hr != 0 {
            return Err(last_error());
        }
        Ok(())
    }
    fn running(&mut self) -> bool {
        if self.killed {
            return false;
        }
        unsafe { WaitForSingleObject(self.process as HANDLE, 0) == WAIT_TIMEOUT }
    }
    fn kill(&mut self) -> io::Result<()> {
        self.killed = true;
        unsafe {
            if TerminateProcess(self.process as HANDLE, 1) == 0 {
                return Err(last_error());
            }
        }
        Ok(())
    }
}

pub fn spawn(
    argv: &[String],
    envs: &[(String, String)],
    cwd: Option<&str>,
    cols: u16,
    rows: u16,
) -> io::Result<Pty> {
    // Two pipes: input (we write / conpty reads), output (conpty writes / we read).
    let mut input_read: HANDLE = ptr::null_mut();
    let mut input_write: HANDLE = ptr::null_mut();
    let mut output_read: HANDLE = ptr::null_mut();
    let mut output_write: HANDLE = ptr::null_mut();
    unsafe {
        if CreatePipe(&mut input_read, &mut input_write, ptr::null(), 0) == 0 {
            return Err(step("CreatePipe(input)", last_error()));
        }
        if CreatePipe(&mut output_read, &mut output_write, ptr::null(), 0) == 0 {
            return Err(step("CreatePipe(output)", last_error()));
        }

        let mut hpc: HPCON = 0;
        let hr = CreatePseudoConsole(coord(cols, rows), input_read, output_write, 0, &mut hpc);
        if hr != 0 {
            return Err(io::Error::other(format!("CreatePseudoConsole hr={hr:#x}")));
        }
        // NOTE: input_read / output_write (the ends given to ConPTY) must
        // stay open until after CreateProcessW; they are closed below.

        // STARTUPINFOEXW with the PSEUDOCONSOLE attribute.
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // STARTF_USESTDHANDLES with all-null std handles stops the child from
        // inheriting our console handles (as alacritty does); its stdio then
        // resolves to the pseudoconsole.
        si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

        let mut size: usize = 0;
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        let attr_list = HeapAlloc(GetProcessHeap(), 0, size);
        if attr_list.is_null() {
            return Err(step("HeapAlloc", last_error()));
        }
        si.lpAttributeList = attr_list as *mut _;
        if InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &mut size) == 0 {
            return Err(step("InitializeProcThreadAttributeList", last_error()));
        }
        if UpdateProcThreadAttribute(
            si.lpAttributeList,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null(),
        ) == 0
        {
            return Err(step("UpdateProcThreadAttribute", last_error()));
        }

        // Command line: quoted argv joined.
        let cmdline = argv
            .iter()
            .map(|a| {
                if a.contains(' ') || a.contains('"') {
                    format!("\"{}\"", a.replace('"', "\\\""))
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

        let cwd_w: Option<Vec<u16>> =
            cwd.map(|c| c.encode_utf16().chain(std::iter::once(0)).collect());

        // Full environment block: a copy of the current environment with
        // the overrides spliced in, sorted case-insensitively as Windows
        // expects, NUL-separated with a double NUL.
        let env_block = build_env_block(envs);

        let mut proc_info = std::mem::zeroed();
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
        let ok = CreateProcessW(
            ptr::null(),
            cmdline_w.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            flags,
            env_block.as_ptr() as *const _,
            cwd_w
                .as_ref()
                .map(|w| w.as_ptr())
                .unwrap_or(ptr::null()),
            &si.StartupInfo,
            &mut proc_info,
        );
        if ok == 0 {
            let e = last_error();
            return Err(io::Error::other(format!("CreateProcessW: {e} (cmdline: {cmdline})")));
        }

        // Free the ends given to the pseudoconsole now that the client is up.
        CloseHandle(input_read);
        CloseHandle(output_write);

        ResumeThread(proc_info.hThread);
        CloseHandle(proc_info.hThread);

        DeleteProcThreadAttributeList(si.lpAttributeList);
        HeapFree(GetProcessHeap(), 0, attr_list);

        Ok(Pty {
            reader: Box::new(PipeReader(output_read as isize)),
            writer: Box::new(PipeWriter(input_write as isize)),
            ctl: Arc::new(Mutex::new(Box::new(ConPtyCtl {
                hpc,
                process: proc_info.hProcess as isize,
                killed: false,
            }) as Box<dyn Ctl>)),
        })
    }
}
