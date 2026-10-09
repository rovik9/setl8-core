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

/// What a pool can pay out right now: its balance, or 0 if the issuer has FROZEN the pool
/// token account (SR-03). A frozen account cannot send tokens, so counting it would make every
/// `settle_claims` that touched it revert and wedge the cycle. Treating it as empty means
/// claims are paid from the other pool only, or carry over unpaid if both are frozen.
pub fn spendable(balance: u64, frozen: bool) -> u64 {
    if frozen {
        0
    } else {
        balance
    }
}

/// The `available` figure `begin_heartbeat` snapshots: both pools' spendable balances.
pub fn available_snapshot(usdc_balance: u64, usdc_frozen: bool, usdt_balance: u64, usdt_frozen: bool) -> Result<u64> {
    spendable(usdc_balance, usdc_frozen)
        .checked_add(spendable(usdt_balance, usdt_frozen))
        .ok_or_else(|| error!(VaultError::MathOverflow))
}

/// `plan_settlement` over the pools' LIVE state: a frozen pool counts as empty, so no leg is
/// ever planned from it (and no transfer is attempted). Both frozen -> a zero payment.
pub fn plan_settlement_with_frozen(
    owed: u64,
    num: u64,
    den: u64,
    usdc_balance: u64,
    usdc_frozen: bool,
    usdt_balance: u64,
    usdt_frozen: bool,
) -> Result<Settlement> {
    plan_settlement(owed, num, den, spendable(usdc_balance, usdc_frozen), spendable(usdt_balance, usdt_frozen))
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

    // ------------------------------------------------------------------ frozen pools (SR-03)

    #[test]
    fn a_frozen_pool_counts_as_empty_exhaustively_on_a_small_grid() {
        for owed in 0..=11u64 {
            for den in 1..=7u64 {
                for num in 0..=den {
                    for usdc in 0..=8u64 {
                        for usdt in 0..=8u64 {
                            for (fu, ft) in [(false, false), (true, false), (false, true), (true, true)] {
                                let s = plan_settlement_with_frozen(owed, num, den, usdc, fu, usdt, ft).unwrap();
                                if fu {
                                    assert_eq!(s.from_usdc, 0, "a leg came from a frozen USDC pool");
                                }
                                if ft {
                                    assert_eq!(s.from_usdt, 0, "a leg came from a frozen USDT pool");
                                }
                                // identical to planning over the spendable balances only
                                let e = plan_settlement(owed, num, den, if fu { 0 } else { usdc }, if ft { 0 } else { usdt }).unwrap();
                                assert_eq!(s, e);
                                let pay = s.total() as u128;
                                assert!(pay <= owed as u128);
                                let target = owed as u128 * num as u128 / den as u128;
                                let live = (if fu { 0 } else { usdc } as u128) + (if ft { 0 } else { usdt } as u128);
                                assert_eq!(pay, target.min(live));
                                if fu && ft {
                                    assert_eq!(pay, 0, "both frozen pays nothing and does not fail");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_snapshot_excludes_a_frozen_pool() {
        assert_eq!(available_snapshot(30, false, 20, false).unwrap(), 50);
        assert_eq!(available_snapshot(30, true, 20, false).unwrap(), 20);
        assert_eq!(available_snapshot(30, false, 20, true).unwrap(), 30);
        assert_eq!(available_snapshot(30, true, 20, true).unwrap(), 0);
        assert!(available_snapshot(u64::MAX, false, 1, false).is_err(), "still checked");
        assert_eq!(available_snapshot(u64::MAX, true, 1, false).unwrap(), 1, "a frozen pool never overflows the sum");
    }

    #[test]
    fn a_thawed_pool_is_back_in_the_arithmetic() {
        let frozen = plan_settlement_with_frozen(10, 1, 1, 6, true, 4, false).unwrap();
        assert_eq!(frozen, Settlement { from_usdc: 0, from_usdt: 4 });
        let thawed = plan_settlement_with_frozen(10, 1, 1, 6, false, 4, false).unwrap();
        assert_eq!(thawed, Settlement { from_usdc: 6, from_usdt: 4 });
    }
}
