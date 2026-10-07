use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use setl8_shared_interfaces::ActivityOutcome;

use crate::constants::{ACTIVITY_THROTTLE_SECS, PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::instructions::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, product_program_id: Pubkey, challenge_id: u64)]
pub struct RecordActivity<'info> {
    pub sector_authority: Signer<'info>,

    #[account(
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

/// Refreshes `last_activity_timestamp`, at most once per
/// `ACTIVITY_THROTTLE_SECS`. If the challenge is already past its inactivity
/// window it is flipped to `Abandoned` instead (Ok + return data, so the
/// write survives) and the sector program must stop treating it as live.
pub fn record_activity(
    ctx: Context<RecordActivity>,
    _trader_wallet: Pubkey,
    _product_program_id: Pubkey,
    _challenge_id: u64,
) -> Result<()> {
    let registry = &ctx.accounts.product_registry;
    assert_sector_authority(&ctx.accounts.sector_authority.key(), &registry.product_program_id)?;

    let ts = &mut ctx.accounts.trader_state;
    require!(ts.status == TraderStatus::Active, VaultError::InvalidTraderStatus);

    let now = Clock::get()?.unix_timestamp;
    let paused_now = registry.paused_secs_at(now);

    if ts.is_stale(now, paused_now) {
        ts.status = TraderStatus::Abandoned;
        set_return_data(&[ActivityOutcome::Abandoned as u8]);
        return Ok(());
    }

    if now.saturating_sub(ts.last_activity_timestamp) < ACTIVITY_THROTTLE_SECS {
        set_return_data(&[ActivityOutcome::Throttled as u8]);
        return Ok(());
    }

    ts.touch(now, paused_now);
    set_return_data(&[ActivityOutcome::Recorded as u8]);
    Ok(())
}
