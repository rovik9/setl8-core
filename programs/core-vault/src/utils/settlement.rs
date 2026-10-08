//! Heartbeat settlement arithmetic, kept pure (no accounts, no CPI) so it can be
//! tested exhaustively.
//!
//! One cycle settles every eligible claim with the SAME ratio `num / den`, where
//! `den` is the total owed when the cycle began and `num = min(available, owed)`
//! (available = both pools' balance then). Each claim is paid
//! `floor(owed * num / den)`, never more than the pools hold right now, from the
//! larger pool first and topped up from the other.

use anchor_lang::prelude::*;

use crate::errors::VaultError;

/// What one claim is paid from each pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub from_usdc: u64,
    pub from_usdt: u64,
}

impl Settlement {
    pub fn total(&self) -> u64 {
        // Each leg is at most its pool balance and the two sum to at most the
        // payment, which fits u64; this never overflows.
        self.from_usdc + self.from_usdt
    }
}

/// The cycle's ratio `(num, den)` from the snapshots taken when it began.
/// `num <= den` always; `den == 0` means nothing was owed.
pub fn cycle_ratio(available: u64, owed: u64) -> (u64, u64) {
    (available.min(owed), owed)
}

/// Splits one claim's payment between the pools.
///
/// * `target = floor(owed * num / den)` (u128 math; rounding dust stays owed)
/// * `pay = min(target, usdc_balance + usdt_balance)`: the LIVE balances, because
///   the pools can change during a cycle
/// * the first pool is the larger one (a tie goes to USDC); it pays
///   `min(pay, its balance)` and the other pool pays the rest
///
/// Guarantees: `pay <= owed`; each leg `<=` its pool's balance; the legs sum to
/// `pay`; nothing is created or lost.
pub fn plan_settlement(owed: u64, num: u64, den: u64, usdc_balance: u64, usdt_balance: u64) -> Result<Settlement> {
    // Nothing owed at the snapshot means nothing can be eligible.
    require!(den > 0, VaultError::ClaimNotEligible);
    require!(num <= den, VaultError::MathOverflow);

    let target = (owed as u128) * (num as u128) / (den as u128);
    let live_total = (usdc_balance as u128) + (usdt_balance as u128);
    let pay = u64::try_from(target.min(live_total)).map_err(|_| error!(VaultError::MathOverflow))?;

    let usdc_first = usdc_balance >= usdt_balance;
    let first_balance = if usdc_first { usdc_balance } else { usdt_balance };
    let from_first = pay.min(first_balance);
    let from_second = pay - from_first;

    Ok(if usdc_first {
        Settlement { from_usdc: from_first, from_usdt: from_second }
    } else {
        Settlement { from_usdc: from_second, from_usdt: from_first }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every invariant that must hold for any input.
    fn check(owed: u64, num: u64, den: u64, usdc: u64, usdt: u64) -> Settlement {
        let s = plan_settlement(owed, num, den, usdc, usdt).unwrap();
        let pay = s.from_usdc as u128 + s.from_usdt as u128;
        let target = owed as u128 * num as u128 / den as u128;
        assert!(pay <= owed as u128, "paid more than owed: {owed} {num}/{den} {usdc} {usdt}");
        assert!(s.from_usdc <= usdc, "USDC leg above the pool");
        assert!(s.from_usdt <= usdt, "USDT leg above the pool");
        assert_eq!(pay, target.min(usdc as u128 + usdt as u128), "pay = min(target, live total)");
        assert_eq!(s.total() as u128, pay);
        // the larger pool is drained first: the smaller one is touched only when the larger is empty
        if usdc >= usdt {
            if s.from_usdt > 0 {
                assert_eq!(s.from_usdc, usdc, "USDT used before the larger USDC pool was emptied");
            }
        } else if s.from_usdc > 0 {
            assert_eq!(s.from_usdt, usdt, "USDC used before the larger USDT pool was emptied");
        }
        s
    }

    #[test]
    fn invariants_hold_exhaustively_on_a_small_grid() {
        for owed in 0..=13u64 {
            for den in 1..=9u64 {
                for num in 0..=den {
                    for usdc in 0..=10u64 {
                        for usdt in 0..=10u64 {
                            check(owed, num, den, usdc, usdt);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invariants_hold_on_extreme_and_pseudo_random_values() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let edges = [0, 1, 2, 3, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        for _ in 0..20_000 {
            let pick = |r: u64, n: &mut dyn FnMut() -> u64| if r % 4 == 0 { edges[(n() % 7) as usize] } else { n() >> (n() % 64) };
            let owed = pick(next(), &mut next);
            let den = pick(next(), &mut next).max(1);
            let num = if den == 0 { 0 } else { pick(next(), &mut next) % (den.saturating_add(1)).max(1) };
            let (usdc, usdt) = (pick(next(), &mut next), pick(next(), &mut next));
            check(owed, num.min(den), den, usdc, usdt);
        }
    }

    #[test]
    fn the_founders_worked_example() {
        // pools USDC 3000 / USDT 1000, ratio 4000/8000
        let d = 1_000_000u64;
        let (num, den) = cycle_ratio(4_000 * d, 8_000 * d);
        assert_eq!((num, den), (4_000 * d, 8_000 * d));
        let alice = check(3_000 * d, num, den, 3_000 * d, 1_000 * d);
        assert_eq!(alice, Settlement { from_usdc: 1_500 * d, from_usdt: 0 });
        let bob = check(3_400 * d, num, den, 1_500 * d, 1_000 * d);
        assert_eq!(bob, Settlement { from_usdc: 1_500 * d, from_usdt: 200 * d });
    }

    #[test]
    fn a_tie_pays_from_usdc_first() {
        assert_eq!(check(10, 1, 1, 5, 5), Settlement { from_usdc: 5, from_usdt: 5 });
        assert_eq!(check(4, 1, 1, 5, 5), Settlement { from_usdc: 4, from_usdt: 0 });
    }

    #[test]
    fn the_larger_pool_is_first_and_the_smaller_tops_up() {
        assert_eq!(check(10, 1, 1, 3, 9), Settlement { from_usdc: 1, from_usdt: 9 });
        assert_eq!(check(8, 1, 1, 3, 9), Settlement { from_usdc: 0, from_usdt: 8 });
    }

    #[test]
    fn rounding_dust_stays_owed() {
        // owed 7 at ratio 1/3: floor(7/3) = 2, the other 5 stays owed
        assert_eq!(check(7, 1, 3, 100, 0).total(), 2);
        // ratio 2/3: floor(14/3) = 4
        assert_eq!(check(7, 2, 3, 100, 0).total(), 4);
    }

    #[test]
    fn ratio_one_pays_in_full_and_ratio_zero_pays_nothing() {
        assert_eq!(check(123, 5, 5, 1_000, 0).total(), 123);
        assert_eq!(check(123, 0, 5, 1_000, 1_000).total(), 0);
    }

    #[test]
    fn the_live_balance_caps_the_payment() {
        assert_eq!(check(100, 1, 1, 30, 20).total(), 50);
        assert_eq!(check(100, 1, 1, 0, 0).total(), 0);
    }

    #[test]
    fn cycle_ratio_never_exceeds_one() {
        assert_eq!(cycle_ratio(5, 10), (5, 10));
        assert_eq!(cycle_ratio(50, 10), (10, 10));
        assert_eq!(cycle_ratio(0, 0), (0, 0));
        assert!(plan_settlement(1, 0, 0, 1, 1).is_err(), "den == 0 pays nothing");
    }

    #[test]
    fn huge_amounts_do_not_overflow() {
        let s = check(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(s.total(), u64::MAX);
        assert_eq!(s.from_usdc, u64::MAX); // tie -> USDC first
    }
}
