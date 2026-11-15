//! Hex's event lines' values (CLAUDE.md §10; ROOT and kiln's metrics owner, 2026-11-14): a value is a JSON string
//! (RFC 8259 §7), so nothing it says splits an event across lines.

use proptest::prelude::*;

use super::*;

/// Do: write a value holding quotes, a backslash, a newline, a carriage return, a tab, a backspace, a form feed, two
/// other control characters and a character past ASCII. Expect: each escaped as RFC 8259 §7 has it, the rest itself,
/// no newline in the result, and a JSON reader reading back the value written.
#[test]
fn a_value_with_quotes_a_backslash_a_newline_and_control_characters_is_one_json_string() {
    let value = "a \"quoted\" \\ path\nover\r\tlines\u{8}\u{c}\u{1}\u{1f} caf\u{e9}";
    let written = json_string(value);
    assert_eq!(
        written,
        "\"a \\\"quoted\\\" \\\\ path\\nover\\r\\tlines\\b\\f\\u0001\\u001f caf\u{e9}\""
    );
    assert!(!written.contains('\n') && !written.contains('\r'));
    assert_eq!(serde_json::from_str::<String>(&written).unwrap(), value);
}

/// Do: form an event line of a text value holding a quote, a backslash, a newline and a control character, and a
/// number. Expect: `<area> event=<name>`, the text a JSON string, the number itself, one line.
#[test]
fn an_event_lines_values_are_json_strings_and_whole_numbers() {
    let formed = line(
        "hex.test",
        "formed",
        &[
            ("why", Value::Text("a \"b\" \\ c\nd\u{1}")),
            ("count", Value::Number(42)),
        ],
    );
    assert_eq!(
        formed,
        "hex.test event=formed why=\"a \\\"b\\\" \\\\ c\\nd\\u0001\" count=42"
    );
    assert!(!formed.contains('\n'));
}

proptest! {
    /// Do: write any text as a value. Expect: one line, which a JSON reader reads back as the text.
    #[test]
    fn any_value_reads_back_as_itself(value in any::<String>()) {
        let written = json_string(&value);
        prop_assert!(!written.contains('\n') && !written.contains('\r'));
        prop_assert_eq!(serde_json::from_str::<String>(&written).unwrap(), value);
    }

    /// Do: append any text's contents to a line within any bound. Expect: the line within the bound wherever it
    /// started within it; the bytes said cut exactly the text's bytes not written; and what was written a whole
    /// prefix of the text, never part of an escape or a character, as a JSON reader reads it back.
    #[test]
    fn contents_cut_at_a_bound_are_a_whole_prefix_and_the_cut_is_counted(
        value in any::<String>(),
        bound in 0usize..64,
    ) {
        let mut line = String::new();
        let cut = push_json_contents(&mut line, &value, bound);
        prop_assert!(line.len() <= bound);
        let shown: String = serde_json::from_str(&format!("\"{line}\"")).unwrap();
        prop_assert!(value.starts_with(&shown));
        prop_assert_eq!(shown.len() + cut, value.len());
    }
}
