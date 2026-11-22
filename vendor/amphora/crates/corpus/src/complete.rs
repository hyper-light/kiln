//! Where a record's statements end, as SQLite says: `sqlite3_complete` (SQLite's `src/complete.c`,
//! ported state for state), and the split the corpus's Python runner makes with it
//! (bench/corpus/slt.py, `split`): the text cut at each `;` after which SQLite calls it complete, so a
//! trigger keeps its body and a quoted `;` stays in its literal.

/// complete.c's tokens.
#[derive(Clone, Copy)]
enum Token {
    Semi = 0,
    Ws = 1,
    Other = 2,
    Explain = 3,
    Create = 4,
    Temp = 5,
    Trigger = 6,
    End = 7,
}

/// complete.c's `trans[state][token]`: states 0 INVALID, 1 START, 2 NORMAL, 3 EXPLAIN, 4 CREATE,
/// 5 TRIGGER, 6 SEMI, 7 END.
const TRANS: [[u8; 8]; 8] = [
    [1, 0, 2, 3, 4, 2, 2, 2],
    [1, 1, 2, 3, 4, 2, 2, 2],
    [1, 2, 2, 2, 2, 2, 2, 2],
    [1, 3, 3, 2, 4, 2, 2, 2],
    [1, 4, 2, 2, 2, 4, 5, 2],
    [6, 5, 5, 5, 5, 5, 5, 5],
    [6, 6, 5, 5, 5, 5, 5, 7],
    [1, 7, 5, 5, 5, 5, 5, 5],
];

/// SQLite's IdChar: letters, digits, `_`, `$` and every byte at or above 0x80 (sqlite3CtypeMap's
/// 0x46 bits).
fn id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// `sqlite3_complete`: whether `sql` ends a statement (a `;` outside literals, comments and a
/// trigger's body).
pub fn complete(sql: &[u8]) -> bool {
    let mut state: u8 = 0;
    let mut i = 0usize;
    while let Some(&c) = sql.get(i) {
        let next = sql.get(i.wrapping_add(1)).copied();
        let token = match c {
            b';' => Token::Semi,
            b' ' | b'\r' | b'\t' | b'\n' | 0x0c => Token::Ws,
            b'/' if next == Some(b'*') => {
                let body = sql.get(i.wrapping_add(2)..).unwrap_or_default();
                match body.windows(2).position(|w| w == b"*/") {
                    // the comment's `*/` ends it: i moves to its `/`
                    Some(at) => i = i.wrapping_add(2).wrapping_add(at).wrapping_add(1),
                    None => return false,
                }
                Token::Ws
            }
            b'-' if next == Some(b'-') => {
                match sql
                    .get(i..)
                    .unwrap_or_default()
                    .iter()
                    .position(|&b| b == b'\n')
                {
                    Some(at) => i = i.wrapping_add(at),
                    None => return state == 1,
                }
                Token::Ws
            }
            b'[' | b'`' | b'"' | b'\'' => {
                let close = if c == b'[' { b']' } else { c };
                let body = sql.get(i.wrapping_add(1)..).unwrap_or_default();
                match body.iter().position(|&b| b == close) {
                    Some(at) => i = i.wrapping_add(1).wrapping_add(at),
                    None => return false,
                }
                Token::Other
            }
            _ if id_char(c) => {
                let rest = sql.get(i..).unwrap_or_default();
                let len = rest.iter().position(|&b| !id_char(b)).unwrap_or(rest.len());
                let word = rest.get(..len).unwrap_or_default();
                i = i.wrapping_add(len).wrapping_sub(1);
                if word.eq_ignore_ascii_case(b"create") {
                    Token::Create
                } else if word.eq_ignore_ascii_case(b"trigger") {
                    Token::Trigger
                } else if word.eq_ignore_ascii_case(b"temp")
                    || word.eq_ignore_ascii_case(b"temporary")
                {
                    Token::Temp
                } else if word.eq_ignore_ascii_case(b"end") {
                    Token::End
                } else if word.eq_ignore_ascii_case(b"explain") {
                    Token::Explain
                } else {
                    Token::Other
                }
            }
            _ => Token::Other,
        };
        state = TRANS
            .get(usize::from(state))
            .and_then(|row| row.get(token as usize))
            .copied()
            .unwrap_or(0);
        i = i.wrapping_add(1);
    }
    state == 1
}

/// A record's statements, cut where SQLite says each ends (slt.py's `split`): pieces between `;`
/// joined until the text so far, with a `;` after it, is complete. The whole text when nothing is.
pub fn split(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    for piece in sql.split(';') {
        if started {
            cur.push(';');
        }
        cur.push_str(piece);
        started = true;
        let mut probe = cur.clone().into_bytes();
        probe.push(b';');
        if complete(&probe) {
            let t = cur.trim();
            if !t.is_empty() {
                out.push(t.to_owned());
            }
            cur.clear();
            started = false;
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_owned());
    }
    if out.is_empty() {
        out.push(sql.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: ask whether texts SQLite's documentation and complete.c's comments name are complete.
    /// Expect: SQLite's answers (sqlite3_complete).
    #[test]
    fn complete_answers_as_sqlite() {
        assert!(complete(b"SELECT 1;"));
        assert!(!complete(b"SELECT 1"));
        assert!(!complete(b"SELECT 'a;"));
        assert!(complete(b"SELECT 'a;b';"));
        assert!(!complete(
            b"CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;"
        ));
        assert!(complete(
            b"CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; END;"
        ));
        assert!(complete(
            b"CREATE TEMP TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; END;"
        ));
        assert!(complete(b"SELECT 1; -- a comment"));
        assert!(!complete(b"SELECT 1 /* open;"));
        assert!(complete(b"SELECT [a;b] FROM t;"));
        assert!(complete(b"EXPLAIN CREATE TABLE end(a);"));
    }

    /// Do: split records as slt.py does. Expect: a trigger kept whole, a quoted `;` kept, empty
    /// statements dropped.
    #[test]
    fn split_keeps_triggers_and_literals() {
        assert_eq!(
            split("SELECT 1; SELECT 'a;b'"),
            vec!["SELECT 1", "SELECT 'a;b'"]
        );
        let trig =
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES (1); END; SELECT 2";
        assert_eq!(
            split(trig),
            vec![
                "CREATE TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES (1); END",
                "SELECT 2"
            ]
        );
        assert_eq!(split("SELECT 1;;"), vec!["SELECT 1"]);
        assert_eq!(split("  "), vec!["  "]);
    }
}
