use anchor_lang::prelude::*;

/// `PayoutClaim::kind` for a trader payout queued by `request_payout`.
pub const CLAIM_KIND_TRADER: u8 = 0;
/// `PayoutClaim::kind` for a bond withdrawal queued by `request_bond_payout`.
pub const CLAIM_KIND_BOND: u8 = 1;

/// One accepted `request_payout`: an amount the vault owes a trader, queued until
/// a heartbeat cycle pays it (fully or in part).
///
/// PDA: `[PAYOUT_CLAIM_SEED, trader_state, request_id.to_le_bytes()]`.
///
/// A claim is either a trader payout (`kind` 0, PDA `[PAYOUT_CLAIM_SEED,
/// trader_state, request_id]`) or a bond withdrawal (`kind` 1, PDA
/// `[BOND_CLAIM_SEED, depositor, deposit_index]`, with `trader_wallet` = the
/// depositor, `trader_state` = the closed position's key, `request_id` = the
/// deposit index and `product_program_id` = the default key). Settlement treats
/// both identically; only the address derivation differs.
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
    /// `CLAIM_KIND_TRADER` or `CLAIM_KIND_BOND`; any other value is invalid.
    pub kind: u8,
    /// Canonical bump, stored so `settle_claims` can verify the address with the
    /// cheap `create_program_address` instead of `find_program_address`.
    pub bump: u8,
}

impl PayoutClaim {
    pub const SPACE: usize = 8 // discriminator
        + 32 * 3 // trader_wallet, trader_state, product_program_id
        + 8 * 4 // request_id, owed, created_in_cycle, last_settled_cycle
        + 1 // kind
        + 1; // bump
}
