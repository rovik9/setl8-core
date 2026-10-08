//! Helpers shared by several instructions.
//!
//! * `auth`          -- the sector-program CPI-auth check (`assert_sector_authority`)
//! * `destination`   -- a claim's destination: associated token accounts and whether they can be paid
//! * `settlement`    -- the pure pro-rata / pool-split arithmetic of a heartbeat cycle
//! * `pda_account`   -- creating an account at a PDA address (safe against pre-funding)
//! * `token_payment` -- splitting a payment between the payout pool and SL8, and
//!                      the trader -> pool / SL8 transfers

mod auth;
mod destination;
mod pda_account;
mod settlement;
mod token_payment;

pub use auth::*;
pub use destination::*;
pub use pda_account::*;
pub use settlement::*;
pub use token_payment::*;
