use anchor_lang::prelude::*;

use crate::constants::PRODUCT_REGISTRY_SEED;
use crate::instructions::assert_sector_authority;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(amount: u64, product_program_id: Pubkey)]
pub struct DepositFee<'info> {
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

/// STUB — auth pattern only, no fee-movement logic yet (Module 2).
pub fn deposit_fee(
    ctx: Context<DepositFee>,
    _amount: u64,
    _product_program_id: Pubkey,
    _challenge_id: u64,
) -> Result<()> {
    assert_sector_authority(
        &ctx.accounts.sector_authority.key(),
        &ctx.accounts.product_registry.product_program_id,
    )?;

    // TODO Module 2: create/update TraderState, move the fee into the
    // vault's payout pool per fee_split_bps.
    Ok(())
}
