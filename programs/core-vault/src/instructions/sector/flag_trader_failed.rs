use anchor_lang::prelude::*;

use crate::constants::{PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::utils::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, product_program_id: Pubkey, challenge_id: u64)]
pub struct FlagTraderFailed<'info> {
    /// CPI-auth identity: the calling sector program's own PDA. See
    /// `assert_sector_authority` for what a valid signature here proves.
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

/// Marks an `Active` challenge `Failed`. Terminal for this `challenge_id`;
/// a retry is a new record via `deposit_fee` or `deposit_reset`. Allowed
/// while the product is paused -- a breach is a breach.
pub fn flag_trader_failed(
    ctx: Context<FlagTraderFailed>,
    _trader_wallet: Pubkey,
    _product_program_id: Pubkey,
    _challenge_id: u64,
) -> Result<()> {
    assert_sector_authority(
        &ctx.accounts.sector_authority.key(),
        &ctx.accounts.product_registry.product_program_id,
    )?;

    let ts = &mut ctx.accounts.trader_state;
    require!(ts.status == TraderStatus::Active, VaultError::InvalidTraderStatus);
    ts.status = TraderStatus::Failed;
    Ok(())
}
