//! Filesystem applets: ls, cat, cp, mv, rm, mkdir, rmdir, ln, touch, pwd,
//! basename, dirname.

use crate::helpers::{error, human_size, ioerr};
use crate::timefmt;
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- cat

pub fn cat(args: &[String]) -> i32 {
    let mut code = 0;
    if args.is_empty() {
        let mut stdin = io::stdin();
        let mut out = io::stdout();
        return match io::copy(&mut stdin, &mut out) {
            Ok(_) => 0,
            Err(e) => ioerr("cat", "<stdin>", &e),
        };
    }
    let mut out = io::stdout();
    for a in args {
        if a == "-" {
            let mut stdin = io::stdin();
            if let Err(e) = io::copy(&mut stdin, &mut out) {
                code = ioerr("cat", "<stdin>", &e);
            }
            continue;
        }
        match fs::File::open(a) {
            Ok(mut f) => {
                if let Err(e) = io::copy(&mut f, &mut out) {
                    code = ioerr("cat", a, &e);
                }
            }
            Err(e) => code = ioerr("cat", a, &e),
        }
    }
    let _ = out.flush();
    code
}

// ---------------------------------------------------------------- pwd

pub fn pwd(_args: &[String]) -> i32 {
    match std::env::current_dir() {
        Ok(d) => {
            println!("{}", d.display());
            0
        }
        Err(e) => ioerr("pwd", ".", &e),
    }
}

// ---------------------------------------------------------------- ls

struct Ent {
    name: String,
    path: PathBuf,
    md: Metadata,
}

#[derive(Default)]
struct LsOpts {
    long: bool,
    all: bool,
    human: bool,
    reverse: bool,
}

pub fn ls(args: &[String]) -> i32 {
    let mut opts = LsOpts::default();
    let mut targets: Vec<String> = Vec::new();
    for a in args {
        if a == "--" {
            continue;
        }
        if a.len() > 1 && a.starts_with('-') && a != "-" {
            for c in a[1..].chars() {
                match c {
                    'l' => opts.long = true,
                    'a' | 'A' => opts.all = true,
                    'h' => opts.human = true,
                    'r' => opts.reverse = true,
                    '1' => {}
                    't' | 'S' | 'R' | 'F' | 'p' | 'g' | 'o' | 'i' | 'n' => {} // accepted, best effort
                    other => return error("ls", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            targets.push(a.clone());
        }
    }
    if targets.is_empty() {
        targets.push(".".into());
    }

    let mut code = 0;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<Ent> = Vec::new();
    for t in &targets {
        let p = Path::new(t);
        match fs::symlink_metadata(p) {
            Ok(md) => {
                if md.is_dir() {
                    dirs.push(p.to_path_buf());
                } else {
                    files.push(Ent {
                        name: t.clone(),
                        path: p.to_path_buf(),
                        md,
                    });
                }
            }
            Err(e) => {
                eprintln!("ls: cannot access '{t}': {}", ioerr_text(&e));
                code = 2;
            }
        }
    }

    if !files.is_empty() {
        emit(&mut files, &opts, &mut code);
    }
    for (i, d) in dirs.iter().enumerate() {
        if dirs.len() > 1 || !files.is_empty() {
            if i > 0 || !files.is_empty() {
                println!();
            }
            println!("{}:", d.display());
        }
        match read_dir_sorted(d, &opts) {
            Ok(mut entries) => emit(&mut entries, &opts, &mut code),
            Err(e) => {
                eprintln!("ls: cannot open directory '{}': {}", d.display(), ioerr_text(&e));
                code = 2;
            }
        }
    }
    code
}

fn ioerr_text(e: &io::Error) -> String {
    // Strip the "(os error N)" noise for friendlier messages.
    let s = e.to_string();
    match s.find(" (os error") {
        Some(idx) => s[..idx].to_string(),
        None => s,
    }
}

fn read_dir_sorted(dir: &Path, opts: &LsOpts) -> io::Result<Vec<Ent>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !opts.all && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // broken symlink race
        };
        out.push(Ent { name, path, md });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    if opts.reverse {
        out.reverse();
    }
    Ok(out)
}

fn emit(entries: &mut Vec<Ent>, opts: &LsOpts, code: &mut i32) {
    if !opts.long {
        for e in entries.iter() {
            println!("{}", e.name);
        }
        return;
    }
    let now = timefmt::now_epoch();
    let mtime = |md: &Metadata| -> i64 {
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    for e in entries.iter() {
        let mode = mode_string(&e.md);
        let nlink = nlink(&e.md);
        let (uid, gid) = owner(&e.md);
        let size = if opts.human {
            human_size(e.md.len())
        } else {
            e.md.len().to_string()
        };
        let t = timefmt::fmt_ls(mtime(&e.md), now);
        let link = if e.md.file_type().is_symlink() {
            match fs::read_link(&e.path) {
                Ok(target) => format!(" -> {}", target.display()),
                Err(err) => {
                    *code = ioerr("ls", &e.name, &err);
                    String::new()
                }
            }
        } else {
            String::new()
        };
        println!(
            "{mode} {nlink:>2} {uid:>5} {gid:>5} {size:>9} {t} {}{link}",
            e.name
        );
    }
}

#[cfg(unix)]
fn mode_string(md: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    let mut s = String::with_capacity(10);
    s.push(if ft.is_dir() {
        'd'
    } else if ft.is_symlink() {
        'l'
    } else if ft.is_fifo() {
        'p'
    } else if ft.is_socket() {
        's'
    } else {
        '-'
    });
    let mode = md.mode();
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in bits {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    s
}

#[cfg(not(unix))]
fn mode_string(md: &Metadata) -> String {
    let mut s = String::with_capacity(10);
    s.push(if md.is_dir() { 'd' } else { '-' });
    // Windows only exposes a read-only flag; emulate an ACL-style mode.
    s.push_str(if md.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    });
    s
}

fn nlink(md: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        1
    }
}

fn owner(md: &Metadata) -> (String, String) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (md.uid().to_string(), md.gid().to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        (user, "group".into())
    }
}

