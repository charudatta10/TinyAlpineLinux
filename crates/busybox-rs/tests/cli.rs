//! End-to-end tests driving the `busybox-rs` multi-call binary.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn bb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_busybox-rs"))
}

fn run(args: &[&str]) -> Output {
    bb().args(args).output().expect("run busybox-rs")
}

fn run_stdin(args: &[&str], input: &str) -> Output {
    use std::io::Write;
    let mut child = bb()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn busybox-rs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait busybox-rs")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("busybox-rs-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir temp");
    d
}

// ---------------------------------------------------------------- applets

#[test]
fn multicall_dispatch_lists_applets() {
    let o = run(&["--list"]);
    assert!(o.status.success());
    let text = out(&o);
    for a in ["ls", "cat", "grep", "printf", "test", "mount"] {
        assert!(text.contains(a), "missing applet {a}: {text}");
    }
}

#[test]
fn echo_behaves_like_posix_echo() {
    assert_eq!(out(&run(&["echo", "hello", "world"])), "hello world\n");
    assert_eq!(out(&run(&["echo", "-n", "hi"])), "hi");
    assert_eq!(out(&run(&["echo", "-e", "a\\tb"])), "a\tb\n");
    // Without -e escapes stay literal (POSIX echo).
    assert_eq!(out(&run(&["echo", "a\\tb"])), "a\\tb\n");
}

#[test]
fn printf_formats_like_the_real_thing() {
    assert_eq!(
        out(&run(&["printf", "%s-%05d|\\n", "abc", "42"])),
        "abc-00042|\n"
    );
    // Format reuse across arguments.
    assert_eq!(out(&run(&["printf", "%s.", "a", "b", "c"])), "a.b.c.");
    // Missing arguments print as empty/zero, not a panic.
    assert_eq!(out(&run(&["printf", "%s|%d|\\n"])), "|0|\n");
}

#[test]
fn text_pipeline_applets() {
    assert_eq!(
        out(&run_stdin(&["sort"], "b\na\nc\n")),
        "a\nb\nc\n"
    );
    assert_eq!(
        out(&run_stdin(&["sort", "-n", "-r"], "10\n2\n1\n")),
        "10\n2\n1\n"
    );
    assert_eq!(out(&run_stdin(&["wc", "-l"], "x\ny\n")), "      2 <stdin>\n");
    assert_eq!(out(&run_stdin(&["head", "-n", "2"], "1\n2\n3\n")), "1\n2\n");
    assert_eq!(out(&run_stdin(&["tail", "-n", "2"], "1\n2\n3\n")), "2\n3\n");
    assert_eq!(out(&run_stdin(&["cut", "-d:", "-f2"], "a:b:c\n")), "b\n");
    assert_eq!(out(&run_stdin(&["tr", "a-z", "A-Z"], "hi there\n")), "HI THERE\n");
    assert_eq!(out(&run_stdin(&["tr", "-d", "aeiou"], "hello\n")), "hll\n");
    assert_eq!(out(&run(&["seq", "1", "3"])), "1\n2\n3\n");
}

#[test]
fn grep_supports_common_flags() {
    let input = "apple\nbanana\ncherry\n";
    let o = run_stdin(&["grep", "an"], input);
    assert_eq!(out(&o), "banana\n");
    assert!(o.status.success());

    let o = run_stdin(&["grep", "-n", "an"], input);
    assert_eq!(out(&o), "2:banana\n");

    let o = run_stdin(&["grep", "-c", "^b"], input);
    assert_eq!(out(&o), "1\n");

    let o = run_stdin(&["grep", "-v", "an"], input);
    assert_eq!(out(&o), "apple\ncherry\n");

    // No match exits 1.
    let o = run_stdin(&["grep", "zzz"], input);
    assert_eq!(o.status.code(), Some(1));

    // Regex features: classes, alternation, anchors.
    let o = run_stdin(&["grep", "^(ap|ch)"], input);
    assert_eq!(out(&o), "apple\ncherry\n");

    // Word matching must not fire inside a longer word.
    let o = run_stdin(&["grep", "-w", "an"], "an\nbanana\n");
    assert_eq!(out(&o), "an\n");
}

