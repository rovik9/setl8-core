use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

use crate::constants::{BPS_DENOMINATOR, FLOOR_BPS, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::VaultState;

#[derive(Accounts)]
pub struct FinalizeHeartbeat<'info> {
    /// Anyone. Pays the transaction fee and nothing else.
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    #[account(address = vault_state.usdc_pool @ VaultError::InvalidTokenAccount)]
    pub usdc_pool: Box<Account<'info, TokenAccount>>,

    #[account(address = vault_state.usdt_pool @ VaultError::InvalidTokenAccount)]
    pub usdt_pool: Box<Account<'info, TokenAccount>>,
}

/// Permissionless. Ends the cycle once every eligible claim has been processed
/// (paid or skipped), and recomputes the reserve floors: 25% of each pool's
/// post-settlement balance. Floors only constrain the future admin withdrawal;
/// they never limit claim settlement.
///
/// `cycle_started_at` is left alone: the minimum gap is measured from the START
/// of the last cycle, not its end.
pub fn finalize_heartbeat(ctx: Context<FinalizeHeartbeat>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let usdc_floor = floor_of(ctx.accounts.usdc_pool.amount)?;
    let usdt_floor = floor_of(ctx.accounts.usdt_pool.amount)?;

    let vs = &mut ctx.accounts.vault_state;
    require!(vs.cycle_active, VaultError::NoCycleInProgress);
    require!(vs.cycle_processed_count == vs.cycle_eligible_count, VaultError::CycleIncomplete);

    vs.usdc_floor = usdc_floor;
    vs.usdt_floor = usdt_floor;
    vs.floor_updated_at = now;
    vs.cycle_active = false;
    Ok(())
}

/// `balance * FLOOR_BPS / 10_000`, rounded down.
fn floor_of(balance: u64) -> Result<u64> {
    let floor = (balance as u128) * FLOOR_BPS / BPS_DENOMINATOR;
    u64::try_from(floor).map_err(|_| error!(VaultError::MathOverflow))
}
