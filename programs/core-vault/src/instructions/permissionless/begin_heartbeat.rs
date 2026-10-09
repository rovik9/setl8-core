use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

use crate::constants::{HEARTBEAT_MIN_GAP_SECS, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::VaultState;
use crate::utils::available_snapshot;

#[derive(Accounts)]
pub struct BeginHeartbeat<'info> {
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

/// Permissionless. Opens a heartbeat cycle and freezes its inputs: the total owed
/// and the total available (both pools, a FROZEN pool counting as empty) are snapshotted, so every claim in the
/// cycle is settled with the same ratio no matter the order it is processed in.
///
/// At most one cycle at a time, and at least `HEARTBEAT_MIN_GAP_SECS` between the
/// STARTS of two cycles (fixed in the program). The very first cycle may start
/// immediately. `settle_claims` then pays claims in batches and
/// `finalize_heartbeat` closes the cycle.
pub fn begin_heartbeat(ctx: Context<BeginHeartbeat>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    // A pool the issuer has frozen cannot pay anything, so it is left out of what is
    // "available" (SR-03). Otherwise the cycle's ratio would count money that cannot move
    // and every claim would be under-paid for the whole cycle.
    let available = available_snapshot(
        ctx.accounts.usdc_pool.amount,
        ctx.accounts.usdc_pool.is_frozen(),
        ctx.accounts.usdt_pool.amount,
        ctx.accounts.usdt_pool.is_frozen(),
    )?;

    let vs = &mut ctx.accounts.vault_state;
    require!(!vs.cycle_active, VaultError::CycleInProgress);
    if vs.cycle_started_at != 0 {
        let earliest = vs
            .cycle_started_at
            .checked_add(HEARTBEAT_MIN_GAP_SECS)
            .ok_or(VaultError::MathOverflow)?;
        require!(now >= earliest, VaultError::HeartbeatTooEarly);
    }

    vs.cycle_id = vs.cycle_id.checked_add(1).ok_or(VaultError::MathOverflow)?;
    vs.cycle_started_at = now;
    vs.cycle_active = true;
    vs.cycle_owed_snapshot = vs.open_claims_total;
    vs.cycle_available_snapshot = available;
    vs.cycle_eligible_count = vs.open_claims_count;
    vs.cycle_processed_count = 0;
    Ok(())
}
