//! kloop-core — the agent loop and everything it stands on.
//!
//! Layout mirrors the architecture bets: append-only [`history`] with
//! record-time offload, a [`provider`] seam speaking one canonical wire
//! shape, per-input concurrency-safe [`tools`] dispatch, [`compact`]ion as
//! the sanctioned history rewrite, and the [`agent`] loop tying them
//! together. The binary (REPL, env config) lives in the `kloop` CLI crate.

pub mod agent;
pub mod compact;
pub mod config;
pub mod history;
pub mod provider;
pub mod tools;
pub mod types;

pub use config::Config;
