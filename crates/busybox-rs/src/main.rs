//! busybox-rs: a BusyBox-style multi-call binary.
//!
//! Every utility is an "applet". The binary decides which applet to run from
//! `argv[0]` (when invoked through a symlink/hardlink such as `ls`) or from
//! the first argument when invoked as `busybox-rs <applet> ...`.

mod fsapp;
mod helpers;
mod regexlite;
mod sysapp;
mod textapp;
mod timefmt;

type Applet = fn(&[String]) -> i32;

const APPLETS: &[(&str, &str, Applet)] = &[
    ("basename", "strip directory and suffix from filenames", fsapp::basename),
    ("cat", "concatenate files to stdout", fsapp::cat),
    ("clear", "clear the terminal screen", textapp::clear),
    ("cp", "copy files and directories", fsapp::cp),
    ("cut", "remove sections from each line", textapp::cut),
    ("date", "print the current date and time", sysapp::date),
    ("dirname", "strip last component of file name", fsapp::dirname),
    ("echo", "display a line of text", textapp::echo),
    ("env", "print or run with a modified environment", sysapp::env),
    ("false", "return an unsuccessful exit status", false_cmd),
    ("grep", "print lines matching a pattern", textapp::grep),
    ("head", "output the first part of files", textapp::head),
    ("ln", "make links between files", fsapp::ln),
    ("ls", "list directory contents", fsapp::ls),
    ("mkdir", "create directories", fsapp::mkdir),
    ("mount", "mount a filesystem (Linux)", sysapp::mount),
    ("mv", "move (rename) files", fsapp::mv),
    ("printf", "format and print data", textapp::printf),
    ("pwd", "print name of current directory", fsapp::pwd),
    ("rm", "remove files or directories", fsapp::rm),
    ("rmdir", "remove empty directories", fsapp::rmdir),
    ("seq", "print a sequence of numbers", textapp::seq),
    ("sleep", "pause for a number of seconds", sysapp::sleep),
    ("sort", "sort lines of text", textapp::sort),
    ("tail", "output the last part of files", textapp::tail),
    ("test", "evaluate expressions", textapp::test_cmd),
    ("touch", "change file timestamps", fsapp::touch),
    ("tr", "translate or delete characters", textapp::tr),
    ("true", "return a successful exit status", true_cmd),
    ("umount", "unmount filesystems (Linux)", sysapp::umount),
    ("uname", "print system information", sysapp::uname),
    ("wc", "count lines, words and bytes", textapp::wc),
    ("which", "locate a command in PATH", textapp::which),
    ("[", "evaluate expressions", textapp::test_cmd),
];

fn true_cmd(_args: &[String]) -> i32 {
    0
}

fn false_cmd(_args: &[String]) -> i32 {
    1
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // Use the file stem so `busybox-rs.exe` behaves like `busybox-rs`.
    let arg0 = argv
        .first()
        .map(|a| {
            std::path::Path::new(a)
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| a.clone())
        })
        .unwrap_or_default();

    let is_multicall = arg0.starts_with("busybox");
    if is_multicall {
        if argv.len() < 2 {
            print_usage();
            std::process::exit(0);
        }
        let name = &argv[1];
        if name == "--help" || name == "-h" {
            print_usage();
            std::process::exit(0);
        }
        if name == "--list" {
            for (n, _, _) in APPLETS {
                println!("{n}");
            }
            std::process::exit(0);
        }
        run(name, &argv[2..]);
    } else {
        // Invoked as `ls`, `cat`, ... through a link or copy.
        run(&arg0, &argv[1..]);
    }
}

fn run(name: &str, args: &[String]) -> ! {
    match APPLETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, _, f)) => std::process::exit(f(args)),
        None => {
            eprintln!("busybox-rs: unknown applet: {name}");
            eprintln!("Try 'busybox-rs --list' for the list of applets.");
            std::process::exit(127);
        }
    }
}

fn print_usage() {
    println!("busybox-rs {} (Rust, multi-call)", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: busybox-rs <applet> [arguments...]");
    println!("   or: <applet> [arguments...]   (when argv[0] is the applet name)");
    println!();
    println!("Available applets:");
    let mut names: Vec<(&str, &str)> = APPLETS.iter().map(|(n, d, _)| (*n, *d)).collect();
    names.sort();
    for (n, d) in names {
        println!("  {n:<10} {d}");
    }
}
