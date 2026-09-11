use anchor_lang::prelude::*;

#[error_code]
pub enum VaultError {
    /// CPI-caller identity check failed: the `sector_authority` signer
    /// passed to `deposit_fee` / `request_payout` / `flag_trader_failed`
    /// doesn't match `derive_sector_authority(&registry.product_program_id)`
    /// for the registry being called against.
    #[msg("sector_authority does not match the registered product's derived CPI authority")]
    Unauthorized,

    /// Reserved for Module 2/3 payout logic (`request_payout` will reject
    /// against a paused product once that logic lands). Not triggered by
    /// any code path in Module 1's stubs.
    #[msg("product is not active")]
    ProductNotActive,

    /// One of the two required 2-of-2 admin signers (`sl8_admin`,
    /// `rov_admin`) did not match the expected admin pubkey.
    #[msg("both sl8_admin and rov_admin must sign and match the configured admin pubkeys")]
    MissingMultisigSignature,

    /// Reserved: Anchor's `init` constraint on `ProductRegistry` already
    /// rejects a second registration for the same `product_program_id` at
    /// the runtime level (account already in use) before this instruction's
    /// body runs, so this variant is currently unreachable in Module 1.
    /// Kept because the brief calls for it explicitly — flag if a future
    /// module needs to replace `init` with an explicit existence check that
    /// would actually return this.
    #[msg("a product with this product_program_id is already registered")]
    ProductAlreadyRegistered,

    /// Not in the brief's "at minimum" list — added because `ProductRegistry`
    /// has fixed on-chain space sized off `MAX_CHALLENGE_SIZES`
    /// (constants.rs); without this guard, passing more tiers than that
    /// bound would fail confusingly deep in Borsh serialization instead of
    /// with a clear instruction-level error.
    #[msg("challenge_sizes exceeds MAX_CHALLENGE_SIZES")]
    TooManyChallengeSizes,
}
