//! The query language's tokens (`docs/observe.md` §6).

/// The longest query: the request-line length every HTTP implementation must accept, 8,000 octets
/// (RFC 9110 §4.1), so any query fits a URL.
pub const MAX_QUERY_BYTES: usize = 8000;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok<'a> {
    /// A name: a metric (`system.cpu.user`), a tag key or value (`web-1`, `10.0.0.1:9100`), a
    /// function, a keyword. Dots, dashes, colons inside, slashes, `*` and `?` are part of names
    /// where a name is expected; the parser splits `key:value` itself.
    Name(&'a str),
    Number(f64),
    /// A quoted string, quotes removed (`'desc'`, `"a b"`).
    Str(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    /// `.` between a metric query and a modifier (`.rollup(...)`).
    Dot,
    Bang,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexError {
    #[error("the query is {0} bytes (at most {MAX_QUERY_BYTES})")]
    TooLong(usize),
    #[error("an unterminated string at byte {0}")]
    Unterminated(usize),
    #[error("an unexpected {1:?} at byte {0}")]
    Unexpected(usize, char),
}

/// A token and the byte it starts at.
pub type Spanned<'a> = (usize, Tok<'a>);

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/' | '*' | '?' | '@' | '#' | '~' | '%' | '=' | '<' | '>')
}

/// Splits a query into tokens.
///
/// A name runs over letters, digits and `_ . - : / * ? @ # ~ % = < >`; a `.` followed by a letter
/// right after a `)` or `}` is a modifier's dot instead (`{*}.rollup(...)`, `).as_count()`). A
/// number is a name that parses as one and starts with a digit or a sign-free `.`.
pub fn lex(q: &str) -> Result<Vec<Spanned<'_>>, LexError> {
    if q.len() > MAX_QUERY_BYTES {
        return Err(LexError::TooLong(q.len()));
    }
    let mut out: Vec<Spanned<'_>> = Vec::new();
    let mut it = q.char_indices().peekable();
    while let Some(&(i, c)) = it.peek() {
        let prev_closes = matches!(out.last(), Some((_, Tok::RParen | Tok::RBrace)));
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '(' => {
                it.next();
                out.push((i, Tok::LParen));
            }
            ')' => {
                it.next();
                out.push((i, Tok::RParen));
            }
            '{' => {
                it.next();
                out.push((i, Tok::LBrace));
            }
            '}' => {
                it.next();
                out.push((i, Tok::RBrace));
            }
            ',' => {
                it.next();
                out.push((i, Tok::Comma));
            }
            '+' => {
                it.next();
                out.push((i, Tok::Plus));
            }
            '!' => {
                it.next();
                out.push((i, Tok::Bang));
            }
            // At a token's start a `/` divides; inside a name it is the name's (`route:/api/*`).
            '/' => {
                it.next();
                out.push((i, Tok::Slash));
            }
            '.' if prev_closes => {
                it.next();
                out.push((i, Tok::Dot));
            }
            '\'' | '"' => {
                let quote = c;
                it.next();
                let mut s = String::new();
                let mut closed = false;
                while let Some((_, c)) = it.next() {
                    match c {
                        '\\' => {
                            if let Some((_, n)) = it.next() {
                                s.push(n);
                            }
                        }
                        c if c == quote => {
                            closed = true;
                            break;
                        }
                        c => s.push(c),
                    }
                }
                if !closed {
                    return Err(LexError::Unterminated(i));
                }
                out.push((i, Tok::Str(s)));
            }
            // At a token's start a `-` is a minus, binary or a sign (the parser tells which); inside
            // a name it is the name's (`web-1`), so `a - b` takes spaces or brackets.
            '-' => {
                it.next();
                out.push((i, Tok::Minus));
            }
            '*' if matches!(out.last(), Some((_, Tok::Name(_) | Tok::Number(_) | Tok::RParen | Tok::RBrace))) && !after_filter_open(&out) => {
                it.next();
                out.push((i, Tok::Star));
            }
            c if name_char(c) => {
                let start = i;
                let mut end = i;
                while let Some(&(j, c)) = it.peek() {
                    // Inside a name: a `-` or `*` or `/` that an operand could follow is still the
                    // name's own when no space comes first (`web-1`, `node_*`, `/api/*`).
                    if name_char(c) {
                        end = j + c.len_utf8();
                        it.next();
                    } else {
                        break;
                    }
                }
                let text = q.get(start..end).unwrap_or("");
                let numeric = text.starts_with(|c: char| c.is_ascii_digit()) || (text.starts_with('.') && text.len() > 1);
                match text.parse::<f64>() {
                    Ok(n) if numeric => out.push((start, Tok::Number(n))),
                    _ => out.push((start, Tok::Name(text))),
                }
            }
            c => return Err(LexError::Unexpected(i, c)),
        }
    }
    Ok(out)
}

