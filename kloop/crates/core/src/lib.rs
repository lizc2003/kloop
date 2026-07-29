//! kloop-core — the agent loop and its organs, network-free.
//!
//! Wire types live in `kloop-protocol`; provider adapters (and the only
//! reqwest dependency) live in `kloop-provider`. This crate owns what makes
//! the agent an agent: the append-only [`history`] with record-time offload,
//! per-input concurrency-safe [`tools`] dispatch, the [`permissions`] gate
//! run before every tool execution, [`compact`]ion as the sanctioned history
//! rewrite, [`rollout`] session persistence, and the [`agent`] loop tying
//! them together. The binary (REPL, env config) is the
//! `kloop` CLI crate.

pub mod agent;
pub mod agent_type;
pub mod commands;
pub mod compact;
pub mod config;
pub mod context;
pub mod diff;
pub mod event;
pub mod file_state;
pub mod fs_complete;
pub mod history;
pub mod hooks;
pub mod image;
pub mod inbox;
pub mod permissions;
pub mod rollout;
pub mod sandbox;
pub mod shell;
pub mod skills;
pub mod tools;
pub mod worktree;

pub use config::Config;
/// Re-exported so the CLI can name the code-mode resource limits type (it lives
/// in the engine crate) without depending on `kloop-codemode` directly.
pub use kloop_codemode::Limits as ProgramLimits;
