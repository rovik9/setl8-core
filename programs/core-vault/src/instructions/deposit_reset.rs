use anchor_lang::prelude::*;

use crate::constants::{PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::instructions::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

#[derive(Accounts)]
#[instruction(
    amount: u64,
    trader_wallet: Pubkey,
    product_program_id: Pubkey,
    prev_challenge_id: u64,
    new_challenge_id: u64
)]
pub struct DepositReset<'info> {
    pub sector_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Account<'info, ProductRegistry>,

    #[account(
        mut,
        seeds = [
            TRADER_STATE_SEED,
            product_program_id.as_ref(),
            trader_wallet.as_ref(),
            &prev_challenge_id.to_le_bytes(),
        ],
        bump = prev_trader_state.bump,
    )]
    pub prev_trader_state: Account<'info, TraderState>,

    #[account(
        init,
        payer = payer,
        space = TraderState::SPACE,
        seeds = [
            TRADER_STATE_SEED,
            product_program_id.as_ref(),
            trader_wallet.as_ref(),
            &new_challenge_id.to_le_bytes(),
        ],
        bump,
    )]
    pub new_trader_state: Account<'info, TraderState>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

/// Phase-specific reset: restart at a reduced price instead of a full rebuy.
///
/// The vault copies `payout_count` and `account_size` from the previous
/// record itself, so a sector bug can never give a reset trader more payouts
/// than they have left. `reset_phase` is sector-supplied and used only to
/// pick the price; the worst a wrong value does is mis-price a reset.
/// Only a `Failed` record can be reset (not `Abandoned`), once.
pub fn deposit_reset(
    ctx: Context<DepositReset>,
    amount: u64,
    trader_wallet: Pubkey,
    product_program_id: Pubkey,
    _prev_challenge_id: u64,
    new_challenge_id: u64,
    reset_phase: u8,
) -> Result<()> {
    let registry = &ctx.accounts.product_registry;
    assert_sector_authority(&ctx.accounts.sector_authority.key(), &registry.product_program_id)?;
    require!(registry.active, VaultError::ProductNotActive);

    let prev = &mut ctx.accounts.prev_trader_state;
    require!(
        prev.status == TraderStatus::Failed && !prev.reset_used,
        VaultError::ResetNotAllowed
    );

    let bps = *registry
        .reset_price_bps
        .get(reset_phase as usize)
        .ok_or(VaultError::InvalidResetPhase)?;
    let expected = (prev.account_size as u128)
        .checked_mul(bps as u128)
        .ok_or(VaultError::MathOverflow)?
        / 10_000u128;
    require!(amount as u128 == expected, VaultError::WrongAmount);

    let now = Clock::get()?.unix_timestamp;
    let paused_now = registry.paused_secs_at(now);

    prev.reset_used = true;

    let ts = &mut ctx.accounts.new_trader_state;
    ts.trader_wallet = trader_wallet;
    ts.product_program_id = product_program_id;
    ts.challenge_id = new_challenge_id;
    ts.account_size = prev.account_size;
    ts.payout_count = prev.payout_count;
    ts.status = TraderStatus::Active;
    ts.last_activity_timestamp = now;
    ts.paused_secs_snapshot = paused_now;
    ts.reset_used = false;
    ts.bump = ctx.bumps.new_trader_state;

    // TODO Module 2b: move `amount` into the payout pool (same split as
    // deposit_fee).
    Ok(())
}
