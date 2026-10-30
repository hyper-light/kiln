//! The structural merge's cases (docs/ci.md W26-R17): each it resolves, each it keeps, per grammar.

use proptest::prelude::*;

use vorpal_language::SupportLang;

use super::{Kept, Outcome, finish, merge_files, resolve_hunk};

fn resolved(o: Outcome) -> String {
    match o {
        Outcome::Resolved(t) => t,
        Outcome::Kept(k) => format!("<kept: {}>", k.as_str()),
    }
}

const RS_BASE: &str = "fn a() -> u32 {\n    1\n}\n\nfn b() -> u32 {\n    2\n}\n";

#[test]
fn w26_r17_structural_rust_different_functions_merge() {
    let main = RS_BASE.replace("    1\n", "    10\n");
    let this = RS_BASE.replace("    2\n", "    20\n");
    let want = "fn a() -> u32 {\n    10\n}\n\nfn b() -> u32 {\n    20\n}\n";
    assert_eq!(resolved(merge_files("src/x.rs", RS_BASE, &main, &this)), want);
}

#[test]
fn w26_r17_structural_rust_statements_of_one_body_merge() {
    let base = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
    let main = "fn f() {\n    let a = 10;\n    let b = 2;\n    let c = 3;\n}\n";
    let this = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 30;\n}\n";
    assert_eq!(resolved(merge_files("x.rs", base, main, this)), "fn f() {\n    let a = 10;\n    let b = 2;\n    let c = 30;\n}\n");
}

#[test]
fn w26_r17_structural_rust_imports_added_at_different_places_merge() {
    let base = "use a::A;\nuse c::C;\nuse e::E;\n\nfn f() {}\n";
    let main = "use a::A;\nuse b::B;\nuse c::C;\nuse e::E;\n\nfn f() {}\n";
    let this = "use a::A;\nuse c::C;\nuse d::D;\nuse e::E;\n\nfn f() {}\n";
    assert_eq!(resolved(merge_files("x.rs", base, main, this)), "use a::A;\nuse b::B;\nuse c::C;\nuse d::D;\nuse e::E;\n\nfn f() {}\n");
}

#[test]
fn w26_r17_structural_keeps_two_additions_in_the_same_place() {
    let main = format!("{RS_BASE}\nfn c() -> u32 {{\n    3\n}}\n");
    let this = format!("{RS_BASE}\nfn d() -> u32 {{\n    4\n}}\n");
    assert_eq!(merge_files("x.rs", RS_BASE, &main, &this), Outcome::Kept(Kept::SamePlace));
}

#[test]
fn w26_r17_structural_keeps_the_same_statement_changed_twice() {
    let main = RS_BASE.replace("    1\n", "    10\n");
    let this = RS_BASE.replace("    1\n", "    11\n");
    assert_eq!(merge_files("x.rs", RS_BASE, &main, &this), Outcome::Kept(Kept::SameNode));
}

#[test]
fn w26_r17_structural_keeps_a_name_defined_twice() {
    // Both add fn c, in different places and otherwise: merged, c would be defined twice.
    let main = format!("fn c() -> u32 {{\n    3\n}}\n\n{RS_BASE}");
    let this = format!("{RS_BASE}\nfn c() -> u32 {{\n    4\n}}\n");
    assert_eq!(merge_files("x.rs", RS_BASE, &main, &this), Outcome::Kept(Kept::DuplicateSignature));
}

#[test]
fn w26_r17_structural_keeps_overloads_whose_identity_is_ambiguous() {
    let base = "class A {\n  int f(int x) { return 1; }\n  int f(String s) { return 2; }\n}\n";
    let main = base.replace("return 1;", "return 10;");
    let this = base.replace("return 2;", "return 20;");
    assert_eq!(merge_files("A.java", base, &main, &this), Outcome::Kept(Kept::Ambiguous));
}

#[test]
fn w26_r17_structural_keeps_a_side_that_does_not_parse() {
    let main = RS_BASE.replace("    1\n", "    10\n");
    let this = RS_BASE.replace("fn b() -> u32 {", "fn b( -> u32 {");
    assert_eq!(merge_files("x.rs", RS_BASE, &main, &this), Outcome::Kept(Kept::DoesNotParse));
}

#[test]
fn w26_r17_structural_keeps_a_merge_that_does_not_parse() {
    assert_eq!(finish(SupportLang::Rust, "fn a() -> u32 {\n    1\n".to_owned()), Err(Kept::MergeDoesNotParse));
    assert_eq!(finish(SupportLang::Rust, "fn a() -> u32 {\n    1\n}\n".to_owned()), Ok("fn a() -> u32 {\n    1\n}\n".to_owned()));
}

#[test]
fn w26_r17_structural_keeps_a_file_without_a_grammar() {
    assert_eq!(merge_files("notes.xyz", "a\n", "b\n", "c\n"), Outcome::Kept(Kept::NoGrammar));
}

#[test]
fn w26_r17_structural_json_different_keys_merge() {
    let base = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
    let main = "{\n  \"a\": 10,\n  \"b\": 2\n}\n";
    let this = "{\n  \"a\": 1,\n  \"b\": 20\n}\n";
    assert_eq!(resolved(merge_files("x.json", base, main, this)), "{\n  \"a\": 10,\n  \"b\": 20\n}\n");
}

