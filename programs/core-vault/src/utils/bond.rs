//! Bond arithmetic, kept pure (no accounts, no CPI) so it can be tested exhaustively.
//!
//! * Deposit: the depositor pays `principal + fee`, `fee = ceil(principal * 0.2%)`.
//!   The principal is split like `deposit_fee` splits a payment (`split_amount`:
//!   pool gets the rounded-down half, SL8 the exact remainder) and 100% of the fee
//!   goes to SL8 as well.
//! * Withdrawal: before the hard lock -> `BondLocked`; from the lock until
//!   maturity -> principal only; at or after maturity -> principal plus
//!   `floor(principal * interest_bps / 10_000)`. The 0.2% withdrawal fee
//!   (rounded up) is taken off the gross amount owed.

use anchor_lang::prelude::*;

use crate::constants::{BOND_FEE_BPS, BOND_POOL_SPLIT_BPS, BOND_WITHDRAWAL_FEE_BPS, BPS_DENOMINATOR};
use crate::errors::VaultError;
use crate::state::BondTerm;
use crate::utils::split_amount;

/// `ceil(amount * bps / 10_000)` in u128 math. Rounds in the protocol's favour and,
/// for `bps <= 10_000`, never exceeds `amount`.
pub fn ceil_bps(amount: u64, bps: u128) -> Result<u64> {
    let n = (amount as u128).checked_mul(bps).ok_or(VaultError::MathOverflow)?;
    let ceil = n.checked_add(BPS_DENOMINATOR - 1).ok_or(VaultError::MathOverflow)? / BPS_DENOMINATOR;
    u64::try_from(ceil).map_err(|_| error!(VaultError::MathOverflow))
}

/// What a deposit moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositPlan {
    /// The 0.2% deposit fee, on top of the principal.
    pub fee: u64,
    /// Into the same-mint payout pool.
    pub pool: u64,
    /// Into the SL8 wallet's token account: its share of the principal PLUS the whole fee.
    pub sl8: u64,
    /// What leaves the depositor: `principal + fee` = `pool + sl8`.
    pub total_debit: u64,
}

pub fn plan_deposit(principal: u64) -> Result<DepositPlan> {
    let fee = ceil_bps(principal, BOND_FEE_BPS)?;
    let (pool, sl8_principal) = split_amount(principal, BOND_POOL_SPLIT_BPS)?;
    let sl8 = sl8_principal.checked_add(fee).ok_or(VaultError::MathOverflow)?;
    let total_debit = principal.checked_add(fee).ok_or(VaultError::MathOverflow)?;
    Ok(DepositPlan { fee, pool, sl8, total_debit })
}

/// Where a position is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maturity {
    /// Before the hard lock ends: no withdrawal.
    Locked,
    /// From the lock until before maturity: principal only.
    Early,
    /// At or after maturity: principal plus interest.
    Matured,
}

/// Boundaries are inclusive on the later side: `now >= created_at + lock` is no
/// longer locked, and `now >= created_at + term` is matured.
pub fn maturity(created_at: i64, now: i64, term: BondTerm) -> Result<Maturity> {
    let unlocks_at = created_at.checked_add(term.lock_secs()).ok_or(VaultError::MathOverflow)?;
    let matures_at = created_at.checked_add(term.term_secs()).ok_or(VaultError::MathOverflow)?;
    Ok(if now >= matures_at {
        Maturity::Matured
    } else if now >= unlocks_at {
        Maturity::Early
    } else {
        Maturity::Locked
    })
}

/// What a withdrawal request is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalPlan {
    /// Principal, plus interest at maturity.
    pub gross: u64,
    /// `ceil(gross * 0.2%)`: not owed, so it stays in the pool.
    pub fee: u64,
    /// `gross - fee`: the claim.
    pub net: u64,
}

