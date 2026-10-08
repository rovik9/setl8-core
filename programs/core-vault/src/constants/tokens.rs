//! Token and basis-point constants: all amounts are 6-decimal base units.

/// Both accepted stablecoins (USDC, USDT) are 6-decimal classic-SPL mints.
/// Every amount in the vault is in these base units.
pub const TOKEN_DECIMALS: u8 = 6;

/// Basis-point denominator for `fee_split_bps` and the reset price table.
pub const BPS_DENOMINATOR: u128 = 10_000;
