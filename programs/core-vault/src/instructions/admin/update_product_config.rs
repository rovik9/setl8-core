use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::constants::{
    BPS_DENOMINATOR, MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY,
    SL8_ADMIN_PUBKEY,
};
use crate::errors::VaultError;
use crate::state::ProductRegistry;
use crate::utils::require_distinct_sizes;

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

/// `reset_price_bps` is in basis points of the TIER PRICE (`10_000` = 100%, rounded up),
/// not of the account size. A reset is priced from the CURRENT `challenge_sizes`, so
/// repricing or removing a tier changes or blocks resets of records already `Failed`.
///
/// Two `challenge_sizes` entries with the same `size` are refused (`DuplicateChallengeSize`),
/// whether their costs are equal or not, and the refusal leaves the old config in place.
/// `deposit_reset` still scans every entry and refuses an ambiguous one
/// (`InvalidChallengeTier`) as defence in depth.
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
    // The pool's share of a payment can't exceed the payment. (Also guarded at
    // payment time as defense in depth.)
    require!(fee_split_bps as u128 <= BPS_DENOMINATOR, VaultError::InvalidFeeSplit);
    // Last, so every config that was already refused keeps its error code.
    require_distinct_sizes(&challenge_sizes)?;

    let registry = &mut ctx.accounts.product_registry;
    registry.challenge_sizes = challenge_sizes;
    registry.fee_split_bps = fee_split_bps;
    registry.max_payout_count = max_payout_count;
    registry.reset_price_bps = reset_price_bps;
    Ok(())
}
