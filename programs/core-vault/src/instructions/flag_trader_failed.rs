use anchor_lang::prelude::*;

use crate::constants::PRODUCT_REGISTRY_SEED;
use crate::instructions::assert_sector_authority;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, product_program_id: Pubkey)]
pub struct FlagTraderFailed<'info> {
    /// CPI-auth identity: the calling sector program's own PDA. See
    /// `assert_sector_authority` for what a valid signature here proves.
    pub sector_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Account<'info, ProductRegistry>,
}

/// STUB — auth pattern only, no TraderState mutation yet (Module 2).
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

    // TODO Module 2: set TraderState.status = Failed, terminal for this
    // challenge_id.
    Ok(())
}
