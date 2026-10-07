use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::constants::{
    MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY,
};
use crate::errors::VaultError;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(product_program_id: Pubkey)]
pub struct UpdateProductConfig<'info> {
    #[account(address = SL8_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub sl8_admin: Signer<'info>,

    #[account(address = ROV_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub rov_admin: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Account<'info, ProductRegistry>,
}

pub fn update_product_config(
    ctx: Context<UpdateProductConfig>,
    _product_program_id: Pubkey,
    challenge_sizes: Vec<ChallengeSize>,
    fee_split_bps: u16,
    max_payout_count: u64,
    reset_price_bps: Vec<u16>,
) -> Result<()> {
    require!(
        challenge_sizes.len() <= MAX_CHALLENGE_SIZES,
        VaultError::TooManyChallengeSizes
    );
    require!(reset_price_bps.len() <= MAX_RESET_PHASES, VaultError::TooManyResetPhases);

    let registry = &mut ctx.accounts.product_registry;
    registry.challenge_sizes = challenge_sizes;
    registry.fee_split_bps = fee_split_bps;
    registry.max_payout_count = max_payout_count;
    registry.reset_price_bps = reset_price_bps;
    Ok(())
}
