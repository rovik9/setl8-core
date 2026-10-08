//! Helpers shared by several instructions.
//!
//! * `auth`          -- the sector-program CPI-auth check (`assert_sector_authority`)
//! * `token_payment` -- splitting a payment between the payout pool and SL8, and
//!                      the trader -> pool / SL8 transfers

mod auth;
mod token_payment;

pub use auth::*;
pub use token_payment::*;
