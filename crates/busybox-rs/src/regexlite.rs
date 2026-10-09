//! A tiny backtracking regex engine supporting an ERE-ish subset:
//!
//! literals, `.`, `*`, `+`, `?`, alternation `|`, groups `()`, bracket
//! classes `[...]` (with ranges, negation and POSIX `[[:alpha:]]` names),
//! anchors `^` / `$`, and escapes `\d \w \s \D \W \S \. \\` etc.
//!
//! This is deliberately small: enough for `grep -E` in an initramfs.

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class { neg: bool, items: Vec<ClassItem> },
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
    },
    Start,
    End,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone)]
pub struct Regex {
    root: Node,
    /// Pattern was anchored at the start (`^...`), so only position 0 can match.
    anchored_start: bool,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn next(&mut self) -> Option<char> {
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

    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.eat('|') {
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            items.push(self.parse_quantifier(atom)?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Concat(items)),
        }
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, None)
            }
            Some('+') => {
                self.pos += 1;
                (1, None)
            }
            Some('?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some('{') => {
                // Bounded repetition {n}, {n,}, {n,m}
                let save = self.pos;
                self.pos += 1;
                match self.parse_brace() {
                    Some(pair) => pair,
                    None => {
                        self.pos = save;
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        // A quantifier only applies to the last single item, never to a Concat.
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
        })
    }

    fn parse_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let mut min_s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                min_s.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if min_s.is_empty() {
            return None;
        }
        let min: usize = min_s.parse().ok()?;
        if self.eat('}') {
            return Some((min, Some(min)));
        }
        if !self.eat(',') {
            return None;
        }
        if self.eat('}') {
            return Some((min, None));
        }
        let mut max_s = String::new();
        while let Some(c) =self.peek() {
            if c.is_ascii_digit() {
                max_s.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if !self.eat('}') {
            return None;
        }
        let max: usize = max_s.parse().ok()?;
        Some((min, Some(max)))
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        let c = self
            .next()
            .ok_or_else(|| "unexpected end of regex".to_string())?;
        match c {
            '(' => {
                // Non-capturing semantics: group contents only.
                let inner = self.parse_alt()?;
                if !self.eat(')') {
                    return Err("unclosed group".into());
                }
                Ok(inner)
            }
            '[' => self.parse_class(),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '\\' => {
                let e = self
                    .next()
                    .ok_or_else(|| "trailing backslash".to_string())?;
                Ok(match e {
                    'd' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Digit(false)],
                    },
                    'D' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Digit(true)],
                    },
                    'w' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Word(false)],
                    },
                    'W' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Word(true)],
                    },
                    's' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Space(false)],
                    },
                    'S' => Node::Class {
                        neg: false,
                        items: vec![ClassItem::Space(true)],
                    },
                    'n' => Node::Char('\n'),
                    't' => Node::Char('\t'),
                    'r' => Node::Char('\r'),
                    other => Node::Char(other),
                })
            }
            other => Ok(Node::Char(other)),
        }
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = match self.next() {
                Some(c) => c,
                None => return Err("unclosed bracket".into()),
            };
            if c == ']' && !first {
                break;
            }
            first = false;
            if c == '[' && self.peek() == Some(':') {
                // POSIX class [[:alpha:]]
                self.pos += 1; // consume ':'
                let mut name = String::new();
                loop {
                    match self.next() {
                        Some(':') if self.peek() == Some(']') => {
                            self.pos += 1; // consume ']'
                            break;
                        }
                        Some(ch) => name.push(ch),
                        None => return Err("unclosed posix class".into()),
                    }
                }
                match name.as_str() {
                    "alpha" => {
                        items.push(ClassItem::Range('a', 'z'));
                        items.push(ClassItem::Range('A', 'Z'));
                    }
                    "digit" => items.push(ClassItem::Digit(false)),
                    "alnum" => {
                        items.push(ClassItem::Digit(false));
                        items.push(ClassItem::Range('a', 'z'));
                        items.push(ClassItem::Range('A', 'Z'));
                    }
                    "space" | "blank" => items.push(ClassItem::Space(false)),
                    "upper" => items.push(ClassItem::Range('A', 'Z')),
                    "lower" => items.push(ClassItem::Range('a', 'z')),
                    "punct" => {
                        for (a, b) in [('!', '/'), (':', '@'), ('[', '`'), ('{', '~')] {
                            items.push(ClassItem::Range(a, b));
                        }
                    }
                    "xdigit" => {
                        items.push(ClassItem::Digit(false));
                        for (a, b) in [('a', 'f'), ('A', 'F')] {
                            items.push(ClassItem::Range(a, b));
                        }
                    }
                    other => return Err(format!("unsupported class [[:{other}:]]")),
                }
                continue;
            }
            // Range?
            if self.peek() == Some('-')
                && self.chars.get(self.pos + 1).copied().unwrap_or(']') != ']'
            {
                self.pos += 1; // consume '-'
                let hi = self.next().ok_or_else(|| "unclosed range".to_string())?;
                items.push(ClassItem::Range(c, hi));
            } else if c == '\\' {
                let e = self
                    .next()
                    .ok_or_else(|| "trailing backslash in class".to_string())?;
                match e {
                    'd' => items.push(ClassItem::Digit(false)),
                    'w' => items.push(ClassItem::Word(false)),
                    's' => items.push(ClassItem::Space(false)),
                    'n' => items.push(ClassItem::Ch('\n')),
                    't' => items.push(ClassItem::Ch('\t')),
                    other => items.push(ClassItem::Ch(other)),
                }
            } else {
                items.push(ClassItem::Ch(c));
            }
        }
        Ok(Node::Class { neg, items })
    }
}

