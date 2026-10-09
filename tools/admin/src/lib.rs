//! `setl8-admin`: the signing ceremony for the core-vault 2-of-2 admin instructions.
//!
//! One side runs `plan`, each signer separately runs `inspect` then `sign`, anyone runs
//! `send`. The binary in `main.rs` is a thin wrapper over [`cli::run_guarded`].

pub mod admin_ix;
pub mod cli;
pub mod cluster;
pub mod constants;
pub mod error;
pub mod fmt;
pub mod host;
pub mod inspect;
pub mod keyfile;
pub mod message;
pub mod nonce;
pub mod plan;
pub mod rpc;
pub mod send;
pub mod sign;
pub mod status;
pub mod txfile;
