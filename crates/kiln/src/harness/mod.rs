//! kiln as the harness: AI coding agents (Claude Code, Codex, ...) run as structured sessions on
//! a machine, and kiln's UI is their whole front end.
//!
//! * [`model`] — the normalized timeline, state and commands every adapter speaks.
//! * [`core`] — a session's state plus what came out of the last step (updates, stdin lines).
//! * [`claude`] — Claude Code over stream-json and the control protocol.
//! * [`codex`] — Codex over its app-server (JSON-RPC v2).
//! * [`acp`] — any Agent Client Protocol agent (Gemini CLI, opencode, Goose, Amp's adapter).
//! * [`diff`] — line diffs for edit previews.
//! * [`kinds`] — the registry of harnesses and their adapters.
//! * [`session`] — one agent session: the harness process, its adapter and its state.
//! * [`host`] — a machine's sessions: limits, commands, continuing them after a restart.
//! * [`registry`] — the server's mirror of every session, for any number of viewers.
//! * [`transcript`] — Claude Code transcripts as the timeline (history on resume, sessions elsewhere).

pub mod acp;
pub mod claude;
pub mod cli;
pub mod codex;
pub mod core;
pub mod diff;
pub mod elicit;
pub mod host;
pub mod kinds;
pub mod model;
pub mod registry;
pub mod session;
pub mod socket;
pub mod transcript;
