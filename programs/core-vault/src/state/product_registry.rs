use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::constants::{CHALLENGE_SIZE_SPACE, MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, PAUSE_NONE};

/// One per registered sector program (lev-trading, options, ...): its
/// configuration, its pause state and the request counters that
/// `reconcile_product` compares with the sector's payout tally.
#[account]
pub struct ProductRegistry {
    /// The sector program's own on-chain program ID. This is what
    /// `derive_sector_authority` is checked against on every CPI-auth-context
    /// instruction — see `utils::assert_sector_authority`.
    pub product_program_id: Pubkey,
    /// Challenge size/cost tiers this product offers. Bounded to
    /// `MAX_CHALLENGE_SIZES` entries by the account's fixed on-chain space.
    pub challenge_sizes: Vec<ChallengeSize>,
    /// Basis points of every fee / reset payment that goes into the payout
    /// pool (e.g. 6500 for 65%); the remainder goes to the SL8 wallet.
    pub fee_split_bps: u16,
    /// Cap on outstanding payouts the vault will allow for this product.
    pub max_payout_count: u64,
    pub active: bool,
    /// Number of `request_payout` calls accepted and queued for this product
    /// (the stale/Abandoned path does not count). Reconciled against the sector's
    /// payout tally by `reconcile_product`.
    pub total_requests_emitted: u64,
    /// Phase-reset prices in basis points of account size, indexed by the
    /// 0-based phase a trader failed in. Empty = no phase resets offered.
    pub reset_price_bps: Vec<u16>,
    /// Why the product is paused (`PAUSE_*`), `PAUSE_NONE` when active.
    pub pause_reason: u8,
    /// Unix time the current pause began; 0 when not paused.
    pub paused_since: i64,
    /// Total seconds spent paused in completed pauses. Together with
    /// `paused_since` this lets the inactivity clock freeze during pauses.
    pub total_paused_secs: i64,
    /// Sum of the `amount` of every accepted `request_payout`, bumped at the same
    /// moment as `total_requests_emitted` (6-decimal dollar units). Only ever
    /// grows. Reconciled against the sector's payout tally.
    pub total_requested_amount: u64,
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
        + 4 + (MAX_RESET_PHASES * 2) // reset_price_bps
        + 1 // pause_reason
        + 8 // paused_since
        + 8 // total_paused_secs
        + 8 // total_requested_amount
        + 1; // bump

    /// Total seconds this product has spent paused as of `now`, including a
    /// pause still in progress.
    pub fn paused_secs_at(&self, now: i64) -> i64 {
        let ongoing = if self.paused_since > 0 { (now - self.paused_since).max(0) } else { 0 };
        self.total_paused_secs.saturating_add(ongoing)
    }

    /// Marks the product paused for `reason`. Caller checks it is active.
    pub fn pause(&mut self, reason: u8, now: i64) {
        self.active = false;
        self.pause_reason = reason;
        self.paused_since = now;
    }

    /// Ends a pause, banking the paused time so inactivity clocks skip it.
    pub fn resume(&mut self, now: i64) {
        if self.paused_since > 0 {
            self.total_paused_secs = self.total_paused_secs.saturating_add((now - self.paused_since).max(0));
        }
        self.paused_since = 0;
        self.pause_reason = PAUSE_NONE;
        self.active = true;
    }
}
