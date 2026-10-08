use anchor_lang::prelude::*;

/// One accepted `request_payout`: an amount the vault owes a trader, queued until
/// a heartbeat cycle pays it (fully or in part).
///
/// PDA: `[PAYOUT_CLAIM_SEED, trader_state, request_id.to_le_bytes()]`.
///
/// Amounts are 6-decimal dollar units (USDC = USDT = $1). `owed` shrinks as
/// cycles pay it down; the claim account is closed when it reaches zero. A claim
/// is never expired or reordered: it stays owed, at equal priority with every
/// other claim, until paid.
#[account]
pub struct PayoutClaim {
    /// The wallet that is paid (its associated token accounts are the only
    /// destinations `settle_claims` accepts).
    pub trader_wallet: Pubkey,
    pub trader_state: Pubkey,
    pub product_program_id: Pubkey,
    /// The `payout_count` this claim was booked under (unique per challenge).
    pub request_id: u64,
    /// Still unpaid, in 6-decimal dollar units.
    pub owed: u64,
    /// `VaultState::cycle_id` when the claim was created. A claim is eligible for
    /// cycle N only if this is `< N`, so a claim made mid-cycle waits for the next.
    pub created_in_cycle: u64,
    /// The last cycle that processed this claim (paid or skipped it); 0 = never.
    pub last_settled_cycle: u64,
    /// Canonical bump, stored so `settle_claims` can verify the address with the
    /// cheap `create_program_address` instead of `find_program_address`.
    pub bump: u8,
}

impl PayoutClaim {
    pub const SPACE: usize = 8 // discriminator
        + 32 * 3 // trader_wallet, trader_state, product_program_id
        + 8 * 4 // request_id, owed, created_in_cycle, last_settled_cycle
        + 1; // bump
}
