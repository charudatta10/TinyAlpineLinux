//! Shell state: variables, functions, special parameters.

use crate::ast::{Heredoc, Node, Script};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Clone)]
pub struct FuncDef {
    pub nodes: Vec<Node>,
}

#[derive(Clone)]
pub struct Shell {
    pub vars: HashMap<String, String>,
    pub exported: HashSet<String>,
    /// Positional parameters $1..$n ($0 is separate).
    pub positional: Vec<String>,
    pub arg0: String,
    pub functions: HashMap<String, FuncDef>,
    /// Last exit status ($?).
    pub status: i32,
    /// PID of the last background job ($!).
    pub last_bg: String,
    pub pid: u32,
    /// Global here-document table (indices baked into the AST).
    pub heredocs: Vec<Heredoc>,
    /// `set -e`
    pub errexit: bool,
    /// `set -x`
    pub xtrace: bool,
    /// `set -u`
    pub nounset: bool,
    /// Depth guard against runaway recursion.
    pub depth: usize,
    /// Logical working directory (tracked, never set process-wide, so that
    /// background threads and subshell clones stay isolated).
    pub cwd: PathBuf,
    /// Local-variable scopes pushed by function calls: each entry remembers
    /// the value the variable had before the function overwrote it.
    pub scopes: Vec<Vec<(String, Option<String>)>>,
    /// Current umask (as an octal value, e.g. 0o022).
    pub umask: u32,
    /// Depth of `if`/`while`/`&&` condition evaluation (suppresses `set -e`).
    pub cond_depth: u32,
}

/// Strip Windows extended-length prefixes (`\\?\C:\x` → `C:\x`).
pub fn strip_unc(p: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(rest) = p.strip_prefix("\\\\?\\UNC\\") {
            return format!("\\\\{rest}");
        }
        if let Some(rest) = p.strip_prefix("\\\\?\\") {
            return rest.to_string();
        }
    }
    p.to_string()
}

impl Shell {
    pub fn new(arg0: &str, interactive: bool) -> Self {
        let mut vars = HashMap::new();
        let mut exported = HashSet::new();
        // Inherit the whole parent environment as exported shell variables.
        for (k, v) in std::env::vars() {
            exported.insert(k.clone());
            vars.insert(k, v);
        }
        vars.insert("IFS".into(), " \t\n".into());
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
        let cwd = PathBuf::from(strip_unc(&cwd));
        let ps1 = if interactive {
            if Shell::effective_uid() == 0 {
                "# ".to_string()
            } else {
                "$ ".to_string()
            }
        } else {
            String::new()
        };
        vars.insert("PS1".into(), ps1);
        Shell {
            vars,
            exported,
            positional: Vec::new(),
            arg0: arg0.to_string(),
            functions: HashMap::new(),
            status: 0,
            last_bg: String::new(),
            pid: std::process::id(),
            heredocs: Vec::new(),
            errexit: false,
            xtrace: false,
            nounset: false,
            depth: 0,
            cwd,
            scopes: Vec::new(),
            umask: 0o022,
            cond_depth: 0,
        }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        match name {
            "?" => Some(self.status.to_string()),
            "$" => Some(self.pid.to_string()),
            "#" => Some(self.positional.len().to_string()),
            "!" => Some(self.last_bg.clone()),
            "0" => Some(self.arg0.clone()),
            _ => self.vars.get(name).cloned(),
        }
    }

    pub fn set_var(&mut self, name: &str, value: String) {
        match name {
            "?" | "$" | "#" | "!" | "0" => return, // read-only
            _ => {}
        }
        self.vars.insert(name.to_string(), value);
    }

    pub fn unset_var(&mut self, name: &str) {
        self.vars.remove(name);
        self.exported.remove(name);
    }

    pub fn export(&mut self, name: &str) {
        self.exported.insert(name.to_string());
    }

    /// Environment that child processes should receive.
    pub fn child_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(k, _)| self.exported.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn set_positional(&mut self, args: Vec<String>) {
        self.positional = args;
    }

    /// Adopt a parsed unit's here-doc bodies into the shell-global table and
    /// rewire the AST indices.
    pub fn adopt(&mut self, script: &mut Script) {
        let base = self.heredocs.len();
        crate::ast::rehome(&mut script.nodes, base);
        self.heredocs.append(&mut script.heredocs);
    }

    /// Effective user id (0 → root prompt `#`).
    pub fn effective_uid() -> u32 {
        #[cfg(unix)]
        {
            unsafe { libc::geteuid() }
        }
        #[cfg(not(unix))]
        {
            1000
        }
    }

    /// Resolve a path against the shell's logical cwd.
    pub fn resolve(&self, p: &str) -> PathBuf {
        #[cfg(windows)]
        {
            // Scripts written for Unix often redirect to /dev/null.
            if p == "/dev/null" || p.eq_ignore_ascii_case("nul") {
                return PathBuf::from("NUL");
            }
        }
        let path = std::path::Path::new(p);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        #[cfg(windows)]
        {
            // Root-relative paths like `/tmp/x` get the current drive.
            if p.starts_with('/') || p.starts_with('\\') {
                let cwd = self.cwd.to_string_lossy();
                let drive: String = cwd.chars().take(2).collect();
                if drive.ends_with(':') {
                    return PathBuf::from(format!("{drive}{p}"));
                }
            }
        }
        self.cwd.join(path)
    }

    pub fn is_builtin(name: &str) -> bool {
        matches!(
            name,
            "cd"
                | "pwd"
                | "export"
                | "unset"
                | "echo"
                | "printf"
                | "test"
                | "["
                | "read"
                | "exec"
                | "exit"
                | "set"
                | "shift"
                | "source"
                | "."
                | "wait"
                | "true"
                | "false"
                | ":"
                | "eval"
                | "local"
                | "return"
                | "break"
                | "continue"
                | "umask"
                | "times"
                | "command"
                | "type"
                | "hash"
                | "jobs"
                | "let"
                | "vars"
        )
    }
}
