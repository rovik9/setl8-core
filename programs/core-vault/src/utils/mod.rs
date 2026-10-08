//! Helpers shared by several instructions.
//!
//! * `auth`          -- the sector-program CPI-auth check (`assert_sector_authority`)
//! * `pda_account`   -- creating an account at a PDA address (safe against pre-funding)
//! * `token_payment` -- splitting a payment between the payout pool and SL8, and
//!                      the trader -> pool / SL8 transfers

mod auth;
mod pda_account;
mod token_payment;

pub use auth::*;
pub use pda_account::*;
pub use token_payment::*;
