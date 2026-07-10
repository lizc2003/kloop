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
pub mod agents;
pub mod compact;
pub mod config;
pub mod context;
pub mod history;
pub mod hooks;
pub mod permissions;
pub mod rollout;
pub mod sandbox;
pub mod shell;
pub mod tools;

pub use config::Config;