fn class_matches(neg: bool, items: &[ClassItem], c: char) -> bool {
    let hit = items.iter().any(|it| match it {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(a, b) => *a <= c && c <= *b,
        ClassItem::Digit(n) => c.is_ascii_digit() != *n,
        ClassItem::Word(n) => (c.is_alphanumeric() || c == '_') != *n,
        ClassItem::Space(n) => c.is_whitespace() != *n,
    });
    hit != neg
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, String> {
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
        };
        let anchored_start = pattern.starts_with('^');
        let root = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return Err(format!(
                "unexpected '{}' in regex",
                p.chars[p.pos]
            ));
        }
        Ok(Regex {
            root,
            anchored_start,
        })
    }

    /// Find the byte-free char index of the leftmost match starting at or after
    /// `from`, returning (start, end).
    pub fn find_at(&self, hay: &[char], from: usize) -> Option<(usize, usize)> {
        if self.anchored_start {
            if from > 0 {
                return None;
            }
            return match_node(&self.root, hay, 0, &mut |_, p| Some(p)).map(|e| (0, e));
        }
        for i in from..=hay.len() {
            if let Some(end) = match_node(&self.root, hay, i, &mut |_, p| Some(p)) {
                return Some((i, end));
            }
        }
        None
    }

    pub fn is_match(&self, hay: &str) -> bool {
        let chars: Vec<char> = hay.chars().collect();
        self.find_at(&chars, 0).is_some()
    }
}

/// Backtracking matcher with an explicit continuation.
///
/// Every arm must invoke `k` with the position *after* this node consumed its
/// input; `k` decides whether the remainder of the pattern still matches.
fn match_node(node: &Node, hay: &[char], pos: usize, k: &mut dyn FnMut(&Node, usize) -> Option<usize>) -> Option<usize> {
    match node {
        Node::Empty => k(node, pos),
        Node::Char(c) => {
            if hay.get(pos) == Some(c) {
                k(node, pos + 1)
            } else {
                None
            }
        }
        Node::Any => {
            if pos < hay.len() && hay[pos] != '\n' {
                k(node, pos + 1)
            } else {
                None
            }
        }
        Node::Class { neg, items } => {
            if pos < hay.len() && class_matches(*neg, items, hay[pos]) {
                k(node, pos + 1)
            } else {
                None
            }
        }
        Node::Start => {
            if pos == 0 {
                k(node, pos)
            } else {
                None
            }
        }
        Node::End => {
            if pos == hay.len() {
                k(node, pos)
            } else {
                None
            }
        }
        Node::Concat(items) => match_seq(items, hay, pos, k),
        Node::Alt(branches) => {
            for b in branches {
                if let Some(end) = match_node(b, hay, pos, k) {
                    return Some(end);
                }
            }
            None
        }
        Node::Repeat {
            node: inner,
            min,
            max,
        } => match_repeat(inner, *min, *max, hay, pos, k),
    }
}

