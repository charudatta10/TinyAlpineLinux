//! AST for the rsh shell.

/// One piece of a (possibly quoted / expanded) word.
#[derive(Debug, Clone)]
pub enum Part {
    /// Literal text (quotes already removed). `quoted` suppresses splitting
    /// and globbing for this fragment.
    Lit(String, bool),
    /// `$name` or `${name}` — parameter expansion.
    Var(String),
    /// `$?`, `$$`, `$#`, `$@`, `$*`, `$!`
    Special(char),
    /// `$0` .. `$9` positional parameters (index is 0-based, includes `$0`).
    Pos(usize),
    /// `$(script)` or backticks — raw script source.
    CmdSub(String),
    /// `$((expr))` arithmetic expansion.
    Arith(String),
    /// `${name:-word}` (or `:=`) — default / assign-if-unset-or-empty.
    Default(String, Vec<Part>, bool),
    /// `${#name}` — string length.
    Length(String),
    /// Leading `~` (tilde expansion).
    Tilde,
    /// A double-quoted region: children expand without splitting/globbing
    /// (except `$@`, which still yields one field per parameter).
    Dq(Vec<Part>),
}

pub type Word = Vec<Part>;

/// A redirection attached to a command.
#[derive(Debug, Clone)]
pub struct Redirect {
    /// File descriptor the redirection applies to (0/1/2/n).
    pub fd: i32,
    pub op: RedirOp,
    /// Target word (path) or the fd number as text for `n>&m`.
    pub word: Option<Word>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirOp {
    /// `<`
    Read,
    /// `>`
    Write,
    /// `>>`
    Append,
    /// `>&` / `<&` duplication
    Dup,
    /// `<<` / `<<-` here-document; index into [`Script::heredocs`].
    HereDoc(usize, bool),
}

/// A here-document body collected when the parser reaches the end of line.
#[derive(Debug, Clone)]
pub struct Heredoc {
    pub body: String,
    /// Delimiter was unquoted → parameter expansion applies.
    pub expand: bool,
}

#[derive(Debug, Clone)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Vec<Node>,
}

#[derive(Debug, Clone)]
pub enum Node {
    /// Simple command: leading assignments, words, redirections.
    Simple {
        assigns: Vec<(String, Word)>,
        words: Vec<Word>,
        redirs: Vec<Redirect>,
    },
    /// Redirections with no command (`> log`) — apply to the current shell.
    RedirOnly {
        redirs: Vec<Redirect>,
    },
    Pipeline {
        negated: bool,
        stages: Vec<Node>,
    },
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    /// `;` / newline separated sequence.
    Seq(Vec<Node>),
    Background(Box<Node>),
    If {
        cond: Box<Node>,
        then_body: Vec<Node>,
        else_body: Vec<Node>,
    },
    For {
        var: String,
        items: Vec<Word>,
        body: Vec<Node>,
    },
    While {
        cond: Box<Node>,
        body: Vec<Node>,
        /// `until` instead of `while`
        until: bool,
    },
    Case {
        word: Word,
        arms: Vec<CaseArm>,
    },
    Subshell(Vec<Node>),
    Group(Vec<Node>),
    Func {
        name: String,
        body: Vec<Node>,
    },
    /// Compound command with trailing redirections.
    Redirected {
        inner: Box<Node>,
        redirs: Vec<Redirect>,
    },
    Nop,
}

/// A parsed script (or `-c` command line).
#[derive(Debug, Clone, Default)]
pub struct Script {
    pub nodes: Vec<Node>,
    pub heredocs: Vec<Heredoc>,
}

impl Script {
    /// Re-render the script as shell source (used to hand compound commands
    /// to a child `rsh` process).
    pub fn to_source(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            out.push_str(&render(n, false));
            out.push_str("; ");
        }
        out
    }
}

