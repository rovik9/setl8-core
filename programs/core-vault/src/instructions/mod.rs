//! One file per instruction (its `Accounts` struct plus handler), grouped by WHO
//! may call it:
//!
//! * `admin`          -- the 2-of-2 admin multisig
//! * `sector`         -- a registered sector program, via CPI
//! * `permissionless` -- anyone
//!
//! Everything is re-exported flat, so `crate::instructions::DepositFee` etc. do
//! not depend on the folder.

mod admin;
mod permissionless;
mod sector;

pub use admin::*;
pub use permissionless::*;
pub use sector::*;
