//! Plain data the decisions are made on. Nothing here touches the network.

use anchor_lang::prelude::Pubkey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultView {
    pub usdc_mint: Pubkey,
    pub usdt_mint: Pubkey,
    pub usdc_pool: Pubkey,
    pub usdt_pool: Pubkey,
    pub open_claims_count: u64,
    pub open_claims_total: u64,
    pub cycle_id: u64,
    pub cycle_started_at: i64,
    pub cycle_active: bool,
    pub cycle_owed_snapshot: u64,
    pub cycle_available_snapshot: u64,
    pub cycle_eligible_count: u64,
    pub cycle_processed_count: u64,
}

impl From<&core_vault::state::VaultState> for VaultView {
    fn from(v: &core_vault::state::VaultState) -> Self {
        VaultView {
            usdc_mint: v.usdc_mint,
            usdt_mint: v.usdt_mint,
            usdc_pool: v.usdc_pool,
            usdt_pool: v.usdt_pool,
            open_claims_count: v.open_claims_count,
            open_claims_total: v.open_claims_total,
            cycle_id: v.cycle_id,
            cycle_started_at: v.cycle_started_at,
            cycle_active: v.cycle_active,
            cycle_owed_snapshot: v.cycle_owed_snapshot,
            cycle_available_snapshot: v.cycle_available_snapshot,
            cycle_eligible_count: v.cycle_eligible_count,
            cycle_processed_count: v.cycle_processed_count,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolView {
    pub amount: u64,
    pub frozen: bool,
}

impl PoolView {
    /// What the heartbeat can actually pay from this pool (a frozen pool counts as empty: SR-03).
    pub fn spendable(&self) -> u64 {
        if self.frozen {
            0
        } else {
            self.amount
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductView {
    pub id: Pubkey,
    pub registry: Pubkey,
    pub active: bool,
    pub pause_reason: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimView {
    pub address: Pubkey,
    pub trader_wallet: Pubkey,
    pub owed: u64,
    pub created_in_cycle: u64,
    pub last_settled_cycle: u64,
    pub kind: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct World {
    /// Unix time of the chain (the same clock the program reads), not the local clock.
    pub now: i64,
    pub vault: VaultView,
    pub usdc: PoolView,
    pub usdt: PoolView,
    pub products: Vec<ProductView>,
    pub payer_balance: u64,
}