fn match_seq(items: &[Node], hay: &[char], pos: usize, k: &mut dyn FnMut(&Node, usize) -> Option<usize>) -> Option<usize> {
    if items.is_empty() {
        return k(&Node::Empty, pos);
    }
    let (first, rest) = items.split_first().unwrap();
    match_node(first, hay, pos, &mut |_, p| match_seq(rest, hay, p, k))
}

fn match_repeat(
    inner: &Node,
    min: usize,
    max: Option<usize>,
    hay: &[char],
    pos: usize,
    k: &mut dyn FnMut(&Node, usize) -> Option<usize>,
) -> Option<usize> {
    if min > 0 {
        return match_node(inner, hay, pos, &mut |_, p| {
            if p == pos {
                // Zero-width guard against infinite recursion.
                return k(&Node::Empty, p);
            }
            match_repeat(inner, min - 1, max.map(|m| m - 1), hay, p, k)
        });
    }
    // min == 0: try zero occurrences first (greedy: prefer more repetitions).
    if max == Some(0) {
        return k(&Node::Empty, pos);
    }
    match_node(inner, hay, pos, &mut |_, p| {
        if p == pos {
            return k(&Node::Empty, p);
        }
        match_repeat(inner, 0, max.map(|m| m - 1), hay, p, k)
    })
    .or_else(|| k(&Node::Empty, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pat: &str, hay: &str) -> Option<(usize, usize)> {
        Regex::new(pat).ok()?.find_at(&hay.chars().collect::<Vec<_>>(), 0)
    }

    #[test]
    fn literals_and_any() {
        assert!(find("abc", "xxabcyy").is_some());
        assert!(find("a.c", "abc").is_some());
        assert!(find("a.c", "ac").is_none());
        assert!(find("abc", "ab").is_none());
    }

    #[test]
    fn quantifiers() {
        assert!(find("ab*c", "ac").is_some());
        assert!(find("ab*c", "abbbbc").is_some());
        assert!(find("ab+c", "ac").is_none());
        assert!(find("ab?c", "ac").is_some());
        assert!(find("a{2,3}", "a").is_none());
        assert!(find("a{2,3}", "aa").is_some());
        assert!(find("a{2,3}", "aaaa").is_some());
    }

    #[test]
    fn anchors() {
        assert!(find("^abc", "abcdef").is_some());
        assert!(find("^abc", "xabc").is_none());
        assert!(find("abc$", "zabc").is_some());
        assert!(find("abc$", "abcd").is_none());
    }

    #[test]
    fn classes_and_alt() {
        assert!(find("[0-9]+", "ab123cd").is_some());
        assert!(find("[^0-9]+", "123ab").is_some());
        assert!(find("^(foo|bar)$", "bar").is_some());
        assert!(find("^(foo|bar)$", "baz").is_none());
        assert!(find("[[:alpha:]]+", "123abc").is_some());
        assert!(find(r"\d+", "x42y").is_some());
        assert!(find(r"\w+", "  hi ").is_some());
    }

    #[test]
    fn greedy_backtracks() {
        // Greedy .* must backtrack to satisfy the trailing literal.
        assert!(find(r"<.*> ", "<a> <b> ").is_some());
        assert_eq!(find(r"a*", "aaa").map(|(s, e)| (s, e)), Some((0, 3)));
    }
}
