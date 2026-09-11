mod deposit_fee;
mod flag_trader_failed;
mod reactivate_product;
mod register_product;
mod request_payout;
mod update_product_config;

pub use deposit_fee::*;
pub use flag_trader_failed::*;
pub use reactivate_product::*;
pub use register_product::*;
pub use request_payout::*;
pub use update_product_config::*;

use anchor_lang::prelude::*;
use setl8_shared_interfaces::derive_sector_authority;

use crate::errors::VaultError;

/// The core CPI-auth security invariant shared by every inbound
/// sector-program call (`deposit_fee`, `request_payout`,
/// `flag_trader_failed`).
///
/// A PDA can only be signed via `invoke_signed` by the program it was
/// derived from. So a valid signature from `sector_authority` here —
/// checked against `derive_sector_authority(&registry.product_program_id)`,
/// the seed/derivation owned by `setl8-shared-interfaces`, never
/// re-implemented locally — is cryptographic proof the call actually
/// originated from the program registered as `product_program_id`, not a
/// self-reported claim. Accepting a caller-supplied program-id field
/// instead of this check would let any program impersonate any registered
/// sector program and corrupt or drain that product's state.
pub fn assert_sector_authority(sector_authority: &Pubkey, registry_product_program_id: &Pubkey) -> Result<()> {
    let (expected_sector_authority, _bump) = derive_sector_authority(registry_product_program_id);
    require_keys_eq!(*sector_authority, expected_sector_authority, VaultError::Unauthorized);
    Ok(())
}
