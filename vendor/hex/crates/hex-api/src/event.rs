//! Hex's event lines on a process's log (CLAUDE.md §10): `hex.<area> event=<name> key=value ...`, one line each, which
//! kiln keeps verbatim and its metrics read (`hex.wait event=ran_out`; ROOT and kiln's metrics owner, 2026-11-14). The
//! code's own words are bare: the area, the event's name, each key, and a wait's name (dotted lowercase words). Every
//! other text value (a name a person gave, a path, an error's words, what a wait follows) is a JSON string (RFC 8259
//! §7), so nothing it says can split an event across lines or run into the next key, and every number is whole.

/// A value of an event line's field: a JSON string ([`json_string`]), or a whole number written as itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Text(&'a str),
    Number(u128),
}

/// The event line `<area> event=<name> <key>=<value> ...`, no newline (its writer ends it): each value a JSON string or
/// a whole number ([`Value`]), so the line is one line whatever its values say. The area, the event's name and the
/// keys are the code's own words.
pub fn line(
    area: &'static str,
    event: &'static str,
    fields: &[(&'static str, Value<'_>)],
) -> String {
    let mut line = format!("{area} event={event}");
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        match value {
            Value::Text(text) => {
                line.push('"');
                push_json_contents(&mut line, text, usize::MAX);
                line.push('"');
            }
            Value::Number(n) => line.push_str(&n.to_string()),
        }
    }
    line
}

/// `text` as a JSON string, quoted (RFC 8259 §7): a `"` and a `\` escaped by a `\`, and every control character,
/// U+0000 to U+001F, escaped as `\b`, `\f`, `\n`, `\r`, `\t` or `\u00XX`. Every other character is itself.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    push_json_contents(&mut out, text, usize::MAX);
    out.push('"');
    out
}

/// Appends `text` to `line` as a JSON string's contents, escaped as [`json_string`] escapes, a character at a time
/// while `line` stays within `bound` bytes; says how many of `text`'s bytes didn't fit, none cut within an escape or
/// a character.
pub fn push_json_contents(line: &mut String, text: &str, bound: usize) -> usize {
    for (at, c) in text.char_indices() {
        let before = line.len();
        match c {
            '"' => line.push_str("\\\""),
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\r' => line.push_str("\\r"),
            '\t' => line.push_str("\\t"),
            '\u{8}' => line.push_str("\\b"),
            '\u{c}' => line.push_str("\\f"),
            c if u32::from(c) < 0x20 => line.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => line.push(c),
        }
        if line.len() > bound {
            line.truncate(before);
            return text.len().saturating_sub(at);
        }
    }
    0
}

#[cfg(test)]
mod tests;
