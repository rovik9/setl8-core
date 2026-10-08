use anchor_lang::prelude::*;

use crate::constants::{PAUSE_PLANNED_UPGRADE, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY};
use crate::errors::VaultError;
use crate::state::ProductRegistry;

#[derive(Accounts)]
#[instruction(product_program_id: Pubkey)]
pub struct PauseProduct<'info> {
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

/// Manual, planned pause (upgrade). 2-of-2, mirroring `reactivate_product`.
/// Records the reason as `PAUSE_PLANNED_UPGRADE` so the public status display
/// can tell it apart from a reconciliation-deficit auto-pause, and starts the
/// pause window that freezes trader inactivity clocks.
pub fn pause_product(ctx: Context<PauseProduct>, _product_program_id: Pubkey) -> Result<()> {
    let registry = &mut ctx.accounts.product_registry;
    require!(registry.active, VaultError::ProductAlreadyPaused);
    registry.pause(PAUSE_PLANNED_UPGRADE, Clock::get()?.unix_timestamp);
    Ok(())
}
