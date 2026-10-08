//! A small, total, deterministic expression language for preconditions, postconditions,
//! verification checks, conditions and invariants.
//!
//! ```text
//! expr    := or
//! or      := and ( "||" and )*
//! and     := unary ( "&&" unary )*
//! unary   := "!" unary | compare
//! compare := primary ( ("=="|"!="|"<"|"<="|">"|">=") primary )?
//! primary := literal | path | "exists" "(" path ")" | "(" expr ")"
//! path    := ident ( "." ident | "[" (string|integer) "]" )*
//! ```
//!
//! Evaluation is three-valued (Kleene logic). A missing path is UNKNOWN, never FALSE, so that
//! "we could not see it" is never confused with "it is not so".

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    pub fn from_bool(b: bool) -> Self {
        if b {
            Self::True
        } else {
            Self::False
        }
    }

    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    /// Conjunction of all values; TRUE for an empty iterator.
    pub fn all(items: impl IntoIterator<Item = Truth>) -> Self {
        items.into_iter().fold(Self::True, Self::and)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expression error at offset {position}: {message}")]
pub struct ExprError {
    pub position: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<Segment>);

impl Path {
    pub fn lookup<'v>(&self, root: &'v Value) -> Option<&'v Value> {
        let mut cur = root;
        for seg in &self.0 {
            cur = match (seg, cur) {
                (Segment::Key(k), Value::Object(m)) => m.get(k)?,
                (Segment::Index(i), Value::Array(a)) => a.get(*i)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    pub fn first(&self) -> Option<&str> {
        match self.0.first() {
            Some(Segment::Key(k)) => Some(k),
            _ => None,
        }
    }

    pub fn second(&self) -> Option<&str> {
        match self.0.get(1) {
            Some(Segment::Key(k)) => Some(k),
            _ => None,
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            match seg {
                Segment::Key(k) if i == 0 => write!(f, "{k}")?,
                Segment::Key(k) => write!(f, ".{k}")?,
                Segment::Index(n) => write!(f, "[{n}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Path(Path),
    Exists(Path),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(CmpOp, Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Evaluates as a condition.
    pub fn eval(&self, ctx: &Value) -> Truth {
        match self {
            Expr::Literal(Value::Bool(b)) => Truth::from_bool(*b),
            Expr::Literal(_) => Truth::Unknown,
            Expr::Path(p) => match p.lookup(ctx) {
                Some(Value::Bool(b)) => Truth::from_bool(*b),
                _ => Truth::Unknown,
            },
            Expr::Exists(p) => Truth::from_bool(matches!(p.lookup(ctx), Some(v) if !v.is_null())),
            Expr::Not(e) => e.eval(ctx).not(),
            Expr::And(a, b) => a.eval(ctx).and(b.eval(ctx)),
            Expr::Or(a, b) => a.eval(ctx).or(b.eval(ctx)),
            Expr::Compare(op, a, b) => compare(*op, a.value(ctx), b.value(ctx)),
        }
    }

    /// Evaluates as a value; `None` means unknown.
    fn value(&self, ctx: &Value) -> Option<Value> {
        match self {
            Expr::Literal(v) => Some(v.clone()),
            Expr::Path(p) => p.lookup(ctx).cloned(),
            other => match other.eval(ctx) {
                Truth::True => Some(Value::Bool(true)),
                Truth::False => Some(Value::Bool(false)),
                Truth::Unknown => None,
            },
        }
    }

    /// All paths the expression reads, for static validation.
    pub fn paths(&self) -> Vec<&Path> {
        let mut out = Vec::new();
        self.collect_paths(&mut out);
        out
    }

    fn collect_paths<'a>(&'a self, out: &mut Vec<&'a Path>) {
        match self {
            Expr::Literal(_) => {}
            Expr::Path(p) | Expr::Exists(p) => out.push(p),
            Expr::Not(e) => e.collect_paths(out),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Compare(_, a, b) => {
                a.collect_paths(out);
                b.collect_paths(out);
            }
        }
    }
}

fn compare(op: CmpOp, a: Option<Value>, b: Option<Value>) -> Truth {
    let (Some(a), Some(b)) = (a, b) else {
        return Truth::Unknown;
    };
    match op {
        CmpOp::Eq => Truth::from_bool(values_equal(&a, &b)),
        CmpOp::Ne => Truth::from_bool(!values_equal(&a, &b)),
        _ => {
            let ord = match (&a, &b) {
                (Value::Number(x), Value::Number(y)) => {
                    match x.as_f64().zip(y.as_f64()).and_then(|(x, y)| x.partial_cmp(&y)) {
                        Some(o) => o,
                        None => return Truth::Unknown,
                    }
                }
                (Value::String(x), Value::String(y)) => x.cmp(y),
                _ => return Truth::Unknown,
            };
            Truth::from_bool(match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
                CmpOp::Eq | CmpOp::Ne => unreachable!(),
            })
        }
    }
}

/// Structural equality where numbers compare numerically (`100 == 100.0`).
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64, bool),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Bang,
    AndAnd,
    OrOr,
    Cmp(CmpOp),
}

fn lex(src: &str) -> Result<Vec<(usize, Tok)>, ExprError> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let err = |pos: usize, m: &str| ExprError {
        position: pos,
        message: m.to_string(),
    };
    while i < chars.len() {
        let (pos, c) = chars[i];
        let next = chars.get(i + 1).map(|x| x.1);
        match c {
            c if c.is_whitespace() => i += 1,
            '(' | ')' | '[' | ']' | '.' => {
                out.push((
                    pos,
                    match c {
                        '(' => Tok::LParen,
                        ')' => Tok::RParen,
                        '[' => Tok::LBracket,
                        ']' => Tok::RBracket,
                        _ => Tok::Dot,
                    },
                ));
                i += 1;
            }
            '&' if next == Some('&') => {
                out.push((pos, Tok::AndAnd));
                i += 2;
            }
            '|' if next == Some('|') => {
                out.push((pos, Tok::OrOr));
                i += 2;
            }
            '=' if next == Some('=') => {
                out.push((pos, Tok::Cmp(CmpOp::Eq)));
                i += 2;
            }
            '!' if next == Some('=') => {
                out.push((pos, Tok::Cmp(CmpOp::Ne)));
                i += 2;
            }
            '!' => {
                out.push((pos, Tok::Bang));
                i += 1;
            }
            '<' | '>' => {
                let eq = next == Some('=');
                let op = match (c, eq) {
                    ('<', false) => CmpOp::Lt,
                    ('<', true) => CmpOp::Le,
                    ('>', false) => CmpOp::Gt,
                    _ => CmpOp::Ge,
                };
                out.push((pos, Tok::Cmp(op)));
                i += if eq { 2 } else { 1 };
            }
            '"' | '\'' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                loop {
                    let Some(&(_, ch)) = chars.get(i) else {
                        return Err(err(pos, "unterminated string"));
                    };
                    i += 1;
                    if ch == quote {
                        break;
                    }
                    if ch == '\\' {
                        let Some(&(_, esc)) = chars.get(i) else {
                            return Err(err(pos, "unterminated escape"));
                        };
                        i += 1;
                        s.push(match esc {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    } else {
                        s.push(ch);
                    }
                }
                out.push((pos, Tok::Str(s)));
            }
            c if c.is_ascii_digit() || (c == '-' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let start = i;
                i += 1;
                let mut is_int = true;
                while let Some(&(_, ch)) = chars.get(i) {
                    if ch.is_ascii_digit() {
                        i += 1;
                    } else if ch == '.'
                        && is_int
                        && chars.get(i + 1).is_some_and(|x| x.1.is_ascii_digit())
                    {
                        is_int = false;
                        i += 1;
                    } else {
                        break;
                    }
                }
                let text: String = chars[start..i].iter().map(|x| x.1).collect();
                let n: f64 = text.parse().map_err(|_| err(pos, "invalid number"))?;
                out.push((pos, Tok::Num(n, is_int)));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|x| x.1.is_ascii_alphanumeric() || x.1 == '_')
                {
                    i += 1;
                }
                out.push((pos, Tok::Ident(chars[start..i].iter().map(|x| x.1).collect())));
            }
            _ => return Err(err(pos, &format!("unexpected character '{c}'"))),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Parser

struct Parser {
    toks: Vec<(usize, Tok)>,
    pos: usize,
    len: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.1)
    }

    fn offset(&self) -> usize {
        self.toks.get(self.pos).map(|t| t.0).unwrap_or(self.len)
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ExprError> {
        Err(ExprError {
            position: self.offset(),
            message: message.into(),
        })
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.1.clone());
        self.pos += 1;
        t
    }

    fn expect(&mut self, tok: Tok, what: &str) -> Result<(), ExprError> {
        if self.peek() == Some(&tok) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(format!("expected {what}"))
        }
    }

    fn or(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::OrOr) {
            self.pos += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.unary()?;
        while self.peek() == Some(&Tok::AndAnd) {
            self.pos += 1;
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, ExprError> {
        if self.peek() == Some(&Tok::Bang) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Expr, ExprError> {
        let left = self.primary()?;
        if let Some(Tok::Cmp(op)) = self.peek().cloned() {
            self.pos += 1;
            let right = self.primary()?;
            return Ok(Expr::Compare(op, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn primary(&mut self) -> Result<Expr, ExprError> {
        match self.bump() {
            Some(Tok::LParen) => {
                let e = self.or()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Some(Tok::Str(s)) => Ok(Expr::Literal(Value::String(s))),
            Some(Tok::Num(n, is_int)) => Ok(Expr::Literal(number(n, is_int))),
            Some(Tok::Ident(id)) => match id.as_str() {
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                "null" => Ok(Expr::Literal(Value::Null)),
                "exists" if self.peek() == Some(&Tok::LParen) => {
                    self.pos += 1;
                    let Some(Tok::Ident(first)) = self.bump() else {
                        return self.err("exists() takes a path");
                    };
                    let p = self.path_rest(first)?;
                    self.expect(Tok::RParen, "')'")?;
                    Ok(Expr::Exists(p))
                }
                _ => Ok(Expr::Path(self.path_rest(id)?)),
            },
            Some(_) => {
                self.pos -= 1;
                self.err("expected a value, path or '('")
            }
            None => self.err("unexpected end of expression"),
        }
    }

    fn path_rest(&mut self, first: String) -> Result<Path, ExprError> {
        let mut segs = vec![Segment::Key(first)];
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    match self.bump() {
                        Some(Tok::Ident(k)) => segs.push(Segment::Key(k)),
                        _ => return self.err("expected a field name after '.'"),
                    }
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    match self.bump() {
                        Some(Tok::Str(k)) => segs.push(Segment::Key(k)),
                        Some(Tok::Num(n, true)) if n >= 0.0 => segs.push(Segment::Index(n as usize)),
                        _ => return self.err("expected a string key or index in '[]'"),
                    }
                    self.expect(Tok::RBracket, "']'")?;
                }
                _ => return Ok(Path(segs)),
            }
        }
    }
}

fn number(n: f64, is_int: bool) -> Value {
    if is_int && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

pub fn parse(src: &str) -> Result<Expr, ExprError> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(ExprError {
            position: 0,
            message: "empty expression".into(),
        });
    }
    let mut p = Parser {
        toks,
        pos: 0,
        len: src.len(),
    };
    let e = p.or()?;
    if p.pos < p.toks.len() {
        return p.err("unexpected trailing input");
    }
    Ok(e)
}

/// Parses a bare path such as `facts.deploy.image` (used by templates).
pub fn parse_path(src: &str) -> Result<Path, ExprError> {
    match parse(src)? {
        Expr::Path(p) => Ok(p),
        _ => Err(ExprError {
            position: 0,
            message: format!("'{src}' is not a path"),
        }),
    }
}

/// Parses and evaluates in one step. Parse errors evaluate to UNKNOWN; validation rejects them
/// before any transaction is sealed, so this only matters for ad-hoc use.
pub fn eval_str(src: &str, ctx: &Value) -> Truth {
    parse(src).map(|e| e.eval(ctx)).unwrap_or(Truth::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({
            "facts": {
                "slo": {"error_rate": 0.002, "p99_ms": 180, "ok": true},
                "deploy": {"image": "app:2", "replicas": [1, 2, 3]}
            },
            "inputs": {"image": "app:2", "weight": 100},
            "nothing": null
        })
    }

    fn t(src: &str) -> Truth {
        parse(src)
            .unwrap_or_else(|e| panic!("{src}: {e}"))
            .eval(&ctx())
    }

    #[test]
    fn comparisons() {
        assert_eq!(t("facts.slo.error_rate < 0.01"), Truth::True);
        assert_eq!(t("facts.slo.p99_ms >= 180"), Truth::True);
        assert_eq!(t("facts.slo.p99_ms > 180"), Truth::False);
        assert_eq!(t("facts.deploy.image == inputs.image"), Truth::True);
        assert_eq!(t("inputs.weight == 100.0"), Truth::True);
        assert_eq!(t("facts.deploy.image != 'app:1'"), Truth::True);
        assert_eq!(t("facts.deploy.replicas[2] == 3"), Truth::True);
        assert_eq!(t(r#"facts["slo"].ok"#), Truth::True);
        assert_eq!(t("'b' > 'a'"), Truth::True);
        assert_eq!(t("-1 < 0"), Truth::True);
    }

    #[test]
    fn missing_paths_are_unknown_not_false() {
        assert_eq!(t("facts.missing.x == 1"), Truth::Unknown);
        assert_eq!(t("facts.missing.x != 1"), Truth::Unknown);
        assert_eq!(t("facts.missing"), Truth::Unknown);
        assert_eq!(t("!facts.missing"), Truth::Unknown);
    }

    #[test]
    fn ordering_type_mismatch_is_unknown() {
        assert_eq!(t("facts.deploy.image < 3"), Truth::Unknown);
        assert_eq!(t("facts.deploy.image == 3"), Truth::False);
    }

    #[test]
    fn kleene_logic() {
        assert_eq!(t("facts.missing == 1 && false"), Truth::False);
        assert_eq!(t("facts.missing == 1 || true"), Truth::True);
        assert_eq!(t("facts.missing == 1 && true"), Truth::Unknown);
        assert_eq!(t("facts.missing == 1 || false"), Truth::Unknown);
    }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(t("true || false && false"), Truth::True);
        assert_eq!(t("(true || false) && false"), Truth::False);
        assert_eq!(t("!(facts.slo.p99_ms > 200) && facts.slo.ok"), Truth::True);
        assert_eq!(t("(1 < 2) == true"), Truth::True);
    }

    #[test]
    fn exists() {
        assert_eq!(t("exists(facts.slo.ok)"), Truth::True);
        assert_eq!(t("exists(facts.nope)"), Truth::False);
        assert_eq!(t("exists(nothing)"), Truth::False);
    }

    #[test]
    fn non_boolean_condition_is_unknown() {
        assert_eq!(t("facts.slo.p99_ms"), Truth::Unknown);
        assert_eq!(t("'yes'"), Truth::Unknown);
    }

    #[test]
    fn parse_errors() {
        for bad in ["", "a ==", "(a", "a.", "a[", "'x", "a ~ b", "a b", "exists(1)"] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn collects_paths() {
        let e = parse("facts.a.b == inputs.x && exists(facts.c)").unwrap();
        let paths: Vec<String> = e.paths().iter().map(|p| p.to_string()).collect();
        assert_eq!(paths, ["facts.a.b", "inputs.x", "facts.c"]);
    }
}
