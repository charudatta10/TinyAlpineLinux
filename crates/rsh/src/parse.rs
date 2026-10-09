//! Recursive-descent parser for the rsh POSIX-subset grammar.

use crate::ast::*;

pub struct Parser {
    src: Vec<char>,
    pos: usize,
    pub heredocs: Vec<Heredoc>,
    heredoc_delims: Vec<String>,
    pending_heredocs: Vec<usize>,
}

const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "in", "while", "until", "do", "done", "case",
    "esac",
];

/// Stop conditions for [`Parser::parse_list`].
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Stop {
    Eof,
    Then,
    Do,
    Fi,
    Done,
    Esac,
    /// `else` / `elif` / `fi` (all end a `then` list).
    ElseElif,
    CloseBrace,
    CloseParen,
    DoubleSemi,
}

pub fn parse(src: &str) -> Result<Script, String> {
    let mut p = Parser {
        src: src.chars().collect(),
        pos: 0,
        heredocs: Vec::new(),
        heredoc_delims: Vec::new(),
        pending_heredocs: Vec::new(),
    };
    let nodes = p.parse_list(Stop::Eof)?;
    if !p.pending_heredocs.is_empty() {
        p.fill_heredocs();
    }
    Ok(Script {
        nodes,
        heredocs: p.heredocs,
    })
}

/// Characters that terminate an unquoted word.
const WORD_STOPS: &[char] = &[' ', '\t', '\n', ';', '&', '|', '<', '>', '(', ')'];