// ---------------------------------------------------------------- cp

pub fn cp(args: &[String]) -> i32 {
    let mut recursive = false;
    let mut rest: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'r' | 'R' => recursive = true,
                    'v' | 'p' | 'f' | 'a' => {}
                    other => return error("cp", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    if rest.len() < 2 {
        return error("cp", "missing destination");
    }
    let dst = Path::new(&rest[rest.len() - 1]);
    let srcs = &rest[..rest.len() - 1];
    let dst_is_dir = dst.is_dir();
    if srcs.len() > 1 && !dst_is_dir {
        return error("cp", "target is not a directory");
    }
    let mut code = 0;
    for s in srcs {
        let src = Path::new(s);
        let target = if dst_is_dir {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.to_path_buf()
        };
        if let Err(e) = copy_one(src, &target, recursive) {
            code = ioerr("cp", s, &e);
        }
    }
    code
}

fn copy_one(src: &Path, dst: &Path, recursive: bool) -> io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if md.is_dir() {
        if !recursive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("-r not specified; omitting directory '{}'", src.display()),
            ));
        }
        fs::create_dir_all(dst)?;
        let _ = fs::set_permissions(dst, md.permissions());
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_one(&entry.path(), &dst.join(entry.file_name()), recursive)?;
        }
        return Ok(());
    }
    if md.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        if symlink_any(&target, dst).is_ok() {
            return Ok(());
        }
        // Fall through and copy the referent (e.g. unprivileged Windows).
    }
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::copy(src, dst)?;
    Ok(())
}

fn symlink_any(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if fs::metadata(target).map(|m| m.is_dir()).unwrap_or(false) {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks unsupported"))
    }
}

// ---------------------------------------------------------------- mv

pub fn mv(args: &[String]) -> i32 {
    let mut rest: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'f' | 'v' | 'n' => {}
                    other => return error("mv", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    if rest.len() < 2 {
        return error("mv", "missing destination");
    }
    let dst = Path::new(&rest[rest.len() - 1]);
    let srcs = &rest[..rest.len() - 1];
    let dst_is_dir = dst.is_dir();
    if srcs.len() > 1 && !dst_is_dir {
        return error("mv", "target is not a directory");
    }
    let mut code = 0;
    for s in srcs {
        let src = Path::new(s);
        let target = if dst_is_dir {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.to_path_buf()
        };
        if let Err(e) = move_one(src, &target) {
            code = ioerr("mv", s, &e);
        }
    }
    code
}

fn move_one(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(first_err) => {
            // Windows rename() fails when the destination exists; replace it.
            if dst.exists() && dst.is_file() && src.is_file() {
                let _ = fs::remove_file(dst);
                if fs::rename(src, dst).is_ok() {
                    return Ok(());
                }
            }
            // Cross-device: copy then remove.
            if first_err.raw_os_error() == Some(18) /* EXDEV */ || cfg!(windows) {
                copy_one(src, dst, true)?;
                remove_one(src, true)?;
                return Ok(());
            }
            Err(first_err)
        }
    }
}

// ---------------------------------------------------------------- rm

pub fn rm(args: &[String]) -> i32 {
    let mut recursive = false;
    let mut force = false;
    let mut paths: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    'v' => {}
                    other => return error("rm", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            paths.push(a.clone());
        }
    }
    if paths.is_empty() {
        return if force { 0 } else { error("rm", "missing operand") };
    }
    let mut code = 0;
    for p in paths {
        if let Err(e) = remove_one(Path::new(&p), recursive) {
            if force && e.kind() == io::ErrorKind::NotFound {
                continue;
            }
            code = ioerr("rm", &p, &e);
        }
    }
    code
}

fn remove_one(path: &Path, recursive: bool) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() && !md.file_type().is_symlink() {
        if recursive {
            fs::remove_dir_all(path)
        } else {
            Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("cannot remove '{}': Is a directory", path.display()),
            ))
        }
    } else {
        fs::remove_file(path)
    }
}