pub fn plan_withdrawal(principal: u64, interest_bps: u16, term: BondTerm, created_at: i64, now: i64) -> Result<WithdrawalPlan> {
    let gross = match maturity(created_at, now, term)? {
        Maturity::Locked => return err!(VaultError::BondLocked),
        Maturity::Early => principal,
        Maturity::Matured => {
            let interest = (principal as u128) * (interest_bps as u128) / BPS_DENOMINATOR;
            let interest = u64::try_from(interest).map_err(|_| error!(VaultError::MathOverflow))?;
            principal.checked_add(interest).ok_or(VaultError::MathOverflow)?
        }
    };
    let fee = ceil_bps(gross, BOND_WITHDRAWAL_FEE_BPS)?;
    let net = gross.checked_sub(fee).ok_or(VaultError::MathOverflow)?;
    Ok(WithdrawalPlan { gross, fee, net })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::*;

    const M: u64 = 1_000_000;

    #[test]
    fn ceil_bps_rounds_up_and_never_exceeds_the_amount() {
        for amount in 0..=2_000u64 {
            for bps in [0u128, 1, 20, 2_500, 9_999, 10_000] {
                let c = ceil_bps(amount, bps).unwrap();
                let exact = amount as u128 * bps;
                assert!(c as u128 * 10_000 >= exact, "{amount} {bps}: rounded down");
                assert!((c as u128 * 10_000) < exact + 10_000, "{amount} {bps}: more than one unit up");
                assert!(c <= amount, "fee above the amount");
            }
        }
        assert_eq!(ceil_bps(0, 20).unwrap(), 0);
        assert_eq!(ceil_bps(500, 20).unwrap(), 1, "any remainder rounds up");
        assert_eq!(ceil_bps(499, 20).unwrap(), 1);
        assert_eq!(ceil_bps(10_000, 20).unwrap(), 20, "exact stays exact");
    }

    #[test]
    fn ceil_bps_at_the_extremes() {
        assert_eq!(ceil_bps(u64::MAX, 10_000).unwrap(), u64::MAX);
        assert_eq!(ceil_bps(u64::MAX, 20).unwrap(), 36_893_488_147_419_104); // ceil(u64::MAX * 0.002)
        assert!(ceil_bps(u64::MAX, 20_000).is_err(), "above 100% overflows u64");
    }

    #[test]
    fn the_worked_deposit() {
        // $1,000 USDC: pays 1,002.000000; pool +500; SL8 +502
        let p = plan_deposit(1_000 * M).unwrap();
        assert_eq!(p, DepositPlan { fee: 2 * M, pool: 500 * M, sl8: 502 * M, total_debit: 1_002 * M });
    }

    #[test]
    fn a_deposit_never_creates_or_loses_a_unit() {
        let mut cases: Vec<u64> = vec![
            BOND_MIN_PRINCIPAL,
            BOND_MIN_PRINCIPAL + 1,
            50_000_001,
            999_999_999,
            1_000_000_001,
            BOND_MAX_PER_WALLET,
            BOND_GLOBAL_CAP,
            123_456_789,
        ];
        cases.extend((0..2_000).map(|i| 50_000_000 + i * 7_919 + i % 3));
        for principal in cases {
            let p = plan_deposit(principal).unwrap();
            assert_eq!(p.pool + p.sl8, p.total_debit, "everything the depositor pays lands somewhere: {principal}");
            assert_eq!(p.total_debit, principal + p.fee);
            assert_eq!(p.pool, principal / 2, "pool gets the rounded-down half");
            assert_eq!(p.sl8, principal - principal / 2 + p.fee);
            assert!(p.fee < principal);
            assert!(p.fee as u128 * 10_000 >= principal as u128 * 20, "fee rounded up, never down");
        }
        assert!(plan_deposit(u64::MAX).is_err(), "principal + fee overflows");
    }

    #[test]
    fn awkward_principals_round_the_fee_up() {
        assert_eq!(plan_deposit(50_000_001).unwrap().fee, 100_001); // 100_000.02 -> 100_001
        assert_eq!(plan_deposit(999_999_999).unwrap().fee, 2_000_000); // 1_999_999.998 -> 2_000_000
        assert_eq!(plan_deposit(50_000_000).unwrap().fee, 100_000); // exact
    }

    #[test]
    fn boundaries_to_the_second_for_both_terms() {
        for (term, lock, full) in [
            (BondTerm::SixMonths, BOND_6M_LOCK_SECS, BOND_6M_TERM_SECS),
            (BondTerm::NineMonths, BOND_9M_LOCK_SECS, BOND_9M_TERM_SECS),
        ] {
            let t0 = 1_700_000_000i64;
            assert_eq!(maturity(t0, t0, term).unwrap(), Maturity::Locked);
            assert_eq!(maturity(t0, t0 + lock - 1, term).unwrap(), Maturity::Locked, "a second before the lock ends");
            assert_eq!(maturity(t0, t0 + lock, term).unwrap(), Maturity::Early, "at the lock");
            assert_eq!(maturity(t0, t0 + full - 1, term).unwrap(), Maturity::Early, "a second before maturity");
            assert_eq!(maturity(t0, t0 + full, term).unwrap(), Maturity::Matured, "at maturity");
            assert_eq!(maturity(t0, t0 + full + 1, term).unwrap(), Maturity::Matured);
            assert_eq!(maturity(t0, t0 + 10 * full, term).unwrap(), Maturity::Matured);
            // the clock reading before creation is still locked
            assert_eq!(maturity(t0, t0 - 5, term).unwrap(), Maturity::Locked);
        }
        assert!(maturity(i64::MAX - 1, 0, BondTerm::SixMonths).is_err(), "created_at + term overflows");
    }

    #[test]
    fn the_terms_are_what_the_founder_set() {
        assert_eq!(BOND_6M_TERM_SECS, 180 * 86_400);
        assert_eq!(BOND_9M_TERM_SECS, 270 * 86_400);
        assert_eq!(BOND_6M_LOCK_SECS * 2, BOND_6M_TERM_SECS);
        assert_eq!(BOND_9M_LOCK_SECS * 2, BOND_9M_TERM_SECS);
        assert_eq!(BondTerm::SixMonths.interest_bps(), 2_000);
        assert_eq!(BondTerm::NineMonths.interest_bps(), 3_000);
    }

    #[test]
    fn the_worked_withdrawals() {
        let (t0, p) = (1_700_000_000i64, 1_000 * M);
        // 6-month at maturity: gross 1,200, fee 2.4, claim 1,197.6
        let w = plan_withdrawal(p, 2_000, BondTerm::SixMonths, t0, t0 + BOND_6M_TERM_SECS).unwrap();
        assert_eq!(w, WithdrawalPlan { gross: 1_200 * M, fee: 2_400_000, net: 1_197_600_000 });
        // 6-month at month 4: gross 1,000, fee 2, claim 998
        let w = plan_withdrawal(p, 2_000, BondTerm::SixMonths, t0, t0 + 120 * 86_400).unwrap();
        assert_eq!(w, WithdrawalPlan { gross: 1_000 * M, fee: 2 * M, net: 998 * M });
        // 9-month at maturity: gross 1,300, fee 2.6, claim 1,297.4
        let w = plan_withdrawal(p, 3_000, BondTerm::NineMonths, t0, t0 + BOND_9M_TERM_SECS).unwrap();
        assert_eq!(w, WithdrawalPlan { gross: 1_300 * M, fee: 2_600_000, net: 1_297_400_000 });
    }

    #[test]
    fn a_locked_bond_cannot_be_withdrawn() {
        let t0 = 1_700_000_000i64;
        for (term, bps, lock) in [(BondTerm::SixMonths, 2_000, BOND_6M_LOCK_SECS), (BondTerm::NineMonths, 3_000, BOND_9M_LOCK_SECS)] {
            assert!(plan_withdrawal(100 * M, bps, term, t0, t0 + lock - 1).is_err());
            assert!(plan_withdrawal(100 * M, bps, term, t0, t0 + lock).is_ok());
        }
    }

    #[test]
    fn withdrawals_balance_exactly_on_a_grid_and_at_the_extremes() {
        let t0 = 1_700_000_000i64;
        let mut principals: Vec<u64> = (0..300).map(|i| BOND_MIN_PRINCIPAL + i * 1_234_567 + i % 11).collect();
        principals.extend([BOND_MIN_PRINCIPAL, 50_000_001, 999_999_999, BOND_MAX_PER_WALLET]);
        for principal in principals {
            for (term, bps) in [(BondTerm::SixMonths, 2_000u16), (BondTerm::NineMonths, 3_000)] {
                let early = plan_withdrawal(principal, bps, term, t0, t0 + term.lock_secs()).unwrap();
                assert_eq!(early.gross, principal, "no interest before maturity");
                let full = plan_withdrawal(principal, bps, term, t0, t0 + term.term_secs()).unwrap();
                let interest = (principal as u128 * bps as u128 / 10_000) as u64;
                assert_eq!(full.gross, principal + interest, "interest = floor(principal * bps / 10_000), on principal only");
                for w in [early, full] {
                    assert_eq!(w.fee + w.net, w.gross, "nothing created or lost");
                    assert!(w.fee < w.gross && w.net > 0);
                    assert!(w.fee as u128 * 10_000 >= w.gross as u128 * 20, "fee rounded up");
                    assert!(w.net >= principal - principal / 400, "the claim stays within 0.25% of the gross");
                }
            }
        }
        // a huge principal overflows cleanly instead of wrapping
        assert!(plan_withdrawal(u64::MAX, 2_000, BondTerm::SixMonths, t0, t0 + BOND_6M_TERM_SECS).is_err());
    }
}
