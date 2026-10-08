use anchor_lang::prelude::*;

/// Per-depositor bond bookkeeping. PDA `[BOND_CAP_SEED, depositor]`, created by the
/// wallet's first `deposit_bond`.
#[account]
pub struct BondCapTracker {
    pub depositor: Pubkey,
    /// Principal across all of this wallet's OPEN positions (the per-wallet cap
    /// counter). Shrinks when a withdrawal request closes a position.
    pub open_principal_total: u64,
    /// The index the next deposit must use. Monotonic: never decreases and never
    /// reused, even after positions close.
    pub next_deposit_index: u64,
    pub bump: u8,
}

impl BondCapTracker {
    pub const SPACE: usize = 8 // discriminator
        + 32 // depositor
        + 8 // open_principal_total
        + 8 // next_deposit_index
        + 1; // bump
}
