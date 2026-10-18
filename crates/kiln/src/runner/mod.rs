//! The runner side of CI (docs/ci.md §2.2): the GitHub-exact step runtime (W06), actions (W07),
//! executors and facts (W13), job terminals (W17). `exec.rs` keeps running v1 assignments until
//! the runtime here takes over `AssignV2`; each workstream adds its own submodule here.

pub mod builtins;
pub mod docker;
pub mod hashfiles;
pub mod kept;
pub mod runtime;
pub mod workspaces;
