//! Reach (`docs/reach.md`): an agent, a person or a script on one runner runs commands on another
//! runner — a Linux build from a Mac, a test on the machine with the GPU — over an end-to-end
//! encrypted link the server only relays.

pub mod cli;
pub mod directory;
pub mod e2e;
pub mod local;
pub mod node;
pub mod relay;
pub mod serve;
pub mod sync;
pub mod wire;

#[cfg(test)]
mod tests;
