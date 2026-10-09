//! `setl8-keeper`: the permissionless off-chain runner for the core-vault payout cycle.
//!
//! It calls only `reconcile_product`, `begin_heartbeat`, `settle_claims` and `finalize_heartbeat`, which need
//! no admin key; it holds no authority over funds; anyone may run one, and two at once are harmless because
//! every decision is made from freshly read chain state.

pub mod alerts;
pub mod chain;
pub mod cli;
pub mod config;
pub mod errors;
pub mod ixs;
pub mod log;
pub mod model;
pub mod plan;
pub mod runner;
pub mod safety;
