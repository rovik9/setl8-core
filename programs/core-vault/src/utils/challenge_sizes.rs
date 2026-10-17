//! Validation of a product's `challenge_sizes` list.

use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

use crate::errors::VaultError;

/// Refuses a list in which two entries have the same `size`, whatever their costs
/// (`DuplicateChallengeSize`). A phase reset is priced from the one tier whose `size`
/// equals the failed record's `account_size`, so a second entry of that size can only be
/// redundant (same cost) or make the price ambiguous (different cost).
///
/// O(n^2) over at most `MAX_CHALLENGE_SIZES` (32) entries: 496 comparisons at most. The
/// caller has already checked the length limit.
pub fn require_distinct_sizes(tiers: &[ChallengeSize]) -> Result<()> {
    for (i, a) in tiers.iter().enumerate() {
        for b in &tiers[i + 1..] {
            require!(a.size != b.size, VaultError::DuplicateChallengeSize);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(size: u64, cost: u64) -> ChallengeSize {
        ChallengeSize { size, cost }
    }

    fn refused(tiers: &[ChallengeSize]) -> bool {
        match require_distinct_sizes(tiers) {
            Ok(()) => false,
            Err(Error::AnchorError(e)) => {
                assert_eq!(e.error_code_number, 6000 + VaultError::DuplicateChallengeSize as u32);
                assert_eq!(e.error_code_number, 6044);
                true
            }
            Err(other) => panic!("wrong error {other:?}"),
        }
    }

    #[test]
    fn a_repeated_size_is_refused_whatever_the_cost_and_wherever_it_sits() {
        assert!(refused(&[t(5, 1), t(5, 2)]));
        assert!(refused(&[t(5, 1), t(5, 1)]));
        assert!(refused(&[t(1, 1), t(2, 1), t(1, 9)]));
        assert!(refused(&[t(1, 1), t(2, 1), t(3, 1), t(3, 1)]), "the last pair");
        assert!(refused(&[t(7, 1), t(2, 1), t(3, 1), t(7, 1)]), "first and last");
        assert!(refused(&[t(0, 1), t(0, 1)]));
        assert!(refused(&[t(u64::MAX, 0), t(u64::MAX, 0)]));
    }

    #[test]
    fn distinct_sizes_pass_even_with_equal_costs() {
        assert!(!refused(&[]));
        assert!(!refused(&[t(5, 1)]));
        assert!(!refused(&[t(5, 1), t(6, 1), t(0, 1), t(u64::MAX, 1)]));
        assert!(!refused(&[t(1, 7), t(2, 7), t(3, 7)]), "same cost, different sizes");
        let max: Vec<ChallengeSize> = (0..32).map(|i| t(1_000 + i, 5)).collect();
        assert!(!refused(&max));
        let mut last_pair = max.clone();
        last_pair[31].size = last_pair[30].size;
        assert!(refused(&last_pair), "only the very last pair of 32 collides");
        let mut first_last = max;
        first_last[31].size = first_last[0].size;
        assert!(refused(&first_last));
    }
}
