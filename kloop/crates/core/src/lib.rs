//! kloop-core — the agent loop and its organs, network-free.
//!
//! Wire types live in `kloop-protocol`; provider adapters (and the only
//! reqwest dependency) live in `kloop-provider`. This crate owns what makes
//! the agent an agent: the append-only [`history`] with record-time offload,
//! per-input concurrency-safe [`tools`] dispatch, [`compact`]ion as the
//! sanctioned history rewrite, [`rollout`] session persistence, and the
//! [`agent`] loop tying them together. The binary (REPL, env config) is the
//! `kloop` CLI crate.

pub mod agent;
pub mod compact;
pub mod config;
pub mod history;
pub mod rollout;
pub mod tools;

pub use config::Config;
