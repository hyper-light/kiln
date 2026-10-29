//! The arithmetic processor's expressions, as Datadog's reads them: numbers, attributes
//! (`http.response_time`), `+ - * / %`, parentheses, unary minus, and `abs`, `ceil`, `floor`,
//! `round(x[, digits])`, `min` and `max`. An attribute that is not there is 0 if the processor
//! says to replace what is missing, and otherwise leaves the target unset.

/// The most an expression may hold (operands, operators and calls).
const NODES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Num(f64),
    Attr(String),
    Neg(Box<Node>),
    Bin(Op, Box<Node>, Box<Node>),
    Call(Func, Vec<Node>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Abs,
    Ceil,
    Floor,
    Round,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr(Node);

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    nodes: usize,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.s.get(self.at).copied()
    }

    fn count(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > NODES {
            return Err(format!("an expression of {NODES} terms at the most"));
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Node, String> {
        let mut left = self.term()?;
        while let Some(c @ (b'+' | b'-')) = self.peek() {
            self.at += 1;
            self.count()?;
            let right = self.term()?;
            left = Node::Bin(if c == b'+' { Op::Add } else { Op::Sub }, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Node, String> {
        let mut left = self.factor()?;
        while let Some(c @ (b'*' | b'/' | b'%')) = self.peek() {
            self.at += 1;
            self.count()?;
            let right = self.factor()?;
            let op = match c {
                b'*' => Op::Mul,
                b'/' => Op::Div,
                _ => Op::Rem,
            };
            left = Node::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn factor(&mut self) -> Result<Node, String> {
        self.count()?;
        match self.peek() {
            Some(b'-') => {
                self.at += 1;
                Ok(Node::Neg(Box::new(self.factor()?)))
            }
            Some(b'+') => {
                self.at += 1;
                self.factor()
            }
            Some(b'(') => {
                self.at += 1;
                let e = self.expr()?;
                if self.peek() != Some(b')') {
                    return Err("a ( that does not close".into());
                }
                self.at += 1;
                Ok(e)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
                    self.at += 1;
                }
                // An exponent: 1e3, 2.5E-2.
                if self.s.get(self.at).is_some_and(|c| *c == b'e' || *c == b'E') && self.s.get(self.at + 1).is_some_and(|c| c.is_ascii_digit() || *c == b'-' || *c == b'+') {
                    self.at += 2;
                    while self.s.get(self.at).is_some_and(u8::is_ascii_digit) {
                        self.at += 1;
                    }
                }
                let text = std::str::from_utf8(self.s.get(start..self.at).unwrap_or_default()).unwrap_or_default();
                text.parse::<f64>().map(Node::Num).map_err(|_| format!("a number: {text}"))
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'@' => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|c| c.is_ascii_alphanumeric() || b"_.@".contains(c)) {
                    self.at += 1;
                }
                let name = std::str::from_utf8(self.s.get(start..self.at).unwrap_or_default()).unwrap_or_default().trim_start_matches('@').to_owned();
                if self.peek() == Some(b'(') {
                    let f = match name.as_str() {
                        "abs" => Func::Abs,
                        "ceil" => Func::Ceil,
                        "floor" => Func::Floor,
                        "round" => Func::Round,
                        "min" => Func::Min,
                        "max" => Func::Max,
                        other => return Err(format!("a function kiln does not have: {other}")),
                    };
                    self.at += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(b')') {
                        loop {
                            args.push(self.expr()?);
                            match self.peek() {
                                Some(b',') => self.at += 1,
                                Some(b')') => break,
                                _ => return Err(format!("{name}(…): its arguments, then )")),
                            }
                        }
                    }
                    self.at += 1;
                    let ok = match f {
                        Func::Abs | Func::Ceil | Func::Floor => args.len() == 1,
                        Func::Round => (1..=2).contains(&args.len()),
                        Func::Min | Func::Max => !args.is_empty(),
                    };
                    if !ok {
                        return Err(format!("{name}(…): the wrong number of arguments"));
                    }
                    Ok(Node::Call(f, args))
                } else {
                    Ok(Node::Attr(name))
                }
            }
            Some(c) => Err(format!("an expression does not go on with {:?}", char::from(c))),
            None => Err("an expression that stops short".into()),
        }
    }
}

impl Expr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut p = Parser { s: s.as_bytes(), at: 0, nodes: 0 };
        let e = p.expr()?;
        if p.peek().is_some() {
            return Err(format!("an expression goes on past its end: {}", s.get(p.at..).unwrap_or_default()));
        }
        Ok(Self(e))
    }

    /// Its value, attributes read by `get`: none if one is missing (unless `missing_zero`), or it
    /// divides by zero.
    pub fn eval(&self, get: &dyn Fn(&str) -> Option<f64>, missing_zero: bool) -> Option<f64> {
        eval(&self.0, get, missing_zero)
    }
}

fn eval(n: &Node, get: &dyn Fn(&str) -> Option<f64>, z: bool) -> Option<f64> {
    Some(match n {
        Node::Num(v) => *v,
        Node::Attr(a) => match get(a) {
            Some(v) => v,
            None if z => 0.0,
            None => return None,
        },
        Node::Neg(x) => -eval(x, get, z)?,
        Node::Bin(op, a, b) => {
            let (a, b) = (eval(a, get, z)?, eval(b, get, z)?);
            match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div if b == 0.0 => return None,
                Op::Div => a / b,
                Op::Rem if b == 0.0 => return None,
                Op::Rem => a % b,
            }
        }
        Node::Call(f, args) => {
            let v: Vec<f64> = args.iter().map(|a| eval(a, get, z)).collect::<Option<_>>()?;
            let x = v.first().copied()?;
            match f {
                Func::Abs => x.abs(),
                Func::Ceil => x.ceil(),
                Func::Floor => x.floor(),
                Func::Round => {
                    let k = 10f64.powi(v.get(1).copied().unwrap_or(0.0).clamp(-15.0, 15.0) as i32);
                    (x * k).round() / k
                }
                Func::Min => v.iter().copied().fold(f64::INFINITY, f64::min),
                Func::Max => v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(e: &str, attrs: &[(&str, f64)], z: bool) -> Option<f64> {
        let get = |k: &str| attrs.iter().find(|(a, _)| *a == k).map(|(_, v)| *v);
        Expr::parse(e).ok()?.eval(&get, z)
    }

    #[test]
    fn expressions_read_and_reckon_as_arithmetic_does() {
        let a = [("http.duration", 1.5), ("bytes", 2048.0), ("@x", 1.0)];
        assert_eq!(value("http.duration * 1000", &a, false), Some(1500.0));
        assert_eq!(value("(bytes / 1024) + -1", &a, false), Some(1.0));
        assert_eq!(value("2 + 3 * 4 - 6 / 3", &a, false), Some(12.0));
        assert_eq!(value("10 % 4", &a, false), Some(2.0));
        assert_eq!(value("round(http.duration * 3.333, 2)", &a, false), Some(5.0));
        assert_eq!(value("round(2.5)", &a, false), Some(3.0));
        assert_eq!(value("abs(-3) + ceil(1.2) + floor(1.8) + min(4, 2, 9) + max(1, 7)", &a, false), Some(15.0));
        assert_eq!(value("1e3 + 2.5E-1", &a, false), Some(1000.25));
        // What is missing: none, or 0 when asked.
        assert_eq!(value("missing + 1", &a, false), None);
        assert_eq!(value("missing + 1", &a, true), Some(1.0));
        assert_eq!(value("1 / 0", &a, false), None);
        for bad in ["1 +", "(1", "1 2", "nope(1)", "abs(1, 2)", "", "#"] {
            assert!(Expr::parse(bad).is_err(), "{bad:?}");
        }
        let deep = "1+".repeat(300) + "1";
        assert!(Expr::parse(&deep).is_err(), "bounded");
    }
}
