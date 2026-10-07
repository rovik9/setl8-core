use anchor_lang::prelude::*;

/// Seed for the `ProductRegistry` PDA: `[PRODUCT_REGISTRY_SEED,
/// product_program_id.as_ref()]`, one registry per registered sector
/// program.
pub const PRODUCT_REGISTRY_SEED: &[u8] = b"product_registry";

/// Seed for the `TraderState` PDA: `[TRADER_STATE_SEED,
/// product_program_id, trader_wallet, challenge_id.to_le_bytes()]`. One
/// record per wallet + product + challenge; a new purchase is always a new
/// record, never a reuse of an old one.
pub const TRADER_STATE_SEED: &[u8] = b"trader_state";

/// A challenge with no recorded activity for longer than this is abandoned.
/// Stated in seconds (7 days, matching MFFU's rule), not heartbeat cycles, so
/// it does not depend on where in a 5-day cycle a trader last acted. The
/// clock freezes while the product is paused (see `ProductRegistry`).
pub const INACTIVITY_LIMIT_SECS: i64 = 604_800;

/// `record_activity` calls closer together than this are ignored, so a
/// sector program can call it on every order action without write churn.
pub const ACTIVITY_THROTTLE_SECS: i64 = 86_400;

/// Upper bound on the phase-reset price table (one entry per phase).
pub const MAX_RESET_PHASES: usize = 8;

/// `ProductRegistry.pause_reason` values. Exactly two reasons are ever
/// surfaced publicly: a planned upgrade, or a reconciliation deficit.
pub const PAUSE_NONE: u8 = 0;
pub const PAUSE_PLANNED_UPGRADE: u8 = 1;
pub const PAUSE_RECONCILIATION_DEFICIT: u8 = 2;

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
pub const MAX_CHALLENGE_SIZES: usize = 32;

/// On-chain size of one Borsh-serialized `ChallengeSize` (`size: u64, cost:
/// u64`) from `setl8-shared-interfaces`. That crate defines the struct, not
/// its serialized size, so it's recomputed here from its two `u64` fields.
pub const CHALLENGE_SIZE_SPACE: usize = 8 + 8;

/// Seed for the singleton `VaultState` PDA: `[VAULT_STATE_SEED,
/// SL8_ADMIN_PUBKEY, ROV_ADMIN_PUBKEY]`.
pub const VAULT_STATE_SEED: &[u8] = b"vault_state";

/// Seed for the two payout-pool token accounts: `[POOL_SEED, vault_state,
/// mint]`. Their token authority is the `VaultState` PDA.
pub const POOL_SEED: &[u8] = b"pool";

/// Both accepted stablecoins (USDC, USDT) are 6-decimal classic-SPL mints.
/// Every amount in the vault is in these base units.
pub const TOKEN_DECIMALS: u8 = 6;

/// Basis-point denominator for `fee_split_bps` and the reset price table.
pub const BPS_DENOMINATOR: u128 = 10_000;
