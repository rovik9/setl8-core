use anchor_lang::prelude::*;

use crate::constants::{PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY};
use crate::errors::VaultError;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(product_program_id: Pubkey)]
pub struct ReactivateProduct<'info> {
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

/// Flips `active = true` on an existing, previously-paused registry entry.
/// Deliberately does nothing else: no reset of `total_requests_emitted`, no
/// reconciliation run. Reactivation is a manual-review decision, not a fresh
/// start: the next `reconcile_product` compares the sector's tally with
/// whatever counts were already there, and pauses the product again if they
/// still disagree. Idempotent on an already-active product.
pub fn reactivate_product(ctx: Context<ReactivateProduct>, _product_program_id: Pubkey) -> Result<()> {
    // `resume` also banks the time spent paused so trader inactivity clocks
    // skip it.
    ctx.accounts.product_registry.resume(Clock::get()?.unix_timestamp);
    Ok(())
}
