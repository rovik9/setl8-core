use anchor_lang::prelude::*;

use crate::constants::PRODUCT_REGISTRY_SEED;
use crate::instructions::assert_sector_authority;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, amount: u64, product_program_id: Pubkey)]
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
}

/// STUB — auth pattern only, no payout/TraderState/request-ID logic yet
/// (Module 2/3).
pub fn request_payout(
    ctx: Context<RequestPayout>,
    _trader_wallet: Pubkey,
    _amount: u64,
    _product_program_id: Pubkey,
    _challenge_id: u64,
    _proposed_request_id: u64,
) -> Result<()> {
    assert_sector_authority(
        &ctx.accounts.sector_authority.key(),
        &ctx.accounts.product_registry.product_program_id,
    )?;

    // TODO Module 2/3: check TraderState.payout_count against
    // max_payout_count, check proposed_request_id against the vault's own
    // expected next ID, move funds, auto-graduate.
    Ok(())
}
