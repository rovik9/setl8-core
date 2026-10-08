use anchor_lang::prelude::*;
use setl8_shared_interfaces::derive_payout_tally;

use crate::constants::{PAUSE_RECONCILIATION_DEFICIT, PRODUCT_REGISTRY_SEED};
use crate::errors::VaultError;
use crate::state::ProductRegistry;
use crate::utils::{read_tally, tally_matches, TallyState};

#[derive(Accounts)]
#[instruction(product_program_id: Pubkey)]
pub struct ReconcileProduct<'info> {
    /// Anyone. Pays the transaction fee and nothing else.
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Box<Account<'info, ProductRegistry>>,

    /// CHECK: read-only. Its address is checked in the handler against
    /// `derive_payout_tally(product_program_id)`, and its owner and contents are
    /// interpreted by `utils::read_tally`, which never trusts them.
    pub payout_tally: UncheckedAccount<'info>,
}

/// Permissionless. Compares the sector's payout tally with the vault's own books
/// for the product, and **pauses the product on any mismatch**.
///
/// * The product must be active; a paused product is neither re-paused nor given
///   a new reason (`ProductAlreadyPaused`).
/// * `payout_tally` must be exactly the sector's canonical tally address, else
///   `InvalidTally`: a wrong address is a hard error, so nobody can pause a
///   product by passing junk.
/// * The tally is read with the shared crate's own parser. Missing (no data, not
///   owned by the sector) counts as `0 / 0`; unparseable, or data owned by
///   someone else, is a mismatch.
/// * `requested_count` must equal `total_requests_emitted` AND `requested_total`
///   must equal `total_requested_amount`.
///
/// On mismatch the registry is paused with `PAUSE_RECONCILIATION_DEFICIT` and the
/// instruction still returns **Ok**: an error would revert the pause. Un-pausing
/// stays 2-of-2 (`reactivate_product`). The heartbeat instructions do not call
/// this; the keeper runs it for every product before each `begin_heartbeat`.
pub fn reconcile_product(ctx: Context<ReconcileProduct>, product_program_id: Pubkey) -> Result<()> {
    let registry = &mut ctx.accounts.product_registry;
    require!(registry.active, VaultError::ProductAlreadyPaused);

    let (expected, _) = derive_payout_tally(&product_program_id);
    require_keys_eq!(ctx.accounts.payout_tally.key(), expected, VaultError::InvalidTally);

    let tally = read_tally(&ctx.accounts.payout_tally.to_account_info(), &registry.product_program_id);
    if tally_matches(tally, registry.total_requests_emitted, registry.total_requested_amount) {
        return Ok(());
    }

    match tally {
        TallyState::Valid(t) => msg!(
            "reconcile_product: MISMATCH tally_count={} tally_total={} vault_count={} vault_total={}",
            t.requested_count,
            t.requested_total,
            registry.total_requests_emitted,
            registry.total_requested_amount
        ),
        TallyState::Missing => msg!(
            "reconcile_product: MISMATCH tally_count=0 tally_total=0 (missing) vault_count={} vault_total={}",
            registry.total_requests_emitted,
            registry.total_requested_amount
        ),
        TallyState::Invalid => msg!(
            "reconcile_product: MISMATCH tally unreadable vault_count={} vault_total={}",
            registry.total_requests_emitted,
            registry.total_requested_amount
        ),
    }
    registry.pause(PAUSE_RECONCILIATION_DEFICIT, Clock::get()?.unix_timestamp);
    Ok(())
}
