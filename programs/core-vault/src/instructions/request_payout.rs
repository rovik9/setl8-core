use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use setl8_shared_interfaces::PayoutOutcome;

use crate::constants::{PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::instructions::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, amount: u64, product_program_id: Pubkey, challenge_id: u64)]
pub struct RequestPayout<'info> {
    /// CPI-auth identity: the calling sector program's own PDA. See
    /// `assert_sector_authority` for what a valid signature here proves.
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
            &challenge_id.to_le_bytes(),
        ],
        bump = trader_state.bump,
    )]
    pub trader_state: Account<'info, TraderState>,
}

/// Checks the challenge, then books one payout against its cap.
///
/// If the challenge is past its inactivity window, this flips it to
/// `Abandoned` and returns **Ok with `PayoutOutcome::Abandoned`**, paying
/// nothing. It must not return an error here: a failed transaction reverts
/// every write, so the `Abandoned` status would never be stored. The sector
/// program must read the return data before telling anyone they were paid.
///
/// NOT YET DONE (next module): the actual token transfer to the trader.
pub fn request_payout(
    ctx: Context<RequestPayout>,
    _trader_wallet: Pubkey,
    amount: u64,
    _product_program_id: Pubkey,
    _challenge_id: u64,
    proposed_request_id: u64,
) -> Result<()> {
    let registry = &mut ctx.accounts.product_registry;
    assert_sector_authority(&ctx.accounts.sector_authority.key(), &registry.product_program_id)?;
    require!(registry.active, VaultError::ProductNotActive);
    require!(amount > 0, VaultError::ZeroAmount);

    let ts = &mut ctx.accounts.trader_state;
    require!(ts.status == TraderStatus::Active, VaultError::InvalidTraderStatus);

    let now = Clock::get()?.unix_timestamp;
    let paused_now = registry.paused_secs_at(now);

    if ts.is_stale(now, paused_now) {
        ts.status = TraderStatus::Abandoned;
        set_return_data(&[PayoutOutcome::Abandoned as u8]);
        return Ok(());
    }

    require!(ts.payout_count < registry.max_payout_count, VaultError::PayoutCapReached);

    // Mutual agreement: the vault, not the sector, decides the next id.
    let expected_request_id = ts.payout_count.checked_add(1).ok_or(VaultError::MathOverflow)?;
    require!(proposed_request_id == expected_request_id, VaultError::RequestIdMismatch);

    // TODO Module 2b: transfer `amount` from the payout pool to the trader.

    ts.payout_count = expected_request_id;
    ts.touch(now, paused_now);
    registry.total_requests_emitted = registry
        .total_requests_emitted
        .checked_add(1)
        .ok_or(VaultError::MathOverflow)?;

    if ts.payout_count >= registry.max_payout_count {
        ts.status = TraderStatus::Graduated;
    }

    set_return_data(&[PayoutOutcome::Paid as u8]);
    Ok(())
}
