use anchor_lang::prelude::*;

use crate::constants::{
    BOND_6M_INTEREST_BPS, BOND_6M_LOCK_SECS, BOND_6M_TERM_SECS, BOND_9M_INTEREST_BPS, BOND_9M_LOCK_SECS,
    BOND_9M_TERM_SECS,
};

/// The two bond terms. Interest is not accrued over time: it is paid in full only
/// at maturity (see `utils::bond`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondTerm {
    /// 6 months (180 days) at 20%.
    SixMonths,
    /// 9 months (270 days) at 30%.
    NineMonths,
}

impl BondTerm {
    /// Seconds from deposit to maturity.
    pub fn term_secs(&self) -> i64 {
        match self {
            BondTerm::SixMonths => BOND_6M_TERM_SECS,
            BondTerm::NineMonths => BOND_9M_TERM_SECS,
        }
    }
    /// Seconds from deposit until the hard lock ends (half the term).
    pub fn lock_secs(&self) -> i64 {
        match self {
            BondTerm::SixMonths => BOND_6M_LOCK_SECS,
            BondTerm::NineMonths => BOND_9M_LOCK_SECS,
        }
    }
    /// Interest in basis points of the principal, paid at maturity. Copied into the
    /// position at deposit.
    pub fn interest_bps(&self) -> u16 {
        match self {
            BondTerm::SixMonths => BOND_6M_INTEREST_BPS,
            BondTerm::NineMonths => BOND_9M_INTEREST_BPS,
        }
    }
}

/// One open bond. PDA `[BOND_SEED, depositor, deposit_index.to_le_bytes()]`. Closed
/// (rent back to the depositor) by `request_bond_payout`, so a position can be
/// withdrawn exactly once.
#[account]
pub struct BondPosition {
    pub depositor: Pubkey,
    /// From the depositor's `BondCapTracker`; never reused.
    pub deposit_index: u64,
    /// Which stablecoin the bond is in (and so which payout pool took the pool share).
    pub mint: Pubkey,
    pub principal: u64,
    pub term: BondTerm,
    /// Copied from the term at deposit, so a later constant change cannot alter an open bond.
    pub interest_bps: u16,
    pub created_at: i64,
    pub bump: u8,
}

impl BondPosition {
    pub const SPACE: usize = 8 // discriminator
        + 32 // depositor
        + 8 // deposit_index
        + 32 // mint
        + 8 // principal
        + 1 // term
        + 2 // interest_bps
        + 8 // created_at
        + 1; // bump
}
