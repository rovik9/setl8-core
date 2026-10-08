use anchor_lang::prelude::*;

/// Which of the vault's two payout pools an instruction means. Pools are always
/// handled one at a time: USDC and USDT are never combined.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolSide {
    Usdc,
    Usdt,
}

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
    /// Pool floors; written when a heartbeat cycle finalizes. Zero until then.
    pub usdc_floor: u64,
    pub usdt_floor: u64,
    pub floor_updated_at: i64,

    // ---- payout queue ----
    /// Number of open `PayoutClaim` accounts.
    pub open_claims_count: u64,
    /// Sum of `owed` over all open claims (6-decimal dollar units).
    pub open_claims_total: u64,

    // ---- marketing withdrawals (informational) ----
    /// Cumulative USDC taken by `admin_withdraw_marketing_funds`.
    pub marketing_withdrawn_usdc: u64,
    /// Cumulative USDT taken by `admin_withdraw_marketing_funds`.
    pub marketing_withdrawn_usdt: u64,

    // ---- bond vault ----
    /// Principal across ALL open bond positions (the global cap counter).
    pub bond_principal_open_total: u64,
    /// Cumulative bond withdrawal fees kept in the pools (informational): no tokens
    /// move when a withdrawal is requested, so the fee is simply not owed.
    pub bond_withdrawal_fees_retained: u64,

    // ---- heartbeat cycle ----
    /// Id of the most recently BEGUN cycle; 0 = none yet.
    pub cycle_id: u64,
    /// Start time of that cycle; 0 = never started. The minimum gap between
    /// cycles is measured from here.
    pub cycle_started_at: i64,
    pub cycle_active: bool,
    /// `open_claims_total` when the cycle began.
    pub cycle_owed_snapshot: u64,
    /// USDC pool + USDT pool balance when the cycle began.
    pub cycle_available_snapshot: u64,
    /// `open_claims_count` when the cycle began: how many claims must be
    /// processed before the cycle may finalize.
    pub cycle_eligible_count: u64,
    pub cycle_processed_count: u64,

    pub bump: u8,
}

impl VaultState {
    pub const SPACE: usize = 8 // discriminator
        + 32 * 5 // usdc_mint, usdt_mint, usdc_pool, usdt_pool, sl8_wallet
        + 8 // usdc_floor
        + 8 // usdt_floor
        + 8 // floor_updated_at
        + 8 // open_claims_count
        + 8 // open_claims_total
        + 8 // marketing_withdrawn_usdc
        + 8 // marketing_withdrawn_usdt
        + 8 // bond_principal_open_total
        + 8 // bond_withdrawal_fees_retained
        + 8 // cycle_id
        + 8 // cycle_started_at
        + 1 // cycle_active
        + 8 // cycle_owed_snapshot
        + 8 // cycle_available_snapshot
        + 8 // cycle_eligible_count
        + 8 // cycle_processed_count
        + 1; // bump

    pub fn mint_of(&self, side: PoolSide) -> Pubkey {
        match side {
            PoolSide::Usdc => self.usdc_mint,
            PoolSide::Usdt => self.usdt_mint,
        }
    }

    pub fn pool_of(&self, side: PoolSide) -> Pubkey {
        match side {
            PoolSide::Usdc => self.usdc_pool,
            PoolSide::Usdt => self.usdt_pool,
        }
    }

    /// The stored reserve floor of `side` (set at `finalize_heartbeat`).
    pub fn floor_of(&self, side: PoolSide) -> u64 {
        match side {
            PoolSide::Usdc => self.usdc_floor,
            PoolSide::Usdt => self.usdt_floor,
        }
    }

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
