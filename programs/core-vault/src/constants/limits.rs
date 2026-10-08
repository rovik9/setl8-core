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
/// accounts can't be unbounded). Not specified anywhere in the Module 1
/// brief — **flagged assumption, confirm before Module 2**: raising this
/// later requires a account-migration (realloc), so pick deliberately rather
/// than inheriting this default.
pub const MAX_CHALLENGE_SIZES: usize = 32;

/// On-chain size of one Borsh-serialized `ChallengeSize` (`size: u64, cost:
/// u64`) from `setl8-shared-interfaces`. That crate defines the struct, not
/// its serialized size, so it's recomputed here from its two `u64` fields.
pub const CHALLENGE_SIZE_SPACE: usize = 8 + 8;
