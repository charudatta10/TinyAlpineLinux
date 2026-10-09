//! End-to-end tests driving the `rsh` binary.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn sh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsh"))
}

fn run(src: &str) -> Output {
    sh()
        .creation_flags(0x00000200) // CREATE_NO_WINDOW on Windows so ConPTY
        .args(["-c", src])
        .output()
        .expect("run rsh")
}

fn run_stdin(src: &str, input: &str) -> Output {
    let mut child = sh()
        .args(["-c", src])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rsh");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait rsh")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_ok(src: &str, expected: &str) {
    let o = run(src);
    assert!(
        o.status.success(),
        "expected success for `{src}`, got {:?} stderr={:?}",
        o.status.code(),
        err(&o)
    );
    assert_eq!(out(&o), expected, "output mismatch for `{src}`");
}

/// Format a path for embedding inside a shell script (forward slashes so
/// backslashes are not eaten as escapes, and Windows APIs accept `/`).
fn p(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------- basics

#[test]
fn simple_commands() {
    assert_ok("echo hello", "hello\n");
    assert_ok("echo hello   world", "hello world\n");
    assert_ok("echo -n hi", "hi");
    assert_ok("echo -e 'a\\tb'", "a\tb\n");
    assert_ok("printf '%s-%05d|\\n' abc 42", "abc-00042|\n");
}

#[test]
fn quoting_and_expansion() {
    assert_ok("X=42; echo $X", "42\n");
    assert_ok("X=42; echo \"$X\"", "42\n");
    assert_ok("X='a b'; echo $X", "a b\n");
    assert_ok("X='a b'; echo \"$X\"", "a b\n");
    assert_ok("echo 'single $X'", "single $X\n");
    assert_ok("echo a\\ b", "a b\n");
    assert_ok("echo ${UNSET:-fallback}", "fallback\n");
    assert_ok("echo ${#HOME}", &format!("{}\n", std::env::var("HOME").unwrap_or_default().chars().count()));
}

#[test]
fn empty_and_unset_expansions() {
    assert_ok("echo x$UNSET_VAR_XYZ", "x\n");
    assert_ok("echo \"$UNSET_VAR_XYZ\"", "\n");
    assert_ok("echo \"\"", "\n");
    // unset variable removes the word entirely
    let o = run("echo $UNSET_VAR_XYZ end");
    assert_eq!(out(&o), "end\n");
}

#[test]
fn arithmetic() {
    assert_ok("echo $((1+2*3))", "7\n");
    assert_ok("N=5; echo $((N*2))", "10\n");
    assert_ok("echo $((10/3)) $((10%3))", "3 1\n");
    assert_ok("echo $((1 < 2))", "1\n");
}

#[test]
fn command_substitution() {
    assert_ok("echo $(echo inner)", "inner\n");
    assert_ok("echo `echo backtick`", "backtick\n");
    assert_ok("echo $(echo $(echo deep))", "deep\n");
    assert_ok("X=$(echo cap); echo $X", "cap\n");
    assert_ok("echo pre$(echo post)", "prepost\n");
}

// ---------------------------------------------------------------- control flow

#[test]
fn pipelines() {
    assert_ok("echo hello | cat", "hello\n");
    let o = run("seq 1 10 | wc -l");
    let n: i64 = out(&o).trim().parse().expect("wc should print a count");
    assert_eq!(n, 10);
    assert_ok("printf 'b\\na\\nc\\n' | sort", "a\nb\nc\n");
    assert_ok("echo a b c | tr a-z A-Z", "A B C\n");
    // Status of a pipeline is the status of the last stage.
    assert_ok("false | true; echo $?", "0\n");
    assert_ok("true | false; echo $?", "1\n");
    // Negation
    assert_ok("! false; echo $?", "0\n");
    assert_ok("! true; echo $?", "1\n");
}

#[test]
fn lists_and_conditions() {
    assert_ok("true; echo after", "after\n");
    assert_ok("false || echo or-ran", "or-ran\n");
    assert_ok("true && echo and-ran", "and-ran\n");
    assert_ok("false && echo nope || echo fell", "fell\n");
    assert_ok("if true; then echo a; else echo b; fi", "a\n");
    assert_ok("if false; then echo a; else echo b; fi", "b\n");
    assert_ok("if false; then echo a; elif true; then echo b; else echo c; fi", "b\n");
    assert_ok("if false; then echo a; elif false; then echo b; else echo c; fi", "c\n");
    assert_ok("if false; then echo a; fi; echo done", "done\n");
}

#[test]
fn loops() {
    assert_ok("for i in 1 2 3; do echo $i; done", "1\n2\n3\n");
    assert_ok(
        "n=0; while [ $n -lt 3 ]; do n=$((n+1)); done; echo $n",
        "3\n",
    );
    assert_ok(
        "n=0; until [ $n -ge 3 ]; do n=$((n+1)); done; echo $n",
        "3\n",
    );
    // break / continue
    assert_ok(
        "for i in 1 2 3 4 5; do if [ $i -eq 2 ]; then continue; fi; if [ $i -eq 4 ]; then break; fi; echo $i; done",
        "1\n3\n",
    );
    // for over command substitution
    assert_ok("for w in $(echo a b); do echo \"[$w]\"; done", "[a]\n[b]\n");
}

#[test]
fn case_statement() {
    assert_ok(
        "case abc in a*) echo matched;; *) echo no;; esac",
        "matched\n",
    );
    assert_ok(
        "case zzz in a*) echo no;; *) echo fallback;; esac",
        "fallback\n",
    );
    assert_ok(
        "case x in x) echo one;; esac",
        "one\n",
    );
    // quoted pattern is literal
    assert_ok(
        "case 'a*b' in 'a*b') echo literal;; *) echo glob;; esac",
        "literal\n",
    );
}

#[test]
fn functions_and_scoping() {
    assert_ok("f() { echo hi $1; }; f there", "hi there\n");
    assert_ok("f() { return 42; }; f; echo $?", "42\n");
    assert_ok(
        "x=global; f() { local x=inner; echo $x; }; f; echo $x",
        "inner\nglobal\n",
    );
    // recursion guard: must terminate
    let o = run("r() { r; }; r");
    assert!(!o.status.success() || err(&o).contains("deep"), "{:?}", o);
}

#[test]
fn positional_parameters() {
    assert_ok("echo $#", "0\n");
    assert_ok("f() { echo \"$1-$2\"; }; f a b", "a-b\n");
    assert_ok("set -- x y z; echo $#; echo \"$@\"", "3\nx y z\n");
}

// ---------------------------------------------------------------- files & redirs

#[test]
fn redirections_and_files() {
    let dir = std::env::temp_dir().join(format!("rsh-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join("out.txt");
    let _ = std::fs::remove_file(&f);
    let fs = p(&f);

    let o = run(&format!("echo hello > {fs}"));
    assert!(o.status.success(), "{:?}", err(&o));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello\n");

    let o = run(&format!("echo second >> {fs}"));
    assert!(o.status.success(), "{:?}", err(&o));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello\nsecond\n");

    // input redirection
    let o = run(&format!("cat < {fs}"));
    assert_eq!(out(&o), "hello\nsecond\n");

    // stdout+stderr merge
    let both = dir.join("both.txt");
    let bs = p(&both);
    let o = run(&format!("echo merged > {bs} 2>&1"));
    assert!(o.status.success(), "{:?}", err(&o));
    assert_eq!(std::fs::read_to_string(&both).unwrap(), "merged\n");

    // here-document
    let o = run("cat <<EOF\nline1\nline2\nEOF\n");
    assert_eq!(out(&o), "line1\nline2\n");

    // quoted here-doc delimiter suppresses expansion
    let o = run("cat <<'EOF'\n$NOT_EXPANDED\nEOF\n");
    assert_eq!(out(&o), "$NOT_EXPANDED\n");

    // here-doc feeding a pipeline stage
    let o = run("cat <<EOF | tr a-z A-Z\nupper\nEOF\n");
    assert_eq!(out(&o), "UPPER\n");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exit_status_and_codes() {
    assert_eq!(run("true").status.code(), Some(0));
    assert_eq!(run("false").status.code(), Some(1));
    assert_eq!(run("exit 7").status.code(), Some(7));
    assert_eq!(run("nosuchcommand_rsh_xyz").status.code(), Some(127));
    // $? tracks failures
    assert_ok("false; echo $?", "1\n");
    assert_ok("nosuchcommand_rsh_xyz 2>/dev/null; echo $?", "127\n");
}

#[test]
fn errexit() {
    // set -e aborts the script on failure
    let o = run("set -e; false; echo NOT_REACHED");
    assert!(!out(&o).contains("NOT_REACHED"));
    assert_eq!(o.status.code(), Some(1));
    // condition contexts are exempt
    assert_ok("set -e; if false; then :; fi; echo survived", "survived\n");
    assert_ok("set -e; false || echo or-ok", "or-ok\n");
}

#[test]
fn nounset() {
    let o = run("set -u; echo $TOTALLY_UNSET_RSH");
    assert!(!o.status.success());
    assert!(err(&o).contains("unbound"), "{}", err(&o));
}

#[test]
fn stdin_reading() {
    let o = run_stdin("read a b; echo \"a=$a b=$b\"", "hello world\n");
    assert_eq!(out(&o), "a=hello b=world\n");

    let o = run_stdin("while read line; do echo \"<$line>\"; done", "one\ntwo\n");
    assert_eq!(out(&o), "<one>\n<two>\n");

    // cat with no args copies stdin
    let o = run_stdin("cat", "through the pipe\n");
    assert_eq!(out(&o), "through the pipe\n");
}

#[test]
fn subshell_isolation() {
    assert_ok("X=outer; ( X=inner; echo $X ); echo $X", "inner\nouter\n");
    assert_ok("{ Y=grouped; }; echo $Y", "grouped\n");
}

#[test]
fn background_and_wait() {
    assert_ok("sleep 0.1 & wait; echo done", "done\n");
}

#[test]
fn syntax_errors_report_status_2() {
    let o = run("if true; then");
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("syntax error"), "{}", err(&o));

    let o = run("echo 'unterminated");
    assert_eq!(o.status.code(), Some(2));

    let o = run("done");
    // `done` alone parses as a command name but must not execute anything
    // harmful; at minimum it must not hang.
    let _ = o;
}

#[test]
fn eval_and_source() {
    assert_ok("eval \"echo evaluated $((2+2))\"", "evaluated 4\n");

    let dir = std::env::temp_dir().join(format!("rsh-src-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join("lib.sh");
    std::fs::write(&f, "VAL=sourced\necho lib:$VAL\n").unwrap();
    let o = run(&format!(". {}; echo after:$VAL", p(&f)));
    assert_eq!(out(&o), "lib:sourced\nafter:sourced\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tilde_expansion() {
    if let Ok(home) = std::env::var("HOME") {
        let o = run("echo ~");
        assert_eq!(out(&o).trim_end(), home);
    }
}

#[test]
fn globbing() {
    let dir = std::env::temp_dir().join(format!("rsh-glob-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();
    std::fs::write(dir.join("c.md"), "").unwrap();
    let ds = p(&dir);

    let o = run(&format!("cd {ds}; echo *.txt"));
    assert_eq!(out(&o), "a.txt b.txt\n");

    // Unmatched globs stay literal.
    let o = run(&format!("cd {ds}; echo nomatch*.zz"));
    assert_eq!(out(&o), "nomatch*.zz\n");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_builtin() {
    assert_ok("[ 1 -eq 1 ] && echo eq", "eq\n");
    assert_ok("[ 2 -gt 1 ] && echo gt", "gt\n");
    assert_ok("[ a = a ] && echo str", "str\n");
    assert_ok("[ -n 'x' ] && echo nonempty", "nonempty\n");
    assert_ok("[ -z '' ] && echo empty", "empty\n");
    assert_ok("[ 1 -eq 2 ] || echo ne", "ne\n");
    assert_ok("[ 1 -eq 1 -a 2 -eq 2 ] && echo and", "and\n");
    assert_ok("[ 1 -eq 2 -o 2 -eq 2 ] && echo or", "or\n");
    assert_ok("! [ 1 -eq 2 ] && echo not", "not\n");
    let o = run("[ -e /definitely/not/a/real/path_rsh ]");
    assert!(!o.status.success());
}

#[test]
fn pwd_cd_use_shell_cwd() {
    let o = run("cd /; pwd");
    assert!(o.status.success(), "{:?}", err(&o));
    // cd failure sets status
    let o = run("cd /definitely/not/a/dir_rsh; echo $?");
    assert!(out(&o).trim().ends_with('1'), "{}", out(&o));
}
