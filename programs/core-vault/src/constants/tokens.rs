//! Token and basis-point constants: all amounts are 6-decimal base units.

use anchor_lang::prelude::*;

/// Both accepted stablecoins (USDC, USDT) are 6-decimal classic-SPL mints.
/// Every amount in the vault is in these base units.
pub const TOKEN_DECIMALS: u8 = 6;

/// Basis-point denominator for `fee_split_bps` and the reset price table.
pub const BPS_DENOMINATOR: u128 = 10_000;

/// Each pool's reserve floor is this share of its post-settlement balance, set when
/// a heartbeat cycle finalizes (25%). The floor only constrains the future admin
/// withdrawal; it never limits claim settlement.
pub const FLOOR_BPS: u128 = 2_500;

/// The Associated Token Account program. Claims are paid only to the trader's
/// ATAs, derived with this id (no extra dependency: the derivation is three seeds).
pub const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
