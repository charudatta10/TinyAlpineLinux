//! Integer arithmetic for `$(( ... ))`.
//!
//! Grammar (C-like precedence):
//!
//! ```text
//! expr    := or
//! or      := and ( '||' and )*
//! and     := cmp ( '&&' cmp )*
//! cmp     := add ( ('=='|'!='|'<='|'>='|'<'|'>') add )*
//! add     := mul ( ('+'|'-') mul )*
//! mul     := unary ( ('*'|'/'|'%') unary )*
//! unary   := ('-'|'!'|'+') unary | primary
//! primary := number | name | '(' expr ')'
//! ```

pub fn eval(expr: &str, lookup: &dyn Fn(&str) -> i64) -> Result<i64, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut p = Parser { s: chars, i: 0, lookup };
    let v = p.expr()?;
    p.skip_ws();
    if p.i < p.s.len() {
        return Err(format!(
            "unexpected '{}' in arithmetic",
            p.s[p.i]
        ));
    }
    Ok(v)
}

struct Parser<'a> {
    s: Vec<char>,
    i: usize,
    lookup: &'a dyn Fn(&str) -> i64,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.s.get(self.i), Some(' ') | Some('\t') | Some('\n')) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn eat_str(&mut self, s: &str) -> bool {
        self.skip_ws();
        let chars: Vec<char> = s.chars().collect();
        if self.i + chars.len() <= self.s.len() && self.s[self.i..self.i + chars.len()] == chars[..] {
            // Avoid consuming `&&` when looking for `&` etc. (call sites pass
            // two-char operators first, so this is only a concern for `=`).
            self.i += chars.len();
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Result<i64, String> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<i64, String> {
        let mut left = self.and_expr()?;
        loop {
            if self.eat_str("||") {
                let right = self.and_expr()?;
                left = i64::from(left != 0 || right != 0);
            } else {
                return Ok(left);
            }
        }
    }

    fn and_expr(&mut self) -> Result<i64, String> {
        let mut left = self.cmp_expr()?;
        loop {
            if self.eat_str("&&") {
                let right = self.cmp_expr()?;
                left = i64::from(left != 0 && right != 0);
            } else {
                return Ok(left);
            }
        }
    }

    fn cmp_expr(&mut self) -> Result<i64, String> {
        let mut left = self.add_expr()?;
        loop {
            let op = if self.eat_str("==") {
                Some("==")
            } else if self.eat_str("!=") {
                Some("!=")
            } else if self.eat_str("<=") {
                Some("<=")
            } else if self.eat_str(">=") {
                Some(">=")
            } else if self.peek() == Some('<') && self.s.get(self.i + 1) != Some(&'=') {
                self.i += 1;
                Some("<")
            } else if self.peek() == Some('>') && self.s.get(self.i + 1) != Some(&'=') {
                self.i += 1;
                Some(">")
            } else {
                None
            };
            match op {
                Some(op) => {
                    let right = self.add_expr()?;
                    left = i64::from(match op {
                        "==" => left == right,
                        "!=" => left != right,
                        "<=" => left <= right,
                        ">=" => left >= right,
                        "<" => left < right,
                        _ => left > right,
                    });
                }
                None => return Ok(left),
            }
        }
    }

    fn add_expr(&mut self) -> Result<i64, String> {
        let mut left = self.mul_expr()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.i += 1;
                    left = left.wrapping_add(self.mul_expr()?);
                }
                Some('-') => {
                    self.i += 1;
                    left = left.wrapping_sub(self.mul_expr()?);
                }
                _ => return Ok(left),
            }
        }
    }

    fn mul_expr(&mut self) -> Result<i64, String> {
        let mut left = self.unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.i += 1;
                    left = left.wrapping_mul(self.unary()?);
                }
                Some('/') => {
                    self.i += 1;
                    let r = self.unary()?;
                    if r == 0 {
                        return Err("division by zero".into());
                    }
                    left = left.wrapping_div(r);
                }
                Some('%') => {
                    self.i += 1;
                    let r = self.unary()?;
                    if r == 0 {
                        return Err("division by zero".into());
                    }
                    left = left.wrapping_rem(r);
                }
                _ => return Ok(left),
            }
        }
    }

    fn unary(&mut self) -> Result<i64, String> {
        self.skip_ws();
        match self.peek() {
            Some('-') => {
                self.i += 1;
                Ok(self.unary()?.wrapping_neg())
            }
            Some('+') => {
                self.i += 1;
                self.unary()
            }
            Some('!') => {
                self.i += 1;
                Ok(i64::from(self.unary()? == 0))
            }
            Some('~') => {
                self.i += 1;
                Ok(!self.unary()?)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<i64, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("expected a value in arithmetic".into()),
            Some('(') => {
                self.i += 1;
                let v = self.expr()?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err("expected ')' in arithmetic".into());
                }
                self.i += 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() => self.number(),
            Some('$') => {
                // $var / ${var} / $(...) inside arithmetic
                self.i += 1;
                if self.peek() == Some('{') {
                    self.i += 1;
                    let name = self.name()?;
                    self.skip_ws();
                    if self.peek() != Some('}') {
                        return Err("expected '}' in arithmetic".into());
                    }
                    self.i += 1;
                    return Ok((self.lookup)(&name));
                }
                if self.peek() == Some('(') {
                    // $(...) — treat contents as another arithmetic expr.
                    self.i += 1;
                    let v = self.expr()?;
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        self.i += 1;
                    }
                    if self.peek() == Some(')') {
                        self.i += 1;
                    }
                    return Ok(v);
                }
                let name = self.name()?;
                Ok((self.lookup)(&name))
            }
            Some(c) if c.is_alphabetic() || c == '_' => {
                let name = self.name()?;
                Ok((self.lookup)(&name))
            }
            Some(other) => Err(format!("unexpected '{other}' in arithmetic")),
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let start = self.i;
        while matches!(self.s.get(self.i), Some(c) if c.is_alphanumeric() || *c == '_') {
            self.i += 1;
        }
        if start == self.i {
            return Err("expected a name in arithmetic".into());
        }
        Ok(self.s[start..self.i].iter().collect())
    }

    fn number(&mut self) -> Result<i64, String> {
        let start = self.i;
        if self.peek() == Some('0') && matches!(self.s.get(self.i + 1), Some('x') | Some('X')) {
            self.i += 2;
            while matches!(self.s.get(self.i), Some(c) if c.is_ascii_hexdigit()) {
                self.i += 1;
            }
            let text: String = self.s[start..self.i].iter().collect();
            return i64::from_str_radix(&text[2..], 16)
                .map_err(|_| format!("bad hex number '{text}'"));
        }
        while matches!(self.s.get(self.i), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        let text: String = self.s[start..self.i].iter().collect();
        // Leading zeros are octal in POSIX arithmetic.
        if text.len() > 1 && text.starts_with('0') {
            i64::from_str_radix(&text, 8).map_err(|_| format!("bad octal number '{text}'"))
        } else {
            text.parse().map_err(|_| format!("bad number '{text}'"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(e: &str) -> i64 {
        eval(e, &|n| match n {
            "x" => 7,
            "y" => 2,
            _ => 0,
        })
        .unwrap()
    }

    #[test]
    fn precedence() {
        assert_eq!(ev("1+2*3"), 7);
        assert_eq!(ev("(1+2)*3"), 9);
        assert_eq!(ev("10/4"), 2);
        assert_eq!(ev("10%4"), 2);
        assert_eq!(ev("2+3*4-6/2"), 11);
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(ev("1 < 2"), 1);
        assert_eq!(ev("2 <= 2"), 1);
        assert_eq!(ev("3 == 4"), 0);
        assert_eq!(ev("3 != 4"), 1);
        assert_eq!(ev("1 && 0"), 0);
        assert_eq!(ev("1 || 0"), 1);
        assert_eq!(ev("!0"), 1);
    }

    #[test]
    fn variables_and_bases() {
        assert_eq!(ev("x + y"), 9);
        assert_eq!(ev("$x * 2"), 14);
        assert_eq!(ev("0x10"), 16);
        assert_eq!(ev("010"), 8);
    }

    #[test]
    fn unary() {
        assert_eq!(ev("-5 + 2"), -3);
        assert_eq!(ev("--5"), 5);
    }

    #[test]
    fn errors() {
        assert!(eval("1 +", &|_| 0).is_err());
        assert!(eval("1/0", &|_| 0).is_err());
        assert!(eval("(1", &|_| 0).is_err());
    }
}
