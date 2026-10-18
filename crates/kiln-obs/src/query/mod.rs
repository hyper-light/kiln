//! The query language (`docs/observe.md` §6): Datadog's metric query syntax, arithmetic and
//! functions, parsed without recursion and evaluated over a store.

pub mod lex;
pub mod eval;
pub mod parse;
