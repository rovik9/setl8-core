//! Payout-tally reconciliation: reading a sector's tally and deciding whether it
//! agrees with the vault's own books.

use anchor_lang::prelude::*;
use setl8_shared_interfaces::PayoutTally;

/// What the account at a product's tally address turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TallyState {
    /// No data, and not owned by the sector (nonexistent, or only dusted with
    /// lamports): the sector has never written a tally, which counts as `0 / 0`.
    Missing,
    /// Owned by the sector and parsed by the shared crate's own `PayoutTally::parse`.
    Valid(PayoutTally),
    /// Anything else: owned by the sector but unparseable (including empty or
    /// all-zero), or holding data but owned by somebody else. Always a mismatch.
    Invalid,
}

/// Classifies the tally account. `sector` is the registered product program id.
pub fn read_tally(info: &AccountInfo, sector: &Pubkey) -> TallyState {
    let Ok(data) = info.try_borrow_data() else {
        return TallyState::Invalid;
    };
    if info.owner == sector {
        match PayoutTally::parse(&data) {
            Ok(t) => TallyState::Valid(t),
            Err(_) => TallyState::Invalid,
        }
    } else if data.is_empty() {
        TallyState::Missing
    } else {
        TallyState::Invalid
    }
}

/// Whether the tally agrees with the vault's books: BOTH the count and the total
/// must be equal (a difference in either field, in either direction, is a
/// mismatch). A missing tally means `0 / 0`; an invalid one never matches.
pub fn tally_matches(tally: TallyState, emitted: u64, amount: u64) -> bool {
    match tally {
        TallyState::Missing => emitted == 0 && amount == 0,
        TallyState::Valid(t) => t.requested_count == emitted && t.requested_total == amount,
        TallyState::Invalid => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(count: u64, total: u64) -> TallyState {
        TallyState::Valid(PayoutTally { requested_count: count, requested_total: total })
    }

    #[test]
    fn exact_equality_on_a_small_grid() {
        // every (tally, books) pair on a grid: matches iff both fields are equal
        for tc in 0..=4u64 {
            for tt in 0..=4u64 {
                for ec in 0..=4u64 {
                    for ea in 0..=4u64 {
                        assert_eq!(tally_matches(valid(tc, tt), ec, ea), tc == ec && tt == ea, "{tc}/{tt} vs {ec}/{ea}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_missing_tally_is_zero_zero() {
        for ec in 0..=3u64 {
            for ea in 0..=3u64 {
                assert_eq!(tally_matches(TallyState::Missing, ec, ea), ec == 0 && ea == 0);
            }
        }
    }

    #[test]
    fn an_invalid_tally_never_matches() {
        for ec in 0..=3u64 {
            for ea in 0..=3u64 {
                assert!(!tally_matches(TallyState::Invalid, ec, ea));
            }
        }
        assert!(!tally_matches(TallyState::Invalid, 0, 0), "not even with zero requests");
    }

    #[test]
    fn each_field_and_each_direction_is_checked_at_the_extremes() {
        let m = u64::MAX;
        assert!(tally_matches(valid(m, m), m, m));
        assert!(!tally_matches(valid(m, m - 1), m, m));
        assert!(!tally_matches(valid(m - 1, m), m, m));
        assert!(!tally_matches(valid(0, m), 0, m - 1));
        assert!(!tally_matches(valid(1, 600), 0, 600));
        assert!(!tally_matches(valid(0, 600), 1, 600));
    }
}
