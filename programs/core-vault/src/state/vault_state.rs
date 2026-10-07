use anchor_lang::prelude::*;

/// Singleton vault configuration: which two stablecoins the vault accepts
/// and where their payout pools live. One per deployment, created once by
/// `init_vault`.
#[account]
pub struct VaultState {
    pub usdc_mint: Pubkey,
    pub usdt_mint: Pubkey,
    /// Payout-pool token accounts (PDAs, token authority = this account).
    pub usdc_pool: Pubkey,
    pub usdt_pool: Pubkey,
    /// Owner of the SL8-side destination token accounts (`SL8_ADMIN_PUBKEY`).
    pub sl8_wallet: Pubkey,
    /// Pool floors; written by the Module 3 heartbeat. Zero and unused until
    /// then.
    pub usdc_floor: u64,
    pub usdt_floor: u64,
    pub floor_updated_at: i64,
    pub bump: u8,
}

impl VaultState {
    pub const SPACE: usize = 8 // discriminator
        + 32 * 5 // usdc_mint, usdt_mint, usdc_pool, usdt_pool, sl8_wallet
        + 8 // usdc_floor
        + 8 // usdt_floor
        + 8 // floor_updated_at
        + 1; // bump

    /// The pool token account for `mint`, if `mint` is one of the vault's.
    pub fn pool_for(&self, mint: &Pubkey) -> Option<Pubkey> {
        if *mint == self.usdc_mint {
            Some(self.usdc_pool)
        } else if *mint == self.usdt_mint {
            Some(self.usdt_pool)
        } else {
            None
        }
    }
}