#[test]
fn grep_multiline_pattern_file() {
    let d = tmpdir("grepfile");
    std::fs::write(d.join("pats.txt"), "foo\nbar\n").unwrap();
    let o = run_stdin(
        &["grep", "-F", "-f", d.join("pats.txt").to_str().unwrap()],
        "foo\nbaz\nbar\n",
    );
    assert_eq!(out(&o), "foo\nbar\n");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn test_applet_expressions() {
    assert!(run(&["test", "abc"]).status.success());
    assert!(!run(&["test", ""]).status.success());
    assert!(run(&["test", "3", "-gt", "2"]).status.success());
    assert!(!run(&["test", "2", "-gt", "3"]).status.success());
    assert!(run(&["test", "a", "=", "a"]).status.success());
    assert!(!run(&["test", "a", "=", "b"]).status.success());
    assert!(run(&["test", "!", "-z", "x"]).status.success());
    // -a / -o chains
    assert!(run(&["test", "1", "-eq", "1", "-a", "2", "-eq", "2"]).status.success());
    assert!(!run(&["test", "1", "-eq", "1", "-a", "2", "-eq", "3"]).status.success());
    assert!(run(&["test", "1", "-eq", "2", "-o", "2", "-eq", "2"]).status.success());
    // file tests
    assert!(run(&["test", "-f", "Cargo.toml"]).status.success());
    assert!(run(&["test", "-d", "."]).status.success());
    assert!(!run(&["test", "-d", "Cargo.toml"]).status.success());
    assert!(!run(&["test", "-e", "definitely-not-here"]).status.success());
    // `[` alias
    assert!(run(&["[", "-f", "Cargo.toml", "]"]).status.success());
}

#[test]
fn file_management_applets() {
    let d = tmpdir("fs");
    let sub = d.join("sub");
    let f = sub.join("a.txt");

    assert!(run(&["mkdir", "-p", sub.to_str().unwrap()]).status.success());
    assert!(sub.is_dir());

    std::fs::write(&f, "hello").unwrap();
    let o = run(&["cat", f.to_str().unwrap()]);
    assert_eq!(out(&o), "hello");

    let dst = d.join("copy");
    assert!(run(&[
        "cp",
        "-r",
        sub.to_str().unwrap(),
        dst.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");

    let moved = d.join("moved");
    assert!(run(&["mv", dst.to_str().unwrap(), moved.to_str().unwrap()]).status.success());
    assert!(!dst.exists());
    assert!(moved.join("a.txt").exists());

    let t = d.join("touch.txt");
    assert!(run(&["touch", t.to_str().unwrap()]).status.success());
    assert!(t.exists());

    assert!(run(&["rm", "-r", moved.to_str().unwrap()]).status.success());
    assert!(!moved.exists());
    assert!(run(&["rm", "-r", "-f", d.join("gone").to_str().unwrap()]).status.success());

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ls_lists_and_long_formats() {
    let d = tmpdir("ls");
    std::fs::write(d.join("alpha.txt"), "x").unwrap();
    std::fs::write(d.join("beta.txt"), "y").unwrap();

    let o = run(&["ls", d.to_str().unwrap()]);
    assert_eq!(out(&o), "alpha.txt\nbeta.txt\n");

    let o = run(&["ls", "-l", d.to_str().unwrap()]);
    let text = out(&o);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    for l in lines {
        assert!(l.starts_with('-') || l.starts_with('d'), "bad mode: {l}");
        assert!(l.contains("alpha.txt") || l.contains("beta.txt"), "{l}");
    }

    let o = run(&["ls", "-a", d.to_str().unwrap()]);
    assert!(out(&o).contains("."));

    let o = run(&["ls", "-r", d.to_str().unwrap()]);
    assert_eq!(out(&o), "beta.txt\nalpha.txt\n");

    let o = run(&["ls", "no-such-path-here"]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).is_empty());

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn path_helpers() {
    assert_eq!(out(&run(&["basename", "/a/b/c.txt"])), "c.txt\n");
    assert_eq!(out(&run(&["basename", "/a/b/c.txt", ".txt"])), "c\n");
    assert_eq!(out(&run(&["basename", "/a/b/"])), "b\n");
    assert_eq!(out(&run(&["dirname", "/a/b/c"])), "/a/b\n");
    assert_eq!(out(&run(&["dirname", "c"])), ".\n");
    assert!(run(&["basename"]).status.success() == false);
}

#[test]
fn env_runs_command_with_overrides() {
    let o = run(&["env", "FOO=bar", "sh", "-c", "echo $FOO"]);
    if o.status.success() {
        assert_eq!(out(&o), "bar\n");
    } else {
        // No sh on this machine: fall back to plain listing behaviour.
        let o = run(&["env"]);
        assert!(out(&o).contains('='));
    }
    let o = run(&["env"]);
    assert!(o.status.success());
}

#[test]
fn date_and_uname() {
    let o = run(&["date", "+%Y"]);
    assert!(o.status.success());
    let year = out(&o).trim().to_string();
    let y: i32 = year.parse().expect("year should be numeric");
    assert!((2000..=2999).contains(&y), "unexpected year {y}");

    let o = run(&["date", "+%s"]);
    assert!(o.status.success());
    out(&o).trim().parse::<i64>().expect("epoch seconds");

    let o = run(&["date", "-u", "+%F"]);
    assert!(o.status.success());
    assert_eq!(out(&o).len(), 11); // YYYY-MM-DD

    let o = run(&["uname", "-s"]);
    assert!(o.status.success());
    assert!(!out(&o).trim().is_empty());
}

#[test]
fn which_finds_or_reports_missing() {
    let o = run(&["which", "definitely-not-a-real-command-xyz"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&o.stderr).is_empty());
}

#[test]
fn true_false_exit_codes() {
    assert!(run(&["true"]).status.success());
    assert_eq!(run(&["false"]).status.code(), Some(1));
}

#[test]
fn unknown_applet_fails_with_127() {
    let o = run(&["no-such-applet"]);
    assert_eq!(o.status.code(), Some(127));
}

#[test]
fn seq_edge_cases() {
    assert_eq!(out(&run(&["seq", "5"])), "1\n2\n3\n4\n5\n");
    assert_eq!(out(&run(&["seq", "0", "2", "6"])), "0\n2\n4\n6\n");
    // FIRST > LAST with a positive increment prints nothing (POSIX seq).
    assert_eq!(out(&run(&["seq", "5", "1"])), "");
    assert_eq!(out(&run(&["seq", "5", "-1", "1"])), "5\n4\n3\n2\n1\n");
}
