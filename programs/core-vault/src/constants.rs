use anchor_lang::prelude::*;

/// Seed for the `ProductRegistry` PDA: `[PRODUCT_REGISTRY_SEED,
/// product_program_id.as_ref()]`, one registry per registered sector
/// program.
pub const PRODUCT_REGISTRY_SEED: &[u8] = b"product_registry";

/// SL8's half of the 2-of-2 admin multisig required on every privileged
/// vault instruction (`register_product`, `reactivate_product`,
/// `update_product_config`).
///
/// PLACEHOLDER — this is a freshly generated throwaway keypair
/// (`/tmp/setl8-vault-keys/sl8-admin.json` at scaffold time, also copied to
/// `tests/fixtures/sl8-admin.json` so the Module 1 tests can sign with it).
/// It does not correspond to any real, funded, or otherwise significant
/// wallet. Replace with the real SL8 protocol admin wallet pubkey before any
/// non-local deploy.
pub const SL8_ADMIN_PUBKEY: Pubkey = pubkey!("9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");

/// Rov's half of the 2-of-2 admin multisig. Never a destination for funds on
/// any instruction — its role is strictly the second required signature.
///
/// PLACEHOLDER — see `SL8_ADMIN_PUBKEY` above; same caveat applies
/// (`tests/fixtures/rov-admin.json`). Replace before any non-local deploy.
pub const ROV_ADMIN_PUBKEY: Pubkey = pubkey!("D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");

/// Upper bound on how many `ChallengeSize` tiers a single `ProductRegistry`
/// can hold, used only to size the account's fixed on-chain space (Anchor
/// accounts can't be unbounded). Not specified anywhere in the Module 1
/// brief — **flagged assumption, confirm before Module 2**: raising this
/// later requires a account-migration (realloc), so pick deliberately rather
/// than inheriting this default.
pub const MAX_CHALLENGE_SIZES: usize = 10;

/// On-chain size of one Borsh-serialized `ChallengeSize` (`size: u64, cost:
/// u64`) from `setl8-shared-interfaces`. That crate defines the struct, not
/// its serialized size, so it's recomputed here from its two `u64` fields.
pub const CHALLENGE_SIZE_SPACE: usize = 8 + 8;
