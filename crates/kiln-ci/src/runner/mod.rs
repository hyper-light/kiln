//! What the runner decides, as pure code (docs/ci.md W06): workflow commands, environment
//! files, masking. The kiln crate's runtime runs the processes and carries these out.
//!
//! - [`commands`]: workflow commands.
//! - [`envfiles`]: `$GITHUB_ENV`, `$GITHUB_OUTPUT`, `$GITHUB_STATE`, `$GITHUB_PATH`, summaries.
//! - [`mask`]: masking, across chunks.

pub mod commands;
pub mod envfiles;
pub mod mask;