#[test]
fn w26_r17_structural_python_different_functions_merge() {
    let base = "def a():\n    return 1\n\n\ndef b():\n    return 2\n";
    let main = base.replace("return 1", "return 10");
    let this = base.replace("return 2", "return 20");
    assert_eq!(resolved(merge_files("x.py", base, &main, &this)), "def a():\n    return 10\n\n\ndef b():\n    return 20\n");
}

#[test]
fn w26_r17_structural_typescript_different_methods_merge() {
    let base = "class A {\n  f(): number { return 1; }\n  g(): number { return 2; }\n}\n";
    let main = base.replace("return 1;", "return 10;");
    let this = base.replace("return 2;", "return 20;");
    assert_eq!(resolved(merge_files("a.ts", base, &main, &this)), "class A {\n  f(): number { return 10; }\n  g(): number { return 20; }\n}\n");
}

#[test]
fn w26_r17_structural_go_different_functions_merge() {
    let base = "package p\n\nfunc A() int {\n\treturn 1\n}\n\nfunc B() int {\n\treturn 2\n}\n";
    let main = base.replace("return 1", "return 10");
    let this = base.replace("return 2", "return 20");
    assert_eq!(resolved(merge_files("p.go", base, &main, &this)), "package p\n\nfunc A() int {\n\treturn 10\n}\n\nfunc B() int {\n\treturn 20\n}\n");
}

#[test]
fn w26_r17_structural_yaml_different_keys_merge() {
    let base = "a: 1\nb: 2\n";
    let main = "a: 10\nb: 2\n";
    let this = "a: 1\nb: 20\n";
    assert_eq!(resolved(merge_files("x.yaml", base, main, this)), "a: 10\nb: 20\n");
}

// The goal's example (D42 in plan/expand.rs): main dedups after `cross()`, D42 applies includes
// and excludes there. Both add a statement in the same place: kept for the next rung.
#[test]
fn w26_r17_structural_d42_expand_hunk_is_kept() {
    let prefix = "fn expand(m: &Matrix) -> Legs {\n    let legs = m.cross();\n";
    let suffix = "    legs\n}\n";
    let (base, main, this) = ("", "    legs.dedup_by_key(|l| l.key());\n", "    let legs = apply(m.include, m.exclude, legs);\n");
    assert_eq!(resolve_hunk("plan/expand.rs", prefix, base, main, this, suffix), Outcome::Kept(Kept::SamePlace));
}

// One hunk of a file whose sides change different functions: the hunk's merged text, and only it.
#[test]
fn w26_r17_structural_resolve_hunk_gives_the_hunk_alone() {
    let prefix = "fn head() {}\n\n";
    let suffix = "\nfn tail() {}\n";
    let base = "fn a() -> u32 {\n    1\n}\n\nfn b() -> u32 {\n    2\n}\n";
    let main = "fn a() -> u32 {\n    10\n}\n\nfn b() -> u32 {\n    2\n}\n";
    let this = "fn a() -> u32 {\n    1\n}\n\nfn b() -> u32 {\n    20\n}\n";
    assert_eq!(resolved(resolve_hunk("x.rs", prefix, base, main, this, suffix)), "fn a() -> u32 {\n    10\n}\n\nfn b() -> u32 {\n    20\n}\n");
}

/// A Rust file of `n` functions, function i returning `values[i]`.
fn rust_file(values: &[u32]) -> String {
    values.iter().enumerate().map(|(i, v)| format!("fn f{i}() -> u32 {{\n    {v}\n}}\n")).collect::<Vec<_>>().join("\n")
}

proptest! {
    // Edits to disjoint functions merge to both applied; the same function edited otherwise by
    // each side is kept.
    #[test]
    fn w26_r17_structural_disjoint_edits_merge_and_overlaps_keep(n in 2usize..12, picks in proptest::collection::vec(0u8..3, 12), clash in 0usize..12) {
        let base: Vec<u32> = (0..n).map(|i| u32::try_from(i).unwrap_or(0)).collect();
        let (mut main, mut this, mut both) = (base.clone(), base.clone(), base.clone());
        for (i, p) in picks.iter().take(n).enumerate() {
            match p {
                1 => if let (Some(m), Some(b)) = (main.get_mut(i), both.get_mut(i)) { *m += 100; *b = *m },
                2 => if let (Some(t), Some(b)) = (this.get_mut(i), both.get_mut(i)) { *t += 200; *b = *t },
                _ => {}
            }
        }
        let got = merge_files("p.rs", &rust_file(&base), &rust_file(&main), &rust_file(&this));
        prop_assert_eq!(got, Outcome::Resolved(rust_file(&both)));
        // Now one function both change, each its own way.
        let c = clash % n;
        if let (Some(m), Some(t)) = (main.get_mut(c), this.get_mut(c)) {
            *m = 1000;
            *t = 2000;
        }
        prop_assert_eq!(merge_files("p.rs", &rust_file(&base), &rust_file(&main), &rust_file(&this)), Outcome::Kept(Kept::SameNode));
    }
}
