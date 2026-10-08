//! Business limits and sizing: inactivity/throttle windows, pause reasons, and the
//! bounds that fix an account's on-chain size.

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

/// Upper bound on how many `ChallengeSize` tiers a single `ProductRegistry`
/// can hold, used only to size the account's fixed on-chain space (Anchor
/// accounts can't be unbounded). Raising this later requires an account
/// migration (realloc), so it is a deliberate, fixed choice.
pub const MAX_CHALLENGE_SIZES: usize = 32;

/// On-chain size of one Borsh-serialized `ChallengeSize` (`size: u64, cost:
/// u64`) from `setl8-shared-interfaces`. That crate defines the struct, not
/// its serialized size, so it's recomputed here from its two `u64` fields.
pub const CHALLENGE_SIZE_SPACE: usize = 8 + 8;

/// Minimum gap between the STARTS of two heartbeat cycles (5 days). Fixed in the
/// program: not an account field, not admin-changeable. The very first cycle may
/// start immediately.
pub const HEARTBEAT_MIN_GAP_SECS: i64 = 432_000;

/// Most claims one `settle_claims` call may process. Proven by
/// tests-rs/tests/settle_batch.rs: a full batch with a distinct key per account,
/// a ComputeBudget instruction and two signatures is 1,106 bytes (limit 1,232) and
/// uses ~125,000 CU for ordinary wallets, ~235,000 CU for wallets ground to make the
/// associated-token-account derivation expensive. Bond claims (kind 1) measure the
/// same, alone or mixed with trader claims. Callers should add a
/// `SetComputeUnitLimit` of 400,000 to a full batch; any single claim can always be
/// settled alone, whatever its wallet.
pub const MAX_SETTLE_BATCH: usize = 6;

// ---------------------------------------------------------------- bond vault
// All amounts are 6-decimal base units of USDC or USDT.

/// Smallest bond principal ($50).
pub const BOND_MIN_PRINCIPAL: u64 = 50_000_000;

/// Most principal one wallet may have open at once, summed across ALL its open
/// positions ($50K). Measured on principal, not on the deposit fee.
pub const BOND_MAX_PER_WALLET: u64 = 50_000_000_000;

/// Most principal open across ALL wallets and positions ($600K).
pub const BOND_GLOBAL_CAP: u64 = 600_000_000_000;

/// Six-month bond: 180 days, 20% interest paid only at maturity.
pub const BOND_6M_TERM_SECS: i64 = 15_552_000;
/// Hard lock of the six-month bond: half the term (90 days). Withdrawals before it fail.
pub const BOND_6M_LOCK_SECS: i64 = 7_776_000;
pub const BOND_6M_INTEREST_BPS: u16 = 2_000;

/// Nine-month bond: 270 days, 30% interest paid only at maturity.
pub const BOND_9M_TERM_SECS: i64 = 23_328_000;
/// Hard lock of the nine-month bond: half the term (135 days).
pub const BOND_9M_LOCK_SECS: i64 = 11_664_000;
pub const BOND_9M_INTEREST_BPS: u16 = 3_000;