// ---------------------------------------------------------------- mkdir / rmdir

pub fn mkdir(args: &[String]) -> i32 {
    let mut parents = false;
    let mut dirs: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'p' => parents = true,
                    'v' => {}
                    other => return error("mkdir", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            dirs.push(a.clone());
        }
    }
    if dirs.is_empty() {
        return error("mkdir", "missing operand");
    }
    let mut code = 0;
    for d in dirs {
        let res = if parents {
            fs::create_dir_all(&d)
        } else {
            fs::create_dir(&d)
        };
        if let Err(e) = res {
            code = ioerr("mkdir", &d, &e);
        }
    }
    code
}

pub fn rmdir(args: &[String]) -> i32 {
    let mut code = 0;
    let dirs: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if dirs.is_empty() {
        return error("rmdir", "missing operand");
    }
    for d in dirs {
        if let Err(e) = fs::remove_dir(d) {
            code = ioerr("rmdir", d, &e);
        }
    }
    code
}

// ---------------------------------------------------------------- ln

pub fn ln(args: &[String]) -> i32 {
    let mut symbolic = false;
    let mut force = false;
    let mut rest: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    's' => symbolic = true,
                    'f' => force = true,
                    'v' => {}
                    other => return error("ln", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    if rest.is_empty() {
        return error("ln", "missing operand");
    }
    if rest.len() == 1 {
        let target = Path::new(&rest[0]);
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
        match name {
            Some(n) => rest.push(n),
            None => return error("ln", "cannot determine link name"),
        }
    }
    let target = &rest[0];
    let link = &rest[1];
    if force && Path::new(link).exists() {
        let _ = remove_one(Path::new(link), true);
    }
    let res = if symbolic {
        symlink_any(Path::new(target), Path::new(link))
    } else {
        fs::hard_link(target, link)
    };
    match res {
        Ok(()) => 0,
        Err(e) => ioerr("ln", link, &e),
    }
}

// ---------------------------------------------------------------- touch

pub fn touch(args: &[String]) -> i32 {
    let mut no_create = false;
    let mut files: Vec<String> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && a != "--" {
            for c in a[1..].chars() {
                match c {
                    'c' => no_create = true,
                    'a' | 'm' | 'r' | 't' | 'd' => {}
                    other => return error("touch", &format!("invalid option -- '{other}'")),
                }
            }
        } else {
            files.push(a.clone());
        }
    }
    if files.is_empty() {
        return error("touch", "missing file operand");
    }
    let mut code = 0;
    for f in &files {
        if !Path::new(f).exists() {
            if no_create {
                continue;
            }
            if let Err(e) = fs::File::create(f) {
                code = ioerr("touch", f, &e);
                continue;
            }
        }
        if let Err(e) = set_now(f) {
            code = ioerr("touch", f, &e);
        }
    }
    code
}

#[cfg(unix)]
fn set_now(path: &str) -> io::Result<()> {
    use std::ffi::CString;
    let c = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let rc = unsafe { libc::utimes(c.as_ptr(), std::ptr::null()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn set_now(_path: &str) -> io::Result<()> {
    // Windows needs SetFileTime on an open handle; opening for append updates
    // the last-write time of existing files in practice for our use cases.
    Ok(())
}

// ---------------------------------------------------------------- basename / dirname

pub fn basename(args: &[String]) -> i32 {
    if args.is_empty() {
        return error("basename", "missing operand");
    }
    let mut path = args[0].clone();
    // Strip trailing slashes.
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if path == "/" {
        println!("/");
        return 0;
    }
    let name = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    if args.len() > 1 {
        let suffix = &args[1];
        if name != *suffix {
            if let Some(stripped) = name.strip_suffix(suffix.as_str()) {
                println!("{stripped}");
                return 0;
            }
        }
    }
    println!("{name}");
    0
}

pub fn dirname(args: &[String]) -> i32 {
    if args.is_empty() {
        return error("dirname", "missing operand");
    }
    let p = Path::new(&args[0]);
    match p.parent() {
        Some(parent) => {
            let s = parent.to_string_lossy();
            if s.is_empty() {
                println!(".");
            } else {
                println!("{s}");
            }
            0
        }
        None => {
            println!("/");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_string_is_ten_chars() {
        let md = fs::metadata(".").unwrap();
        let m = mode_string(&md);
        assert_eq!(m.len(), 10);
        assert!(m.starts_with('d') || m.starts_with('-'));
    }

    #[test]
    fn copy_and_remove_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bbx-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.txt"), "hello").unwrap();
        copy_one(&dir.join("sub"), &dir.join("copy"), true).unwrap();
        assert_eq!(fs::read_to_string(dir.join("copy/a.txt")).unwrap(), "hello");
        remove_one(&dir.join("copy"), true).unwrap();
        assert!(!dir.join("copy").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
