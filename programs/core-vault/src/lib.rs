use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

pub mod constants;
pub mod errors;
pub mod instructions;
pub mod state;

use instructions::*;

declare_id!("2Z6WNsj4hNhKhmK9Cj3sXV5San9VYhh8gwtyvBfpP6ft");

/// Module 1: `ProductRegistry` + the CPI-auth gateway. Every instruction
/// name below is exact-match (snake_case) to what `setl8-shared-interfaces`
/// v0.2.0 assumed when it precomputed each `*_DISCRIMINATOR` constant as
/// `sha256("global:<fn_name>")[..8]` — Anchor's own default discriminator
/// scheme, which this program does not override anywhere in this file. That
/// exact-name match (not any shared constant) is what keeps this program's
/// wire format compatible with that crate's instruction builders without
/// either repo hardcoding the other's discriminator bytes.
#[program]
pub mod core_vault {
    use super::*;

    /// One-time vault setup: records the USDC/USDT mints and creates the two
    /// payout-pool token accounts. See `instructions::init_vault`.
    pub fn init_vault(ctx: Context<InitVault>, usdc_mint: Pubkey, usdt_mint: Pubkey) -> Result<()> {
        instructions::init_vault(ctx, usdc_mint, usdt_mint)
    }

    pub fn register_product(
        ctx: Context<RegisterProduct>,
        product_program_id: Pubkey,
        fee_split_bps: u16,
        challenge_sizes: Vec<ChallengeSize>,
        max_payout_count: u64,
        reset_price_bps: Vec<u16>,
    ) -> Result<()> {
        instructions::register_product(
            ctx,
            product_program_id,
            fee_split_bps,
            challenge_sizes,
            max_payout_count,
            reset_price_bps,
        )
    }

    pub fn reactivate_product(ctx: Context<ReactivateProduct>, product_program_id: Pubkey) -> Result<()> {
        instructions::reactivate_product(ctx, product_program_id)
    }

    pub fn update_product_config(
        ctx: Context<UpdateProductConfig>,
        product_program_id: Pubkey,
        challenge_sizes: Vec<ChallengeSize>,
        fee_split_bps: u16,
        max_payout_count: u64,
        reset_price_bps: Vec<u16>,
    ) -> Result<()> {
        instructions::update_product_config(
            ctx,
            product_program_id,
            challenge_sizes,
            fee_split_bps,
            max_payout_count,
            reset_price_bps,
        )
    }

    /// Manual planned pause, 2-of-2. See `instructions::pause_product`.
    pub fn pause_product(ctx: Context<PauseProduct>, product_program_id: Pubkey) -> Result<()> {
        instructions::pause_product(ctx, product_program_id)
    }

    /// Creates the `TraderState` for a new challenge. Token movement is the
    /// next module; see `instructions::deposit_fee`.
    pub fn deposit_fee(
        ctx: Context<DepositFee>,
        amount: u64,
        product_program_id: Pubkey,
        challenge_id: u64,
        trader_wallet: Pubkey,
        account_size: u64,
    ) -> Result<()> {
        instructions::deposit_fee(ctx, amount, product_program_id, challenge_id, trader_wallet, account_size)
    }

    /// Phase-specific reset of a `Failed` record. See `instructions::deposit_reset`.
    pub fn deposit_reset(
        ctx: Context<DepositReset>,
        amount: u64,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        prev_challenge_id: u64,
        new_challenge_id: u64,
        reset_phase: u8,
    ) -> Result<()> {
        instructions::deposit_reset(
            ctx,
            amount,
            trader_wallet,
            product_program_id,
            prev_challenge_id,
            new_challenge_id,
            reset_phase,
        )
    }

    /// Refreshes a challenge's inactivity clock. See `instructions::record_activity`.
    pub fn record_activity(
        ctx: Context<RecordActivity>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::record_activity(ctx, trader_wallet, product_program_id, challenge_id)
    }

    /// Permissionless. See `instructions::mark_abandoned`.
    pub fn mark_abandoned(
        ctx: Context<MarkAbandoned>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::mark_abandoned(ctx, trader_wallet, product_program_id, challenge_id)
    }

    /// Books one payout against the challenge cap. Token transfer is the next
    /// module; see `instructions::request_payout`.
    pub fn request_payout(
        ctx: Context<RequestPayout>,
        trader_wallet: Pubkey,
        amount: u64,
        product_program_id: Pubkey,
        challenge_id: u64,
        proposed_request_id: u64,
    ) -> Result<()> {
        instructions::request_payout(ctx, trader_wallet, amount, product_program_id, challenge_id, proposed_request_id)
    }

    /// Marks a challenge `Failed`. See `instructions::flag_trader_failed`.
    pub fn flag_trader_failed(
        ctx: Context<FlagTraderFailed>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::flag_trader_failed(ctx, trader_wallet, product_program_id, challenge_id)
    }
}
