use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::constants::{
    MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, PAUSE_NONE, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY,
};
use crate::errors::VaultError;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(product_program_id: Pubkey)]
pub struct RegisterProduct<'info> {
    /// SL8's half of the 2-of-2 admin multisig. Also pays for the new
    /// `ProductRegistry` account.
    #[account(mut, address = SL8_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub sl8_admin: Signer<'info>,

    /// Rov's half of the 2-of-2 admin multisig. Never a fund destination on
    /// any instruction — its role is strictly this second required
    /// signature.
    #[account(address = ROV_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub rov_admin: Signer<'info>,

    /// One PDA per sector program. `init` means Anchor itself rejects a
    /// second `register_product` for the same `product_program_id` at the
    /// runtime level (account already in use) before this handler ever
    /// runs — see `VaultError::ProductAlreadyRegistered`'s doc comment.
    #[account(
        init,
        payer = sl8_admin,
        space = ProductRegistry::SPACE,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump,
    )]
    pub product_registry: Account<'info, ProductRegistry>,

    pub system_program: Program<'info, System>,
}

pub fn register_product(
    ctx: Context<RegisterProduct>,
    product_program_id: Pubkey,
    fee_split_bps: u16,
    challenge_sizes: Vec<ChallengeSize>,
    max_payout_count: u64,
    reset_price_bps: Vec<u16>,
) -> Result<()> {
    require!(
        challenge_sizes.len() <= MAX_CHALLENGE_SIZES,
        VaultError::TooManyChallengeSizes
    );
    require!(reset_price_bps.len() <= MAX_RESET_PHASES, VaultError::TooManyResetPhases);

    let registry = &mut ctx.accounts.product_registry;
    registry.product_program_id = product_program_id;
    registry.challenge_sizes = challenge_sizes;
    registry.fee_split_bps = fee_split_bps;
    registry.max_payout_count = max_payout_count;
    registry.active = true;
    registry.total_requests_emitted = 0;
    registry.reset_price_bps = reset_price_bps;
    registry.pause_reason = PAUSE_NONE;
    registry.paused_since = 0;
    registry.total_paused_secs = 0;
    registry.bump = ctx.bumps.product_registry;
    Ok(())
}
