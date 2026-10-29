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
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/' | '*' | '?' | '@' | '#' | '~' | '%' | '=' | '<' | '>' | '$')
}

/// Splits a query into tokens.
///
/// A name runs over letters, digits and `_ . - : / * ? @ # ~ % = < > $` (a dashboard's `$env`); a
/// `.` followed by a letter right after a `)` or `}` is a modifier's dot instead (`{*}.rollup(...)`,
/// `).as_count()`). A number is a name that parses as one and starts with a digit or a sign-free
/// `.`; outside a filter it ends at a `*`, `/` or `-` (`2*a{*}`, `100-a{*}`).
pub fn lex(q: &str) -> Result<Vec<Spanned<'_>>, LexError> {
    if q.len() > MAX_QUERY_BYTES {
        return Err(LexError::TooLong(q.len()));
    }
    let mut out: Vec<Spanned<'_>> = Vec::new();
    let mut it = q.char_indices().peekable();
    // The `{` not yet closed: inside a filter, `*` is a wildcard, not a product, and a name may
    // start with a digit and hold `-`, `*` and `/` (`10.0.0.1:9100`, `2024-05`).
    let mut braces = 0usize;
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
                braces += 1;
                out.push((i, Tok::LBrace));
            }
            '}' => {
                it.next();
                braces = braces.saturating_sub(1);
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
                // Sized to the next quote: the string's length unless it escapes one.
                let rest = q.get(i + 1..).unwrap_or("");
                let mut s = String::with_capacity(rest.find(quote).unwrap_or(rest.len()));
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
            '*' if matches!(out.last(), Some((_, Tok::Name(_) | Tok::Number(_) | Tok::RParen | Tok::RBrace))) && braces == 0 => {
                it.next();
                out.push((i, Tok::Star));
            }
            c if name_char(c) => {
                let start = i;
                let mut end = i;
                // Outside a filter a number ends where an operator starts (`100/(a + b)`, `2*a{*}`,
                // `1e-3` whole); a calendar's `9am`, `21:30`, `15th` stay names.
                let number = braces == 0 && (c.is_ascii_digit() || (c == '.' && q.get(i + 1..).is_some_and(|r| r.starts_with(|d: char| d.is_ascii_digit()))));
                while let Some(&(j, c)) = it.peek() {
                    // Inside a name: a `-` or `*` or `/` that an operand could follow is still the
                    // name's own when no space comes first (`web-1`, `node_*`, `/api/*`).
                    let operator = number && (matches!(c, '*' | '/') || (c == '-' && !q.get(start..j).is_some_and(|t| t.ends_with(['e', 'E']))));
                    if name_char(c) && !operator {
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
        // A number outside a filter ends at an operator, written without spaces as Datadog's
        // dashboards write them; inside one it is a name's start.
        assert_eq!(toks("2*a{*}"), vec![Tok::Number(2.0), Tok::Star, Tok::Name("a"), Tok::LBrace, Tok::Name("*"), Tok::RBrace]);
        assert_eq!(toks("a{*}*100/(b{*})").get(3..8), Some(&[Tok::RBrace, Tok::Star, Tok::Number(100.0), Tok::Slash, Tok::LParen][..]));
        assert_eq!(toks("100-1e-3"), vec![Tok::Number(100.0), Tok::Minus, Tok::Number(0.001)]);
        assert_eq!(toks(".5*a{*}").get(..3), Some(&[Tok::Number(0.5), Tok::Star, Tok::Name("a")][..]));
        assert_eq!(toks("x{ip:10.0.0.1:9100, d IN (2024-05, 3*4)}").get(2..6), Some(&[Tok::Name("ip:10.0.0.1:9100"), Tok::Comma, Tok::Name("d"), Tok::Name("IN")][..]));
        assert_eq!(toks("x{d IN (2024-05, 3*4)}").get(5..9), Some(&[Tok::Name("2024-05"), Tok::Comma, Tok::Name("3*4"), Tok::RParen][..]));
        assert_eq!(toks("rollup(sum, daily, 9:30am)").get(6), Some(&Tok::Name("9:30am")));
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
