//! The marketing-withdrawal reserve: how much of a pool the admins may take.
//!
//! DOCUMENTED EXCEPTION to "no admin key on money" (see the README's security
//! model): `admin_withdraw_marketing_funds` may take up to 75% of a pool's LIVE
//! balance per call, repeatably, with no deduction for open claims, bond
//! liabilities or the current cycle.
//!
//! `reserve = max(stored_floor, ceil(live * 25%))`. The stored floor is only set at
//! `finalize_heartbeat` (0 before the first one), so the live 25% term is what stops
//! the admins taking everything before then; the stored floor wins when the balance
//! has since fallen and the floor is higher. Each pool is handled alone.

use anchor_lang::prelude::*;

use crate::constants::FLOOR_BPS;
use crate::utils::ceil_bps;

/// The amount of `live` that must stay in the pool.
pub fn reserve(live: u64, stored_floor: u64) -> Result<u64> {
    Ok(ceil_bps(live, FLOOR_BPS)?.max(stored_floor))
}

/// The most that may be withdrawn right now: `live - reserve`, or 0 if the balance
/// is already at or below the reserve.
pub fn withdrawable(live: u64, stored_floor: u64) -> Result<u64> {
    Ok(live.saturating_sub(reserve(live, stored_floor)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worked_examples() {
        let m = 1_000_000u64;
        assert_eq!(reserve(1_000 * m, 0).unwrap(), 250 * m);
        assert_eq!(withdrawable(1_000 * m, 0).unwrap(), 750 * m);
        // a stored floor equal to 25% changes nothing
        assert_eq!(withdrawable(1_000 * m, 250 * m).unwrap(), 750 * m);
        // 1,000.000001: 25% is 250.00000025, rounded UP to 250.000001
        assert_eq!(reserve(1_000_000_001, 0).unwrap(), 250_000_001);
        assert_eq!(withdrawable(1_000_000_001, 0).unwrap(), 750_000_000);
        // a higher stored floor wins (the balance fell after it was stored)
        assert_eq!(reserve(400 * m, 250 * m).unwrap(), 250 * m);
        assert_eq!(withdrawable(400 * m, 250 * m).unwrap(), 150 * m);
        // a balance below the stored floor: nothing is withdrawable
        assert_eq!(withdrawable(100 * m, 250 * m).unwrap(), 0);
        assert_eq!(withdrawable(0, 0).unwrap(), 0);
    }

    #[test]
    fn repeated_withdrawals_decay_geometrically_with_ceil_rounding() {
        let mut balance = 1_000_000_000u64;
        let mut seen = vec![];
        for _ in 0..4 {
            balance -= withdrawable(balance, 0).unwrap();
            seen.push(balance);
        }
        assert_eq!(seen, vec![250_000_000, 62_500_000, 15_625_000, 3_906_250]);
        // with rounding: 7 -> ceil(7/4) = 2 -> ceil(2/4) = 1 -> 1
        let mut b = 7u64;
        for want in [2, 1, 1] {
            b -= withdrawable(b, 0).unwrap();
            assert_eq!(b, want);
        }
    }

    fn check(live: u64, floor: u64) -> (u64, u64) {
        let r = reserve(live, floor).unwrap();
        let w = withdrawable(live, floor).unwrap();
        let ceil_quarter = ((live as u128 * 2_500 + 9_999) / 10_000) as u64;
        assert!(r >= ceil_quarter, "reserve below ceil(25%): {live} {floor}");
        assert!(r >= floor, "reserve below the stored floor: {live} {floor}");
        assert!(r == ceil_quarter || r == floor, "reserve is exactly the larger of the two");
        assert!(w <= live, "withdrawable above the balance");
        assert!(w == live.saturating_sub(r));
        assert!(w as u128 * 4 <= live as u128 * 3, "at most 75% of the live balance");
        (r, w)
    }

    #[test]
    fn invariants_hold_exhaustively_on_a_small_grid() {
        for live in 0..=200u64 {
            for floor in 0..=220u64 {
                check(live, floor);
            }
        }
    }

    #[test]
    fn withdrawable_is_monotone_in_the_balance() {
        for floor in [0u64, 1, 5, 40, 200] {
            let mut prev = 0;
            for live in 0..=2_000u64 {
                let (_, w) = check(live, floor);
                assert!(w >= prev, "withdrawable fell when the balance rose: floor {floor} live {live}");
                prev = w;
            }
        }
    }

    #[test]
    fn extremes() {
        for live in [0, 1, 3, 4, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
            for floor in [0, 1, u64::MAX / 4, u64::MAX / 2, u64::MAX] {
                check(live, floor);
            }
        }
        assert_eq!(reserve(u64::MAX, 0).unwrap(), 4_611_686_018_427_387_904, "ceil(u64::MAX / 4)");
        assert_eq!(withdrawable(u64::MAX, 0).unwrap(), u64::MAX - 4_611_686_018_427_387_904);
    }
}