/// Whether the last unclosed bracket is a `{`: inside a filter, `*` is a wildcard, not a product.
fn after_filter_open(out: &[Spanned<'_>]) -> bool {
    let mut depth = 0i32;
    for (_, t) in out.iter().rev() {
        match t {
            Tok::RBrace => depth += 1,
            Tok::LBrace if depth == 0 => return true,
            Tok::LBrace => depth -= 1,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(q: &str) -> Vec<Tok<'_>> {
        lex(q).unwrap().into_iter().map(|(_, t)| t).collect()
    }

    #[test]
    fn names_numbers_brackets_and_modifiers() {
        assert_eq!(
            toks("avg:system.cpu.user{host:web-1,!env:dev} by {host}.rollup(avg, 60)"),
            vec![
                Tok::Name("avg:system.cpu.user"),
                Tok::LBrace,
                Tok::Name("host:web-1"),
                Tok::Comma,
                Tok::Bang,
                Tok::Name("env:dev"),
                Tok::RBrace,
                Tok::Name("by"),
                Tok::LBrace,
                Tok::Name("host"),
                Tok::RBrace,
                Tok::Dot,
                Tok::Name("rollup"),
                Tok::LParen,
                Tok::Name("avg"),
                Tok::Comma,
                Tok::Number(60.0),
                Tok::RParen,
            ]
        );
        assert_eq!(toks("100 * a{*} / b{*}"), vec![Tok::Number(100.0), Tok::Star, Tok::Name("a"), Tok::LBrace, Tok::Name("*"), Tok::RBrace, Tok::Slash, Tok::Name("b"), Tok::LBrace, Tok::Name("*"), Tok::RBrace]);
        assert_eq!(toks("top(x{*}, 10, 'mean', \"desc\")").get(6..), Some(&[Tok::Comma, Tok::Number(10.0), Tok::Comma, Tok::Str("mean".into()), Tok::Comma, Tok::Str("desc".into()), Tok::RParen][..]));
        assert_eq!(toks("x{route:/api/*}"), vec![Tok::Name("x"), Tok::LBrace, Tok::Name("route:/api/*"), Tok::RBrace]);
        assert_eq!(toks("-1.5e3"), vec![Tok::Minus, Tok::Number(1500.0)]);
    }

    #[test]
    fn bad_input_is_an_error_never_a_panic() {
        assert_eq!(lex("'open"), Err(LexError::Unterminated(0)));
        assert!(lex(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
        assert!(matches!(lex("a ^ b"), Err(LexError::Unexpected(2, '^'))));
        let mut rng = crate::testkit::Rng::new(4);
        let alphabet: Vec<char> = "ab:.{}(),*/+-!'\" 0123456789ñ_\\".chars().collect();
        for _ in 0..5000 {
            let s: String = (0..rng.below(40)).map(|_| alphabet[rng.below(alphabet.len() as u64) as usize]).collect();
            let _ = lex(&s);
        }
    }
}
