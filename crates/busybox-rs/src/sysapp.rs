//! System applets: env, sleep, date, uname, mount, umount.

use crate::helpers::error;
use crate::timefmt;
use std::process::Command;

// ---------------------------------------------------------------- env

pub fn env(args: &[String]) -> i32 {
    let mut clear = false;
    let mut i = 0;
    let mut assigns: Vec<(String, String)> = Vec::new();
    while i < args.len() {
        let a = &args[i];
        if a == "-i" || a == "-" {
            clear = true;
            i += 1;
            continue;
        }
        if a == "-u" {
            i += 1;
            if let Some(name) = args.get(i) {
                std::env::remove_var(name);
                i += 1;
                continue;
            }
            return error("env", "option requires an argument");
        }
        if let Some((k, v)) = a.split_once('=') {
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                assigns.push((k.to_string(), v.to_string()));
                i += 1;
                continue;
            }
        }
        break;
    }
    let cmd: Vec<String> = args[i..].to_vec();
    if cmd.is_empty() {
        if clear {
            return 0;
        }
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        use std::io::Write;
        for (k, v) in std::env::vars() {
            let _ = writeln!(lock, "{k}={v}");
        }
        return 0;
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    if clear {
        c.env_clear();
    }
    for (k, v) in &assigns {
        c.env(k, v);
    }
    match c.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => error("env", &format!("{}: {}", cmd[0], e)),
    }
}

// ---------------------------------------------------------------- sleep

pub fn sleep(args: &[String]) -> i32 {
    let spec = match args.first() {
        Some(s) => s,
        None => return error("sleep", "missing operand"),
    };
    let secs = match parse_duration(spec) {
        Some(s) => s,
        None => return error("sleep", &format!("invalid time interval '{spec}'")),
    };
    std::thread::sleep(secs);
    0
}

fn parse_duration(spec: &str) -> Option<std::time::Duration> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (num, mult) = match spec.chars().last() {
        Some('s') => (&spec[..spec.len() - 1], 1.0),
        Some('m') => (&spec[..spec.len() - 1], 60.0),
        Some('h') => (&spec[..spec.len() - 1], 3600.0),
        Some('d') => (&spec[..spec.len() - 1], 86400.0),
        _ => (spec, 1.0),
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some(std::time::Duration::from_secs_f64(v * mult))
}

// ---------------------------------------------------------------- date

pub fn date(args: &[String]) -> i32 {
    let mut utc = false;
    let mut fmt: Option<String> = None;
    for a in args {
        if a == "-u" || a == "--utc" || a == "--universal" {
            utc = true;
        } else if a.starts_with('+') {
            fmt = Some(a[1..].to_string());
        } else if a.starts_with('-') {
            return error("date", &format!("invalid option -- '{}'", &a[1..]));
        }
    }
    let now = timefmt::now_epoch();
    let t = if utc {
        timefmt::utc(now)
    } else {
        timefmt::local_parts(now)
    };
    let fmt = fmt.unwrap_or_else(|| {
        if utc {
            "%a %b %e %H:%M:%S UTC %Y".to_string()
        } else {
            "%a %b %e %H:%M:%S %Z %Y".to_string()
        }
    });
    // Provide a local-friendly %Z (timezone names are not available without
    // a tz database, so render the numeric offset instead).
    let rendered = timefmt::strftime(&fmt, &t);
    let tz = if utc {
        "UTC".to_string()
    } else {
        let off = t.offset;
        if off == 0 {
            "UTC".to_string()
        } else {
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.abs();
            format!("UTC{sign}{:02}{:02}", a / 3600, (a % 3600) / 60)
        }
    };
    println!("{}", rendered.replace("%Z", &tz));
    0
}

// ---------------------------------------------------------------- uname

pub fn uname(args: &[String]) -> i32 {
    let mut s = false;
    let mut n = false;
    let mut r = false;
    let mut v = false;
    let mut m = false;
    let mut a = false;
    let mut any = false;
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for c in arg[1..].chars() {
                match c {
                    's' => s = true,
                    'n' => n = true,
                    'r' => r = true,
                    'v' => v = true,
                    'm' => m = true,
                    'a' => a = true,
                    other => return error("uname", &format!("invalid option -- '{other}'")),
                }
                any = true;
            }
        }
    }
    if !any || a {
        s = true;
        n = true;
        r = true;
        v = true;
        m = true;
    }
    let mut parts: Vec<String> = Vec::new();
    if s {
        parts.push(sysname());
    }
    if n {
        parts.push(nodename());
    }
    if r {
        parts.push(release());
    }
    if v {
        parts.push(version());
    }
    if m {
        parts.push(machine());
    }
    println!("{}", parts.join(" "));
    0
}