/// Render a node back to shell source. This is only used for debugging and
/// `set -x`; it is intentionally best-effort.
pub fn render(node: &Node, _inner: bool) -> String {
    match node {
        Node::Simple {
            assigns,
            words,
            redirs,
        } => {
            let mut s = String::new();
            for (k, w) in assigns {
                s.push_str(k);
                s.push('=');
                s.push_str(&render_word(w));
                s.push(' ');
            }
            for w in words {
                s.push_str(&render_word(w));
                s.push(' ');
            }
            s.push_str(&render_redirs(redirs));
            s.trim_end().to_string()
        }
        Node::RedirOnly { redirs } => render_redirs(redirs),
        Node::Pipeline { negated, stages } => {
            let parts: Vec<String> = stages.iter().map(|s| render(s, false)).collect();
            format!("{}{}", if *negated { "! " } else { "" }, parts.join(" | "))
        }
        Node::And(a, b) => format!("{} && {}", render(a, false), render(b, false)),
        Node::Or(a, b) => format!("{} || {}", render(a, false), render(b, false)),
        Node::Seq(list) => list
            .iter()
            .map(|n| render(n, false))
            .collect::<Vec<_>>()
            .join("; "),
        Node::Background(inner) => format!("{} &", render(inner, false)),
        Node::If {
            cond,
            then_body,
            else_body,
        } => {
            let mut s = format!("if {}; then ", render(cond, false));
            for n in then_body {
                s.push_str(&render(n, false));
                s.push_str("; ");
            }
            if !else_body.is_empty() {
                s.push_str("else ");
                for n in else_body {
                    s.push_str(&render(n, false));
                    s.push_str("; ");
                }
            }
            s.push_str("fi");
            s
        }
        Node::For { var, items, body } => {
            let mut s = format!("for {var} in");
            for i in items {
                s.push(' ');
                s.push_str(&render_word(i));
            }
            s.push_str("; do ");
            for n in body {
                s.push_str(&render(n, false));
                s.push_str("; ");
            }
            s.push_str("done");
            s
        }
        Node::While { cond, body, until } => {
            let kw = if *until { "until" } else { "while" };
            let mut s = format!("{kw} {}; do ", render(cond, false));
            for n in body {
                s.push_str(&render(n, false));
                s.push_str("; ");
            }
            s.push_str("done");
            s
        }
        Node::Case { word, arms } => {
            let mut s = format!("case {} in ", render_word(word));
            for a in arms {
                let pats: Vec<String> = a.patterns.iter().map(render_word).collect();
                s.push_str(&pats.join("|"));
                s.push_str(") ");
                for n in &a.body {
                    s.push_str(&render(n, false));
                    s.push_str("; ");
                }
                s.push_str(";; ");
            }
            s.push_str("esac");
            s
        }
        Node::Subshell(body) | Node::Group(body) => {
            let paren = matches!(node, Node::Subshell(_));
            let open = if paren { "(" } else { "{" };
            let close = if paren { ")" } else { "}" };
            let mut s = format!("{open} ");
            for n in body {
                s.push_str(&render(n, false));
                s.push_str("; ");
            }
            s.push(' ');
            s.push_str(close);
            s
        }
        Node::Func { name, body } => {
            let mut s = format!("{name}() {{ ");
            for n in body {
                s.push_str(&render(n, false));
                s.push_str("; ");
            }
            s.push_str("}");
            s
        }
        Node::Redirected { inner, redirs } => {
            format!("{} {}", render(inner, false), render_redirs(redirs))
        }
        Node::Nop => String::new(),
    }
}

fn render_redirs(redirs: &[Redirect]) -> String {
    let mut s = String::new();
    for r in redirs {
        let op = match r.op {
            RedirOp::Read => "<",
            RedirOp::Write => ">",
            RedirOp::Append => ">>",
            RedirOp::Dup => ">&",
            RedirOp::HereDoc(..) => "<<EOF ",
        };
        s.push_str(&format!("{}{}", r.fd, op));
        if let Some(w) = &r.word {
            s.push_str(&render_word(w));
        }
        s.push(' ');
    }
    s
}

/// Offset all here-document indices in `nodes` by `base`, so a parsed unit
/// can append its here-doc bodies onto a shell-global table.
pub fn rehome(nodes: &mut [Node], base: usize) {
    for n in nodes {
        rehome_one(n, base);
    }
}

fn rehome_one(n: &mut Node, base: usize) {
    match n {
        Node::Simple { redirs, .. } | Node::RedirOnly { redirs } => {
            rehome_redirs(redirs, base);
        }
        Node::Redirected { inner, redirs } => {
            rehome_redirs(redirs, base);
            rehome_one(inner, base);
        }
        Node::Pipeline { stages, .. } => rehome(stages, base),
        Node::And(a, b) | Node::Or(a, b) => {
            rehome_one(a, base);
            rehome_one(b, base);
        }
        Node::Seq(list) => rehome(list, base),
        Node::Background(inner) => rehome_one(inner, base),
        Node::If {
            cond,
            then_body,
            else_body,
        } => {
            rehome_one(cond, base);
            rehome(then_body, base);
            rehome(else_body, base);
        }
        Node::For { body, .. } => rehome(body, base),
        Node::While { cond, body, .. } => {
            rehome_one(cond, base);
            rehome(body, base);
        }
        Node::Case { arms, .. } => {
            for a in arms {
                rehome(&mut a.body, base);
            }
        }
        Node::Subshell(body) | Node::Group(body) | Node::Func { body, .. } => rehome(body, base),
        Node::Nop => {}
    }
}

fn rehome_redirs(redirs: &mut [Redirect], base: usize) {
    for r in redirs {
        if let RedirOp::HereDoc(i, _) = &mut r.op {
            *i += base;
        }
    }
}

pub fn render_word(w: &Word) -> String {
    let mut s = String::new();
    for p in w {
        match p {
            Part::Lit(t, _) => s.push_str(t),
            Part::Var(v) => {
                s.push('$');
                s.push_str(v);
            }
            Part::Special(c) => {
                s.push('$');
                s.push(*c);
            }
            Part::Pos(i) => {
                s.push('$');
                s.push_str(&i.to_string());
            }
            Part::CmdSub(c) => {
                s.push_str("$(");
                s.push_str(c);
                s.push(')');
            }
            Part::Arith(a) => {
                s.push_str("$((");
                s.push_str(a);
                s.push_str("))");
            }
            Part::Default(v, body, assign) => {
                s.push_str(&format!(
                    "${{{v}{}{}}}",
                    if *assign { ":=" } else { ":-" },
                    render_word(body)
                ));
            }
            Part::Length(v) => {
                s.push_str(&format!("${{#{v}}}"));
            }
            Part::Tilde => s.push('~'),
            Part::Dq(inner) => {
                s.push('"');
                s.push_str(&render_word(inner));
                s.push('"');
            }
        }
    }
    s
}
