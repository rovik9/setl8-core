use anchor_lang::prelude::*;

use crate::constants::{PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, product_program_id: Pubkey, challenge_id: u64)]
pub struct MarkAbandoned<'info> {
    /// Anyone. Pays the transaction fee and nothing else.
    pub caller: Signer<'info>,

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

/// Permissionless. Flips an `Active` challenge to `Abandoned` only if it is
/// genuinely past the inactivity window, so no caller can kill a live one.
/// Exists for records nobody ever revisits; there is no heartbeat sweep.
pub fn mark_abandoned(
    ctx: Context<MarkAbandoned>,
    _trader_wallet: Pubkey,
    _product_program_id: Pubkey,
    _challenge_id: u64,
) -> Result<()> {
    let ts = &mut ctx.accounts.trader_state;
    require!(ts.status == TraderStatus::Active, VaultError::InvalidTraderStatus);

    let now = Clock::get()?.unix_timestamp;
    let paused_now = ctx.accounts.product_registry.paused_secs_at(now);
    require!(ts.is_stale(now, paused_now), VaultError::NotAbandonable);

    ts.status = TraderStatus::Abandoned;
    Ok(())
}
