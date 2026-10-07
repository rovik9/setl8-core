use anchor_lang::prelude::*;

use crate::constants::INACTIVITY_LIMIT_SECS;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraderStatus {
    /// Live; may request payouts and record activity.
    Active,
    /// Hit `max_payout_count`. Terminal.
    Graduated,
    /// Flagged failed by the sector program. Terminal, except it can be
    /// reset once via `deposit_reset` (which creates a new record).
    Failed,
    /// Past the inactivity window. Terminal; not resettable.
    Abandoned,
}

/// One per wallet + product + challenge. A new purchase always creates a
/// fresh record; an existing record is never reused or reopened.
#[account]
pub struct TraderState {
    pub trader_wallet: Pubkey,
    pub product_program_id: Pubkey,
    pub challenge_id: u64,
    /// Tier size this challenge was bought at; used to price phase resets.
    pub account_size: u64,
    /// Payouts made so far. The vault, not the sector program, owns this.
    pub payout_count: u64,
    pub status: TraderStatus,
    pub last_activity_timestamp: i64,
    /// `ProductRegistry::paused_secs_at(now)` at the last activity, so time
    /// the product spent paused since then is not counted as idle.
    pub paused_secs_snapshot: i64,
    /// Set once a `Failed` record has been used as the base for a reset.
    pub reset_used: bool,
    pub bump: u8,
}

impl TraderState {
    pub const SPACE: usize = 8 // discriminator
        + 32 // trader_wallet
        + 32 // product_program_id
        + 8 // challenge_id
        + 8 // account_size
        + 8 // payout_count
        + 1 // status
        + 8 // last_activity_timestamp
        + 8 // paused_secs_snapshot
        + 1 // reset_used
        + 1; // bump

    /// Seconds idle as of `now`, excluding time the product spent paused.
    /// `paused_now` is `ProductRegistry::paused_secs_at(now)`.
    pub fn idle_secs(&self, now: i64, paused_now: i64) -> i64 {
        let wall = now.saturating_sub(self.last_activity_timestamp);
        let paused = paused_now.saturating_sub(self.paused_secs_snapshot);
        wall.saturating_sub(paused).max(0)
    }

    /// True once idle time strictly exceeds the inactivity limit.
    pub fn is_stale(&self, now: i64, paused_now: i64) -> bool {
        self.idle_secs(now, paused_now) > INACTIVITY_LIMIT_SECS
    }

    /// Records activity at `now`.
    pub fn touch(&mut self, now: i64, paused_now: i64) {
        self.last_activity_timestamp = now;
        self.paused_secs_snapshot = paused_now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(last: i64, snapshot: i64) -> TraderState {
        TraderState {
            trader_wallet: Pubkey::default(),
            product_program_id: Pubkey::default(),
            challenge_id: 1,
            account_size: 2_500,
            payout_count: 0,
            status: TraderStatus::Active,
            last_activity_timestamp: last,
            paused_secs_snapshot: snapshot,
            reset_used: false,
            bump: 255,
        }
    }

    #[test]
    fn exactly_at_limit_is_not_stale_one_second_over_is() {
        let s = state(1_000, 0);
        assert!(!s.is_stale(1_000 + INACTIVITY_LIMIT_SECS, 0));
        assert!(s.is_stale(1_000 + INACTIVITY_LIMIT_SECS + 1, 0));
    }

    #[test]
    fn pause_time_is_not_counted_as_idle() {
        // 8 days wall-clock, but 3 of them paused => 5 days idle, not stale.
        let day = 86_400;
        let s = state(0, 0);
        assert_eq!(s.idle_secs(8 * day, 3 * day), 5 * day);
        assert!(!s.is_stale(8 * day, 3 * day));
        // Same 8 days with no pause => stale.
        assert!(s.is_stale(8 * day, 0));
    }

    #[test]
    fn pause_before_last_activity_is_ignored_via_snapshot() {
        // Product paused 10 days total before the trader's last activity.
        let day = 86_400;
        let s = state(20 * day, 10 * day);
        // 5 days later, no new pause: idle is 5 days.
        assert_eq!(s.idle_secs(25 * day, 10 * day), 5 * day);
    }

    #[test]
    fn touch_resets_clock_and_snapshot() {
        let mut s = state(0, 0);
        s.touch(500, 40);
        assert_eq!(s.last_activity_timestamp, 500);
        assert_eq!(s.paused_secs_snapshot, 40);
        assert_eq!(s.idle_secs(500, 40), 0);
    }

    #[test]
    fn clock_going_backwards_never_yields_negative_idle() {
        let s = state(1_000, 0);
        assert_eq!(s.idle_secs(900, 0), 0);
    }
}