fn sysname() -> String {
    if cfg!(target_os = "linux") {
        "Linux".into()
    } else if cfg!(target_os = "macos") {
        "Darwin".into()
    } else if cfg!(windows) {
        "Windows".into()
    } else {
        std::env::consts::OS.to_string()
    }
}

fn nodename() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0i8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return "localhost".into();
        }
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        "localhost".into()
    }
}

fn release() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            return s.trim().to_string();
        }
    }
    if cfg!(windows) {
        "unknown".into()
    } else {
        "unknown".into()
    }
}

fn version() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/version") {
            return s.trim().to_string();
        }
    }
    "unknown".into()
}

fn machine() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".into(),
        "x86" => "i686".into(),
        "aarch64" => "aarch64".into(),
        "arm" => "armv7l".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- mount / umount

#[cfg(target_os = "linux")]
pub fn mount(args: &[String]) -> i32 {
    use std::ffi::CString;
    let mut fstype: Option<String> = None;
    let mut options: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-t" => {
                i += 1;
                fstype = args.get(i).cloned();
            }
            "-o" => {
                i += 1;
                options = args.get(i).cloned();
            }
            "-n" | "-v" | "-a" | "-r" | "-w" => {}
            other if other.starts_with('-') => {
                return error("mount", &format!("invalid option -- '{}'", &other[1..]));
            }
            _ => rest.push(a.clone()),
        }
        i += 1;
    }
    if rest.len() != 2 {
        return error("mount", "usage: mount [-t type] [-o options] device dir");
    }
    let dev = match CString::new(rest[0].clone()) {
        Ok(c) => c,
        Err(_) => return error("mount", "invalid device"),
    };
    let dir = match CString::new(rest[1].clone()) {
        Ok(c) => c,
        Err(_) => return error("mount", "invalid directory"),
    };
    // Keep the CStrings alive until after the syscall.
    let type_c: Option<CString> = fstype.as_ref().and_then(|f| CString::new(f.clone()).ok());
    let opts_str = options.unwrap_or_default();
    let bind = opts_str.split(',').any(|o| o == "bind");
    let opts_c: Option<CString> = if bind || opts_str.is_empty() {
        None
    } else {
        CString::new(opts_str).ok()
    };
    let type_ptr = type_c
        .as_ref()
        .map(|c| c.as_ptr())
        .unwrap_or(std::ptr::null());
    let data_ptr = opts_c
        .as_ref()
        .map(|c| c.as_ptr())
        .unwrap_or(std::ptr::null());
    let flags: libc::c_ulong = if bind { libc::MS_BIND } else { 0 };
    let rc = unsafe {
        libc::mount(
            dev.as_ptr(),
            dir.as_ptr(),
            type_ptr,
            flags,
            data_ptr as *const libc::c_void,
        )
    };
    if rc == 0 {
        0
    } else {
        let e = std::io::Error::last_os_error();
        error("mount", &format!("{}: {e}", rest[1]))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn mount(_args: &[String]) -> i32 {
    error("mount", "only supported on Linux")
}

#[cfg(target_os = "linux")]
pub fn umount(args: &[String]) -> i32 {
    use std::ffi::CString;
    let mut detach = false;
    let mut target: Option<String> = None;
    for a in args {
        match a.as_str() {
            "-l" | "-d" | "-v" | "-n" | "-r" => detach = true,
            other if other.starts_with('-') && other.len() > 1 => {
                return error("umount", &format!("invalid option -- '{}'", &other[1..]));
            }
            _ => target = Some(a.clone()),
        }
    }
    let target = match target {
        Some(t) => t,
        None => return error("umount", "missing target"),
    };
    let c = match CString::new(target.clone()) {
        Ok(c) => c,
        Err(_) => return error("umount", "invalid target"),
    };
    let flags = if detach { libc::MNT_DETACH } else { 0 };
    let rc = unsafe { libc::umount2(c.as_ptr(), flags) };
    if rc == 0 {
        0
    } else {
        let e = std::io::Error::last_os_error();
        error("umount", &format!("{target}: {e}"))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn umount(_args: &[String]) -> i32 {
    error("umount", "only supported on Linux")
}