impl Parser {
    // ------------------------------------------------------------ primitives

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        let mut i = self.pos;
        for ch in s.chars() {
            if self.src.get(i) != Some(&ch) {
                return false;
            }
            i += 1;
        }
        true
    }

    fn skip_blanks(&mut self) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') => {
                    self.pos += 1;
                }
                Some('\\') if self.peek_at(1) == Some('\n') => {
                    self.pos += 2;
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Skip blanks, comments, newlines and lone `;` separators. Fills pending
    /// here-docs as soon as a newline is crossed (their bodies start on the
    /// next input line).
    fn skip_separators(&mut self) {
        loop {
            self.skip_blanks();
            match self.peek() {
                Some('\n') => {
                    self.pos += 1;
                    self.fill_heredocs();
                }
                Some(';') if self.peek_at(1) != Some(';') => {
                    self.pos += 1;
                }
                _ => break,
            }
        }
    }

    fn at_keyword(&self, kw: &str) -> bool {
        if !self.starts_with(kw) {
            return false;
        }
        match self.peek_at(kw.chars().count()) {
            None => true,
            Some(c) => !(c.is_alphanumeric() || c == '_'),
        }
    }

    fn peek_ident(&self) -> Option<String> {
        let c = self.peek()?;
        if !(c.is_alphabetic() || c == '_') {
            return None;
        }
        let mut i = self.pos;
        let mut s = String::new();
        while let Some(c) = self.src.get(i) {
            if c.is_alphanumeric() || *c == '_' {
                s.push(*c);
                i += 1;
            } else {
                break;
            }
        }
        Some(s)
    }

    fn read_name(&mut self) -> Option<String> {
        let s = self.peek_ident()?;
        self.pos += s.chars().count();
        Some(s)
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.at_keyword(kw) {
            self.pos += kw.chars().count();
            Ok(())
        } else {
            Err(format!("expected '{kw}', found '{}'", self.peek_or_eof()))
        }
    }

    fn peek_or_eof(&self) -> String {
        match self.peek() {
            Some(c) => {
                if c == '\n' {
                    "newline".to_string()
                } else {
                    c.to_string()
                }
            }
            None => "end of input".to_string(),
        }
    }

    fn at_stop(&self, stop: Stop) -> bool {
        match stop {
            Stop::Eof => false,
            Stop::Then => self.at_keyword("then"),
            Stop::Do => self.at_keyword("do"),
            Stop::Fi => self.at_keyword("fi"),
            Stop::Done => self.at_keyword("done"),
            Stop::Esac => self.at_keyword("esac"),
            Stop::ElseElif => {
                self.at_keyword("else") || self.at_keyword("elif") || self.at_keyword("fi")
            }
            Stop::CloseBrace => self.peek() == Some('}'),
            Stop::CloseParen => self.peek() == Some(')'),
            Stop::DoubleSemi => self.starts_with(";;") || self.at_keyword("esac"),
        }
    }

    /// True when any compound-command stop keyword is at the cursor.
    fn at_any_stop(&self) -> bool {
        self.peek().is_none()
            || self.at_keyword("then")
            || self.at_keyword("do")
            || self.at_keyword("fi")
            || self.at_keyword("done")
            || self.at_keyword("esac")
            || self.at_keyword("else")
            || self.at_keyword("elif")
            || self.starts_with(";;")
    }

    // ------------------------------------------------------------ here-docs

    fn fill_heredocs(&mut self) {
        if self.pending_heredocs.is_empty() {
            return;
        }
        let idxs: Vec<usize> = std::mem::take(&mut self.pending_heredocs);
        for idx in idxs {
            let delim = self.heredoc_delims[idx].clone();
            // The cursor already sits at the start of the body (it just
            // crossed the newline that ended the redirect's line).
            let mut body = String::new();
            loop {
                if self.pos >= self.src.len() {
                    break;
                }
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.pos += 1;
                }
                let line: String = self.src[start..self.pos].iter().collect();
                let at_end = self.pos >= self.src.len();
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                if line == delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
                if at_end {
                    break;
                }
            }
            self.heredocs[idx].body = body;
        }
    }

    // ------------------------------------------------------------ lists

    fn parse_list(&mut self, stop: Stop) -> Result<Vec<Node>, String> {
        let mut nodes: Vec<Node> = Vec::new();
        loop {
            // --- skip separators at command position
            loop {
                self.skip_blanks();
                match self.peek() {
                    Some('\n') => {
                        self.pos += 1;
                        self.fill_heredocs();
                    }
                    Some(';') if self.peek_at(1) != Some(';') => {
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
            if stop == Stop::CloseBrace && self.peek() == Some('}') {
                break;
            }
            if stop == Stop::CloseParen && self.peek() == Some(')') {
                break;
            }
            if self.at_stop(stop) {
                break;
            }
            match self.peek() {
                None => {
                    if matches!(stop, Stop::Eof | Stop::CloseBrace | Stop::CloseParen) {
                        break;
                    }
                    return Err(format!("unexpected end of input (expected a '{stop:?}' marker)"));
                }
                Some('}') if stop != Stop::Eof => {
                    return Err("unexpected '}'".to_string());
                }
                Some(')') if stop == Stop::Eof => {
                    return Err("unexpected ')'".to_string());
                }
                _ => {}
            }

            let before = self.pos;
            let node = self.parse_and_or()?;
            if self.pos == before {
                return Err(format!("syntax error near '{}'", self.peek_or_eof()));
            }
            if !matches!(node, Node::Nop) {
                nodes.push(node);
            }

            // --- terminator after the command
            self.skip_blanks();
            match self.peek() {
                Some('&') => {
                    self.pos += 1;
                    if self.peek() == Some('&') {
                        return Err("syntax error: unexpected '&&'".into());
                    }
                    if let Some(last) = nodes.pop() {
                        nodes.push(Node::Background(Box::new(last)));
                    }
                }
                Some(';') if self.peek_at(1) == Some(';') => {
                    if stop == Stop::DoubleSemi || stop == Stop::Esac {
                        break;
                    }
                    return Err("unexpected ';;'".into());
                }
                Some(';') => {
                    self.pos += 1;
                }
                Some('\n') => {
                    self.pos += 1;
                    self.fill_heredocs();
                }
                _ => {
                    // No separator: valid only when a stop marker / EOF follows.
                    if self.at_stop(stop) || self.peek().is_none() {
                        break;
                    }
                    if stop == Stop::CloseParen && self.peek() == Some(')') {
                        break;
                    }
                    if stop == Stop::CloseBrace && self.peek() == Some('}') {
                        break;
                    }
                    return Err(format!(
                        "syntax error near unexpected token '{}'",
                        self.peek_or_eof()
                    ));
                }
            }
        }
        Ok(nodes)
    }

    // ------------------------------------------------------------ and-or / pipeline

    fn parse_and_or(&mut self) -> Result<Node, String> {
        let mut left = self.parse_pipeline()?;
        loop {
            self.skip_blanks();
            if self.starts_with("&&") {
                self.pos += 2;
                if self.at_any_stop() {
                    return Err("expected command after '&&'".into());
                }
                self.skip_continuation();
                let right = self.parse_pipeline()?;
                left = Node::And(Box::new(left), Box::new(right));
            } else if self.starts_with("||") {
                self.pos += 2;
                if self.at_any_stop() {
                    return Err("expected command after '||'".into());
                }
                self.skip_continuation();
                let right = self.parse_pipeline()?;
                left = Node::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    /// Newlines (and here-docs) may continue a line after `&&` / `||` / `|`.
    fn skip_continuation(&mut self) {
        loop {
            self.skip_blanks();
            if self.peek() == Some('\n') {
                self.pos += 1;
                self.fill_heredocs();
                continue;
            }
            break;
        }
    }

    fn parse_pipeline(&mut self) -> Result<Node, String> {
        let mut negated = false;
        self.skip_blanks();
        if self.peek() == Some('!') {
            match self.peek_at(1) {
                None | Some(' ') | Some('\t') | Some('\n') | Some('(') => {
                    self.pos += 1;
                    negated = true;
                    self.skip_blanks();
                }
                _ => {}
            }
        }
        let first = self.parse_command()?;
        let mut stages = vec![first];
        loop {
            self.skip_blanks();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.pos += 1;
                if self.at_any_stop() {
                    return Err("expected command after '|'".into());
                }
                self.skip_continuation();
                stages.push(self.parse_command()?);
            } else {
                break;
            }
        }
        if stages.len() == 1 && !negated {
            return Ok(stages.pop().unwrap());
        }
        Ok(Node::Pipeline { negated, stages })
    }

    // ------------------------------------------------------------ command

    fn parse_command(&mut self) -> Result<Node, String> {
        self.skip_blanks();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let body = self.parse_list(Stop::CloseParen)?;
                self.skip_blanks();
                if !self.eat(')') {
                    return Err(format!("expected ')', found '{}'", self.peek_or_eof()));
                }
                let n = Node::Subshell(body);
                self.parse_trailing_redirects(n)
            }
            Some('{') => {
                self.pos += 1;
                let body = self.parse_list(Stop::CloseBrace)?;
                self.skip_blanks();
                if !self.eat('}') {
                    return Err("expected '}'".into());
                }
                self.parse_trailing_redirects(Node::Group(body))
            }
            _ if self.at_keyword("if") => {
                self.pos += 2;
                let n = self.parse_if()?;
                self.parse_trailing_redirects(n)
            }
            _ if self.at_keyword("for") => {
                self.pos += 3;
                let n = self.parse_for()?;
                self.parse_trailing_redirects(n)
            }
            _ if self.at_keyword("while") => {
                self.pos += 5;
                let n = self.parse_while(false)?;
                self.parse_trailing_redirects(n)
            }
            _ if self.at_keyword("until") => {
                self.pos += 5;
                let n = self.parse_while(true)?;
                self.parse_trailing_redirects(n)
            }
            _ if self.at_keyword("case") => {
                self.pos += 4;
                let n = self.parse_case()?;
                self.parse_trailing_redirects(n)
            }
            _ if self.at_keyword("function") => {
                self.pos += 8;
                self.skip_blanks();
                let name = self
                    .read_name()
                    .ok_or_else(|| "expected function name".to_string())?;
                self.skip_blanks();
                let body = self.parse_command()?;
                let body = match body {
                    Node::Group(b) => b,
                    other => vec![other],
                };
                return Ok(Node::Func { name, body });
            }
            _ => self.parse_simple_or_func(),
        }
    }

    fn parse_trailing_redirects(&mut self, node: Node) -> Result<Node, String> {
        let mut redirs = Vec::new();
        loop {
            self.skip_blanks();
            match self.try_parse_redirects()? {
                Some(mut v) => redirs.append(&mut v),
                None => break,
            }
        }
        if redirs.is_empty() {
            Ok(node)
        } else {
            Ok(Node::Redirected {
                inner: Box::new(node),
                redirs,
            })
        }
    }

    fn parse_simple_or_func(&mut self) -> Result<Node, String> {
        if let Some(f) = self.try_parse_funcdef()? {
            return Ok(f);
        }
        let mut assigns: Vec<(String, Word)> = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirs: Vec<Redirect> = Vec::new();
        let mut in_assigns = true;

        loop {
            self.skip_blanks();
            if let Some(mut v) = self.try_parse_redirects()? {
                redirs.append(&mut v);
                continue;
            }
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\n' || c == ';' || c == '&' || c == ')' || c == '|' || c == '}' {
                break;
            }
            if c == '<' || c == '>' {
                return Err(format!("syntax error near '{c}'"));
            }
            if in_assigns && (c.is_alphabetic() || c == '_') {
                let save = self.pos;
                if let Some(name) = self.read_name() {
                    if self.peek() == Some('=') {
                        self.pos += 1;
                        let w = self.parse_word(WORD_STOPS)?;
                        assigns.push((name, w));
                        continue;
                    }
                }
                self.pos = save;
            }
            let w = self.parse_word(WORD_STOPS)?;
            if w.is_empty() {
                break;
            }
            in_assigns = false;
            words.push(w);
        }

        if words.is_empty() && assigns.is_empty() && redirs.is_empty() {
            return Ok(Node::Nop);
        }
        if words.is_empty() && assigns.is_empty() {
            return Ok(Node::RedirOnly { redirs });
        }
        Ok(Node::Simple {
            assigns,
            words,
            redirs,
        })
    }

    fn try_parse_funcdef(&mut self) -> Result<Option<Node>, String> {
        let save = self.pos;
        let name = match self.peek_ident() {
            Some(n) if !KEYWORDS.contains(&n.as_str()) => n,
            _ => return Ok(None),
        };
        self.pos += name.chars().count();
        self.skip_blanks();
        if self.peek() != Some('(') {
            self.pos = save;
            return Ok(None);
        }
        self.pos += 1;
        self.skip_blanks();
        if !self.eat(')') {
            self.pos = save;
            return Ok(None);
        }
        self.skip_blanks();
        if self.peek().is_none() || matches!(self.peek(), Some('\n') | Some(';') | Some('&')) {
            // `name()` with body on the next line/after separator: invalid here.
            self.pos = save;
            return Err(format!("function '{name}' has no body"));
        }
        let body_node = self.parse_command()?;
        let body = match body_node {
            Node::Group(b) => b,
            other => vec![other],
        };
        Ok(Some(Node::Func { name, body }))
    }

    // ------------------------------------------------------------ redirects

    fn try_parse_redirects(&mut self) -> Result<Option<Vec<Redirect>>, String> {
        let save = self.pos;
        let mut fd: Option<i32> = None;
        if let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                let start = self.pos;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
                let digits: String = self.src[start..self.pos].iter().collect();
                match self.peek() {
                    Some('<') | Some('>') => {
                        fd = Some(
                            digits
                                .parse()
                                .map_err(|_| "invalid file descriptor".to_string())?,
                        );
                    }
                    _ => {
                        self.pos = save;
                        return Ok(None);
                    }
                }
            }
        }
        let op_char = match self.peek() {
            Some('<') | Some('>') => self.peek().unwrap(),
            _ => {
                self.pos = save;
                return Ok(None);
            }
        };
        self.pos += 1;

        let op = if op_char == '<' {
            if self.peek() == Some('<') {
                self.pos += 1;
                let _dash = self.eat('-');
                return Ok(Some(self.parse_heredoc_redirect(fd.unwrap_or(0))?));
            }
            RedirOp::Read
        } else if self.peek() == Some('>') {
            self.pos += 1;
            RedirOp::Append
        } else if self.peek() == Some('&') {
            self.pos += 1;
            RedirOp::Dup
        } else if self.peek() == Some('|') {
            self.pos += 1;
            RedirOp::Write
        } else {
            RedirOp::Write
        };

        self.skip_blanks();
        let word = self.parse_word(&[' ', '\t', '\n', ';', '&', '|', ')'])?;
        if word.is_empty() {
            if op == RedirOp::Dup && self.eat('-') {
                // `<&-` / `>&-` close the descriptor.
                return Ok(Some(vec![Redirect {
                    fd: fd.unwrap_or(if op_char == '<' { 0 } else { 1 }),
                    op: RedirOp::Dup,
                    word: Some(vec![Part::Lit("-".into(), true)]),
                }]));
            }
            return Err(format!(
                "syntax error: redirection needs a target after '{op_char}'"
            ));
        }
        let default_fd = if op_char == '<' { 0 } else { 1 };
        let fd = fd.unwrap_or(default_fd);

        if op == RedirOp::Dup {
            // `>&file` (non-numeric target) means stdout+stderr to file.
            let numeric = literal_text(&word).map(|t| t.chars().all(|c| c.is_ascii_digit())) == Some(true);
            if !numeric && fd == 1 {
                return Ok(Some(vec![
                    Redirect {
                        fd: 1,
                        op: RedirOp::Write,
                        word: Some(word.clone()),
                    },
                    Redirect {
                        fd: 2,
                        op: RedirOp::Dup,
                        word: Some(vec![Part::Lit("1".into(), true)]),
                    },
                ]));
            }
        }
        Ok(Some(vec![Redirect {
            fd,
            op,
            word: Some(word),
        }]))
    }

    fn parse_heredoc_redirect(&mut self, fd: i32) -> Result<Vec<Redirect>, String> {
        self.skip_blanks();
        // The delimiter is a single word; the remainder of the line (e.g.
        // `| grep x`) must stay available to the parser.
        let start = self.pos;
        let word = self.parse_word(&[' ', '\t', '\n', ';', '&'])?;
        let end = self.pos;
        let quoted = self.src[start..end]
            .iter()
            .any(|c| *c == '\'' || *c == '"');
        let delim = plain_text(&word);
        if delim.is_empty() {
            return Err("here-document needs a delimiter".into());
        }
        let idx = self.heredocs.len();
        self.heredocs.push(Heredoc {
            body: String::new(),
            expand: !quoted,
        });
        self.heredoc_delims.push(delim);
        self.pending_heredocs.push(idx);
        Ok(vec![Redirect {
            fd,
            op: RedirOp::HereDoc(idx, !quoted),
            word: Some(word),
        }])
    }

    // ------------------------------------------------------------ words

    fn parse_word(&mut self, stops: &[char]) -> Result<Word, String> {
        let mut w: Word = Vec::new();
        let mut lit = String::new();
        let mut first = true;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if stops.contains(&c) || c == ' ' || c == '\t' {
                break;
            }
            match c {
                '\\' => {
                    self.pos += 1;
                    match self.bump() {
                        Some('\n') => {} // line continuation: emits nothing
                        Some(other) => {
                            // Backslash-escaped characters are protected from
                            // field splitting and globbing.
                            if !lit.is_empty() {
                                w.push(Part::Lit(std::mem::take(&mut lit), false));
                            }
                            w.push(Part::Lit(other.to_string(), true));
                        }
                        None => lit.push('\\'),
                    }
                }
                '\'' => {
                    self.pos += 1;
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == '\'' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.peek() != Some('\'') {
                        return Err("unterminated single quote".into());
                    }
                    let text: String = self.src[start..self.pos].iter().collect();
                    self.pos += 1;
                    if !lit.is_empty() {
                        w.push(Part::Lit(std::mem::take(&mut lit), false));
                    }
                    w.push(Part::Lit(text, true));
                }
                '"' => {
                    self.pos += 1;
                    let mut dq: Vec<Part> = Vec::new();
                    let mut closed = false;
                    while let Some(c) = self.peek() {
                        if c == '"' {
                            self.pos += 1;
                            closed = true;
                            break;
                        }
                        if c == '\\' {
                            self.pos += 1;
                            match self.bump() {
                                Some('\n') => {}
                                Some(n) if matches!(n, '$' | '`' | '"' | '\\') => {
                                    dq.push(Part::Lit(n.to_string(), true));
                                }
                                Some(n) => {
                                    let mut s = String::from("\\");
                                    s.push(n);
                                    dq.push(Part::Lit(s, true));
                                }
                                None => dq.push(Part::Lit("\\".into(), true)),
                            }
                            continue;
                        }
                        if c == '$' || c == '`' {
                            let mut tmp = Vec::new();
                            self.parse_expansion(&mut tmp)?;
                            dq.extend(tmp);
                            continue;
                        }
                        if c == '\n' {
                            return Err("unterminated double quote".into());
                        }
                        match dq.last_mut() {
                            Some(Part::Lit(t, _)) => t.push(c),
                            _ => dq.push(Part::Lit(c.to_string(), true)),
                        }
                        self.pos += 1;
                    }
                    if !closed {
                        return Err("unterminated double quote".into());
                    }
                    if !lit.is_empty() {
                        w.push(Part::Lit(std::mem::take(&mut lit), false));
                    }
                    if dq.is_empty() {
                        w.push(Part::Lit(String::new(), true));
                    } else {
                        w.push(Part::Dq(dq));
                    }
                }
                '`' => {
                    if !lit.is_empty() {
                        w.push(Part::Lit(std::mem::take(&mut lit), false));
                    }
                    let text = self.read_backtick_sub()?;
                    w.push(Part::CmdSub(text));
                }
                '$' => {
                    if !lit.is_empty() {
                        w.push(Part::Lit(std::mem::take(&mut lit), false));
                    }
                    self.parse_expansion(&mut w)?;
                }
                '~' if first => {
                    w.push(Part::Tilde);
                    self.pos += 1;
                }
                _ => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
            first = false;
        }
        if !lit.is_empty() {
            w.push(Part::Lit(lit, false));
        }
        Ok(w)
    }

    fn parse_expansion(&mut self, w: &mut Word) -> Result<(), String> {
        debug_assert_eq!(self.peek(), Some('$'));
        self.pos += 1;
        let c = match self.peek() {
            Some(c) => c,
            None => {
                w.push(Part::Lit("$".into(), false));
                return Ok(());
            }
        };
        match c {
            '(' => {
                self.pos += 1;
                if self.peek() == Some('(') {
                    self.pos += 1;
                    let start = self.pos;
                    let mut depth = 1usize;
                    while let Some(ch) = self.peek() {
                        if ch == '(' {
                            depth += 1;
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        self.pos += 1;
                    }
                    let expr: String = self.src[start..self.pos].iter().collect();
                    if !self.eat(')') || !self.eat(')') {
                        return Err("unterminated $((".into());
                    }
                    w.push(Part::Arith(expr));
                } else {
                    let text = self.read_paren_sub()?;
                    w.push(Part::CmdSub(text));
                }
            }
            '{' => {
                self.pos += 1;
                // ${#name}
                if self.peek() == Some('#') {
                    self.pos += 1;
                    let name = self
                        .read_name()
                        .ok_or_else(|| "bad substitution".to_string())?;
                    if !self.eat('}') {
                        return Err("unterminated ${".into());
                    }
                    w.push(Part::Length(name));
                    return Ok(());
                }
                let name = self
                    .read_name()
                    .ok_or_else(|| "bad substitution".to_string())?;
                if self.eat('}') {
                    w.push(Part::Var(name));
                    return Ok(());
                }
                let assign = if self.starts_with(":-") {
                    self.pos += 2;
                    false
                } else if self.starts_with(":=") {
                    self.pos += 2;
                    true
                } else if self.peek() == Some('-') {
                    self.pos += 1;
                    false
                } else if self.peek() == Some('=') {
                    self.pos += 1;
                    true
                } else if self.peek() == Some(':') {
                    self.pos += 1;
                    // `${name:offset:len}` is not supported; treat as `}` needed.
                    return Err(format!("unsupported ${{{name}:...}} substitution"));
                } else {
                    return Err(format!("bad substitution for '{name}'"));
                };
                let body = self.parse_brace_body()?;
                w.push(Part::Default(name, body, assign));
            }
            '?' | '@' | '*' | '$' | '!' => {
                self.pos += 1;
                w.push(Part::Special(c));
            }
            '#' => {
                self.pos += 1;
                match self.peek() {
                    Some('{') => {
                        self.pos += 1;
                        let name = self
                            .read_name()
                            .ok_or_else(|| "bad substitution".to_string())?;
                        if !self.eat('}') {
                            return Err("unterminated ${".into());
                        }
                        w.push(Part::Length(name));
                    }
                    _ => w.push(Part::Special('#')),
                }
            }
            d if d.is_ascii_digit() => {
                self.pos += 1;
                w.push(Part::Pos(d as usize - '0' as usize));
            }
            c if c.is_alphabetic() || c == '_' => {
                let name = self.read_name().unwrap();
                w.push(Part::Var(name));
            }
            other => {
                self.pos += 1;
                w.push(Part::Lit(format!("${other}"), false));
            }
        }
        Ok(())
    }

    /// Parse the substitution body of `${name:-...}` up to the matching `}`.
    fn parse_brace_body(&mut self) -> Result<Word, String> {
        let mut w: Word = Vec::new();
        let mut lit = String::new();
        let mut depth = 1usize;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err("unterminated ${".into()),
            };
            match c {
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        break;
                    }
                    lit.push(c);
                    self.pos += 1;
                }
                '{' => {
                    depth += 1;
                    lit.push(c);
                    self.pos += 1;
                }
                '$' => {
                    if !lit.is_empty() {
                        w.push(Part::Lit(std::mem::take(&mut lit), false));
                    }
                    self.parse_expansion(&mut w)?;
                }
                '\'' => {
                    self.pos += 1;
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == '\'' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.peek() != Some('\'') {
                        return Err("unterminated single quote in ${}".into());
                    }
                    let text: String = self.src[start..self.pos].iter().collect();
                    self.pos += 1;
                    w.push(Part::Lit(text, true));
                }
                '"' => {
                    self.pos += 1;
                    let mut dq: Vec<Part> = Vec::new();
                    loop {
                        match self.peek() {
                            None => return Err("unterminated double quote in ${}".into()),
                            Some('"') => {
                                self.pos += 1;
                                break;
                            }
                            Some('\\') => {
                                self.pos += 1;
                                if let Some(n) = self.bump() {
                                    dq.push(Part::Lit(n.to_string(), true));
                                }
                            }
                            Some('$') => {
                                let mut tmp = Vec::new();
                                self.parse_expansion(&mut tmp)?;
                                dq.extend(tmp);
                            }
                            Some(other) => {
                                match dq.last_mut() {
                                    Some(Part::Lit(t, _)) => t.push(other),
                                    _ => dq.push(Part::Lit(other.to_string(), true)),
                                }
                                self.pos += 1;
                            }
                        }
                    }
                    if dq.is_empty() {
                        w.push(Part::Lit(String::new(), true));
                    } else {
                        w.push(Part::Dq(dq));
                    }
                }
                '\\' => {
                    self.pos += 1;
                    if let Some(n) = self.bump() {
                        lit.push(n);
                    }
                }
                _ => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
        }
        if !lit.is_empty() {
            w.push(Part::Lit(lit, false));
        }
        Ok(w)
    }

    fn read_paren_sub(&mut self) -> Result<String, String> {
        let mut depth = 1usize;
        let start = self.pos;
        let mut in_single = false;
        let mut in_double = false;
        while let Some(c) = self.peek() {
            if in_single {
                if c == '\'' {
                    in_single = false;
                }
                self.pos += 1;
                continue;
            }
            if in_double {
                match c {
                    '\\' => {
                        self.pos += 2;
                        continue;
                    }
                    '"' => in_double = false,
                    _ => {}
                }
                self.pos += 1;
                continue;
            }
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '\\' => {
                    self.pos += 1;
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                    continue;
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        let text: String = self.src[start..self.pos].iter().collect();
        if !self.eat(')') {
            return Err("unterminated $( ...".into());
        }
        Ok(text)
    }

    fn read_backtick_sub(&mut self) -> Result<String, String> {
        self.pos += 1;
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '\\' && matches!(self.peek_at(1), Some('`') | Some('$') | Some('\\')) {
                self.pos += 2;
                continue;
            }
            if c == '`' {
                break;
            }
            self.pos += 1;
        }
        let text: String = self.src[start..self.pos].iter().collect();
        if !self.eat('`') {
            return Err("unterminated backtick substitution".into());
        }
        Ok(text)
    }

    // ------------------------------------------------------------ compound

    fn parse_if(&mut self) -> Result<Node, String> {
        // Collect the whole `if`/`elif` chain first, then nest it from the
        // end so that a single trailing `fi` closes the chain.
        let mut branches: Vec<(Vec<Node>, Vec<Node>)> = Vec::new();
        let cond = self.parse_list(Stop::Then)?;
        self.expect_keyword("then")?;
        let then_body = self.parse_list(Stop::ElseElif)?;
        branches.push((cond, then_body));
        let mut else_body: Vec<Node> = Vec::new();
        loop {
            if self.at_keyword("elif") {
                self.pos += 4;
                let cond = self.parse_list(Stop::Then)?;
                self.expect_keyword("then")?;
                let then_body = self.parse_list(Stop::ElseElif)?;
                branches.push((cond, then_body));
                continue;
            }
            if self.at_keyword("else") {
                self.pos += 4;
                else_body = self.parse_list(Stop::Fi)?;
            }
            break;
        }
        self.expect_keyword("fi")?;
        let (last_cond, last_then) = branches.pop().unwrap();
        let mut node = Node::If {
            cond: Box::new(as_seq(last_cond)),
            then_body: last_then,
            else_body,
        };
        for (cond, then_body) in branches.into_iter().rev() {
            node = Node::If {
                cond: Box::new(as_seq(cond)),
                then_body,
                else_body: vec![node],
            };
        }
        Ok(node)
    }

    fn parse_for(&mut self) -> Result<Node, String> {
        self.skip_blanks();
        let var = self
            .read_name()
            .ok_or_else(|| "expected a loop variable name after 'for'".to_string())?;
        self.skip_blanks();
        let mut items: Vec<Word> = Vec::new();
        if self.at_keyword("in") {
            self.pos += 2;
            loop {
                self.skip_blanks();
                if self.peek() == Some('\n') || self.peek() == Some(';') {
                    break;
                }
                if self.at_keyword("do") {
                    break;
                }
                if self.peek().is_none() {
                    break;
                }
                let w = self.parse_word(&[' ', '\t', '\n', ';', '&', ')'])?;
                if w.is_empty() {
                    break;
                }
                items.push(w);
            }
        }
        // Separator before `do`
        self.skip_blanks();
        if self.peek() == Some(';') {
            self.pos += 1;
        } else if self.peek() == Some('\n') {
            self.pos += 1;
            self.fill_heredocs();
        } else if !self.at_keyword("do") {
            return Err(format!(
                "expected ';' or newline before 'do', found '{}'",
                self.peek_or_eof()
            ));
        }
        self.parse_list(Stop::Do)?;
        self.expect_keyword("do")?;
        let body = self.parse_list(Stop::Done)?;
        self.expect_keyword("done")?;
        Ok(Node::For {
            var,
            items,
            body,
        })
    }

    fn parse_while(&mut self, until: bool) -> Result<Node, String> {
        let cond_nodes = self.parse_list(Stop::Do)?;
        self.expect_keyword("do")?;
        let body = self.parse_list(Stop::Done)?;
        self.expect_keyword("done")?;
        Ok(Node::While {
            cond: Box::new(as_seq(cond_nodes)),
            body,
            until,
        })
    }

    fn parse_case(&mut self) -> Result<Node, String> {
        self.skip_blanks();
        let word = self.parse_word(&[' ', '\t', '\n'])?;
        self.skip_separators();
        self.expect_keyword("in")?;
        let mut arms: Vec<CaseArm> = Vec::new();
        loop {
            self.skip_separators();
            if self.at_keyword("esac") {
                self.pos += 4;
                break;
            }
            if self.peek().is_none() {
                return Err("unexpected end of input in 'case'".into());
            }
            // Patterns separated by unquoted `|`, terminated by `)`.
            let mut patterns: Vec<Word> = Vec::new();
            loop {
                let p = self.parse_word(&['|', ')', '\n', ' '])?;
                if p.is_empty() {
                    return Err(format!(
                        "expected a case pattern, found '{}'",
                        self.peek_or_eof()
                    ));
                }
                patterns.push(p);
                if self.eat('|') {
                    continue;
                }
                break;
            }
            if !self.eat(')') {
                return Err(format!("expected ')' after case pattern, found '{}'", self.peek_or_eof()));
            }
            let body = self.parse_list(Stop::DoubleSemi)?;
            arms.push(CaseArm { patterns, body });
            if self.starts_with(";;") {
                self.pos += 2;
                // `;;&` (fallthrough) unsupported: ignore trailing `&`.
                if self.peek() == Some('&') {
                    self.pos += 1;
                }
                continue;
            }
            // Body stopped at `esac`.
            if self.at_keyword("esac") {
                self.pos += 4;
                break;
            }
            if self.peek().is_none() {
                return Err("unexpected end of input in 'case'".into());
            }
        }
        Ok(Node::Case { word, arms })
    }
}

fn as_seq(nodes: Vec<Node>) -> Node {
    if nodes.len() == 1 {
        nodes.into_iter().next().unwrap()
    } else {
        Node::Seq(nodes)
    }
}

/// The quote-stripped literal text of a word (used for here-doc delimiters).
pub fn plain_text(w: &Word) -> String {
    let mut s = String::new();
    for p in w {
        match p {
            Part::Lit(t, _) => s.push_str(t),
            Part::Dq(inner) => s.push_str(&plain_text(inner)),
            Part::Tilde => s.push('~'),
            other => s.push_str(&render_word(&vec![other.clone()])),
        }
    }
    s
}

/// The literal text of a word if it contains only plain literals.
fn literal_text(w: &Word) -> Option<String> {
    let mut s = String::new();
    for p in w {
        match p {
            Part::Lit(t, _) => s.push_str(t),
            _ => return None,
        }
    }
    Some(s)
}
