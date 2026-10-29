//! Logs (`docs/observe.md` §3, §4.6): records, their columnar blocks, and search over them.

pub mod archive;
pub mod block;
pub mod bloom;
pub mod buf;
pub mod file;
pub mod metrics;
pub mod patterns;
pub mod record;
pub mod search;
pub mod store;
pub mod tail;
