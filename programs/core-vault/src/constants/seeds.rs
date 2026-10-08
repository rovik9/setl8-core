//! PDA seeds. Every account this program owns is derived from one of these.

/// Seed for the `ProductRegistry` PDA: `[PRODUCT_REGISTRY_SEED,
/// product_program_id.as_ref()]`, one registry per registered sector
/// program.
pub const PRODUCT_REGISTRY_SEED: &[u8] = b"product_registry";

/// Seed for the `TraderState` PDA: `[TRADER_STATE_SEED,
/// product_program_id, trader_wallet, challenge_id.to_le_bytes()]`. One
/// record per wallet + product + challenge; a new purchase is always a new
/// record, never a reuse of an old one.
pub const TRADER_STATE_SEED: &[u8] = b"trader_state";

/// Seed for the singleton `VaultState` PDA: `[VAULT_STATE_SEED,
/// SL8_ADMIN_PUBKEY, ROV_ADMIN_PUBKEY]`.
pub const VAULT_STATE_SEED: &[u8] = b"vault_state";

/// Seed for the two payout-pool token accounts: `[POOL_SEED, vault_state,
/// mint]`. Their token authority is the `VaultState` PDA.
pub const POOL_SEED: &[u8] = b"pool";

/// Seed for a `PayoutClaim` PDA: `[PAYOUT_CLAIM_SEED, trader_state_key,
/// request_id.to_le_bytes()]`. One claim per accepted `request_payout`; the
/// request id is unique per challenge, so a claim address is never reused.
pub const PAYOUT_CLAIM_SEED: &[u8] = b"payout_claim";

/// Seed for a `BondPosition` PDA: `[BOND_SEED, depositor, deposit_index.to_le_bytes()]`.
/// The index comes from the depositor's `BondCapTracker` and is never reused.
pub const BOND_SEED: &[u8] = b"bond";

/// Seed for a depositor's `BondCapTracker` PDA: `[BOND_CAP_SEED, depositor]`.
pub const BOND_CAP_SEED: &[u8] = b"bond_cap";

/// Seed for the `PayoutClaim` a bond withdrawal creates (claim kind 1):
/// `[BOND_CLAIM_SEED, depositor, deposit_index.to_le_bytes()]`. A position is
/// closed by the request, so its index (and this address) is used exactly once.
pub const BOND_CLAIM_SEED: &[u8] = b"bond_claim";
