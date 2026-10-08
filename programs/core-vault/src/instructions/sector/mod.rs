//! Sector-program instructions: called by a registered sector program (e.g. lev-trading) via CPI,
//! authenticated by its `sector_authority` PDA signature (see `utils::assert_sector_authority`).

mod deposit_fee;
mod deposit_reset;
mod record_activity;
mod request_payout;
mod flag_trader_failed;

pub use deposit_fee::*;
pub use deposit_reset::*;
pub use record_activity::*;
pub use request_payout::*;
pub use flag_trader_failed::*;
