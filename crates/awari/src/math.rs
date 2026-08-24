//! Dependency-free calculator mode for the launcher.
//!
//! `evaluate` accepts only numeric arithmetic (`+ - * / % ^`, parentheses,
//! unary minus, decimals) — no identifiers or variables — so it is safe to run
//! on arbitrary query text. A non-arithmetic query (an app name, a path, a
//! stray letter) simply fails to parse and the launcher falls back to normal
//! search.

/// Evaluate `expr` as arithmetic and return the result as a display string.
/// Returns `None` when the input is not a complete, finite expression (e.g. it
/// contains letters, or ends mid-expression). At least one operator is required
/// so plain words and bare numbers don't trigger calculator mode.
pub fn evaluate(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() || !has_operator(trimmed) {
        return None;
    }
    let mut p = Parser::new(trimmed);
    let val = p.parse_expr()?;
    p.expect_end()?;
    if !val.is_finite() {
        return None;
    }
    Some(format_number(val))
}

fn has_operator(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '+' | '-' | '*' | '/' | '%' | '^'))
}

fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{:.10}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            chars: s.char_indices().peekable(),
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().map(|(_, c)| *c)
    }

    fn bump(&mut self) -> Option<char> {
        self.chars.next().map(|(_, c)| c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn expect_end(&mut self) -> Option<()> {
        self.skip_ws();
        if self.peek().is_some() {
            None
        } else {
            Some(())
        }
    }

    fn parse_expr(&mut self) -> Option<f64> {
        let mut left = self.parse_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.bump();
                    left += self.parse_term()?;
                }
                Some('-') => {
                    self.bump();
                    left -= self.parse_term()?;
                }
                _ => break,
            }
        }
        Some(left)
    }

    fn parse_term(&mut self) -> Option<f64> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.bump();
                    left *= self.parse_unary()?;
                }
                Some('/') => {
                    self.bump();
                    let r = self.parse_unary()?;
                    if r == 0.0 {
                        return None;
                    }
                    left /= r;
                }
                Some('%') => {
                    self.bump();
                    let r = self.parse_unary()?;
                    if r == 0.0 {
                        return None;
                    }
                    left %= r;
                }
                _ => break,
            }
        }
        Some(left)
    }

    fn parse_factor(&mut self) -> Option<f64> {
        let base = self.parse_primary()?;
        self.skip_ws();
        if self.peek() == Some('^') {
            self.bump();
            let exp = self.parse_factor()?;
            Some(base.powf(exp))
        } else {
            Some(base)
        }
    }

    fn parse_unary(&mut self) -> Option<f64> {
        self.skip_ws();
        match self.peek() {
            Some('-') => {
                self.bump();
                Some(-self.parse_unary()?)
            }
            Some('+') => {
                self.bump();
                self.parse_unary()
            }
            _ => self.parse_factor(),
        }
    }

    fn parse_primary(&mut self) -> Option<f64> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.bump();
                let v = self.parse_expr()?;
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.bump();
                    Some(v)
                } else {
                    None
                }
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_number(&mut self) -> Option<f64> {
        self.skip_ws();
        let mut s = String::new();
        let mut seen_dot = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.bump();
            } else if c == '.' && !seen_dot {
                seen_dot = true;
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if s.is_empty() || s == "." {
            return None;
        }
        s.parse::<f64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Option<String> {
        evaluate(s)
    }

    #[test]
    fn basic_addition() {
        assert_eq!(r("2 + 2"), Some("4".into()));
    }

    #[test]
    fn precedence_and_parens() {
        assert_eq!(r("2 + 3 * 4"), Some("14".into()));
        assert_eq!(r("(2 + 3) * 4"), Some("20".into()));
        assert_eq!(r("2 ^ 3 ^ 2"), Some("512".into()));
    }

    #[test]
    fn division_and_float() {
        assert_eq!(r("10 / 4"), Some("2.5".into()));
        assert_eq!(r("0.1 + 0.2"), Some("0.3".into()));
    }

    #[test]
    fn unary_minus() {
        assert_eq!(r("-2 ^ 2"), Some("-4".into()));
        assert_eq!(r("3 - -2"), Some("5".into()));
    }

    #[test]
    fn rejects_non_math() {
        assert_eq!(r("python"), None);
        assert_eq!(r("2 + 2x"), None);
        assert_eq!(r("node"), None);
        assert_eq!(r("2"), None);
        assert_eq!(r("1 / 0"), None);
    }
}
