//! kiln as the harness: AI coding agents (Claude Code, Codex, ...) run as structured sessions on
//! a machine, and kiln's UI is their whole front end.
//!
//! * [`model`] — the normalized timeline, state and commands every adapter speaks.
//! * [`core`] — a session's state plus what came out of the last step (updates, stdin lines).
//! * [`claude`] — Claude Code over stream-json and the control protocol.
//! * [`codex`] — Codex over its app-server (JSON-RPC v2).
//! * [`acp`] — any Agent Client Protocol agent (Devin, Gemini CLI, opencode, Goose, Amp's adapter).
//! * [`diff`] — line diffs for edit previews.
//! * [`handoff`] — a conversation carried from one harness to another (switching models across them).
//! * [`kinds`] — the registry of harnesses and their adapters.
//! * [`session`] — one agent session: the harness process, its adapter and its state.
//! * [`host`] — a machine's sessions: limits, commands, continuing them after a restart.
//! * [`registry`] — the server's mirror of every session, for any number of viewers.
//! * [`lineage`] — agents an agent starts: its children, held to no more than it has.
//! * [`ops`] — the `agents.*` operations (HTTP, MCP, code mode, the CLI).
//! * [`wait`] — where agents stand, and waiting for them to settle.
//! * [`queued`] — what waits in an agent's queue, and the messages kiln keeps there itself.
//! * [`subagent`] — messages a person writes to one of an agent's own subagents.
//! * [`transcript`] — Claude Code transcripts as the timeline (history on resume, sessions elsewhere).

pub mod acp;
pub mod antigravity;
pub mod archive;
pub mod broadcast;
pub mod carry;
pub mod claude;
pub mod cli;
pub mod codex;
pub mod core;
pub mod diff;
pub mod elicit;
pub mod elsewhere;
pub mod files;
pub mod gitstate;
pub mod handing;
pub mod handoff;
pub mod homes;
pub mod host;
pub mod kept;
pub mod kinds;
pub mod lineage;
pub mod live;
pub mod model;
pub mod moving;
pub mod opened;
pub mod ops;
pub mod pause;
pub mod port;
pub mod queued;
pub mod registry;
pub mod relocate;
pub mod screen;
pub mod session;
pub mod subagent;
pub mod survivors;
pub mod told;
pub mod socket;
pub mod stores;
pub mod streams;
pub mod transcript;
pub mod visuals;
pub mod wait;
pub mod webrtc;
pub mod worktree;
