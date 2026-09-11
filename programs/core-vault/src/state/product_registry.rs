use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::constants::{CHALLENGE_SIZE_SPACE, MAX_CHALLENGE_SIZES};

/// One per registered sector program (lev-trading, options, ...). Extends
/// the Module 1 scope only as far as registration/config; trader-state and
/// fund-movement fields land in later modules.
#[account]
pub struct ProductRegistry {
    /// The sector program's own on-chain program ID. This is what
    /// `derive_sector_authority` is checked against on every CPI-auth-context
    /// instruction — see `instructions::assert_sector_authority`.
    pub product_program_id: Pubkey,
    /// Challenge size/cost tiers this product offers. Bounded to
    /// `MAX_CHALLENGE_SIZES` entries by the account's fixed on-chain space.
    pub challenge_sizes: Vec<ChallengeSize>,
    /// Basis points of collected fees this product's operator receives
    /// (e.g. 6500 for 65%).
    pub fee_split_bps: u16,
    /// Cap on outstanding payouts the vault will allow for this product.
    pub max_payout_count: u64,
    pub active: bool,
    /// Running count for heartbeat reconciliation (Module 3+); untouched by
    /// Module 1.
    pub total_requests_emitted: u64,
    /// Canonical PDA bump for `[PRODUCT_REGISTRY_SEED,
    /// product_program_id.as_ref()]`, stored so later instructions can pass
    /// `bump = product_registry.bump` instead of re-deriving.
    pub bump: u8,
}

impl ProductRegistry {
    /// Fixed on-chain space: 8-byte Anchor discriminator + every field at
    /// its worst case, including `MAX_CHALLENGE_SIZES` challenge-size
    /// entries and the Vec's own 4-byte Borsh length prefix.
    pub const SPACE: usize = 8 // discriminator
        + 32 // product_program_id
        + 4 + (MAX_CHALLENGE_SIZES * CHALLENGE_SIZE_SPACE) // challenge_sizes
        + 2 // fee_split_bps
        + 8 // max_payout_count
        + 1 // active
        + 8 // total_requests_emitted
        + 1; // bump
}
