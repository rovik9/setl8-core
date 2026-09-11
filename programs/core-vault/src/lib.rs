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

    pub fn register_product(
        ctx: Context<RegisterProduct>,
        product_program_id: Pubkey,
        fee_split_bps: u16,
        challenge_sizes: Vec<ChallengeSize>,
        max_payout_count: u64,
    ) -> Result<()> {
        instructions::register_product(ctx, product_program_id, fee_split_bps, challenge_sizes, max_payout_count)
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
    ) -> Result<()> {
        instructions::update_product_config(ctx, product_program_id, challenge_sizes, fee_split_bps, max_payout_count)
    }

    /// STUB — see `instructions::deposit_fee`. CPI-auth enforced; deposit
    /// logic is `// TODO Module 2`.
    pub fn deposit_fee(
        ctx: Context<DepositFee>,
        amount: u64,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::deposit_fee(ctx, amount, product_program_id, challenge_id)
    }

    /// STUB — see `instructions::request_payout`. CPI-auth enforced; payout
    /// logic is `// TODO Module 2/3`.
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

    /// STUB — see `instructions::flag_trader_failed`. CPI-auth enforced;
    /// state mutation is `// TODO Module 2`.
    pub fn flag_trader_failed(
        ctx: Context<FlagTraderFailed>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::flag_trader_failed(ctx, trader_wallet, product_program_id, challenge_id)
    }
}
