//! deposit_reset pricing: the reset price is a PERCENTAGE OF THE TIER PRICE, not of the
//! account size.
//!
//!   amount == ceil(tier.cost * reset_price_bps[reset_phase] / 10_000)   (u128, rounded up)
//!
//! where `tier` is the registered `challenge_sizes` entry whose `size` equals the failed
//! record's `account_size`, read from the registry as it is NOW. The lev-trading ladder is
//! `[10000, 11500, 12000, 12750, 13000]` (100%, 115%, 120%, 127.5%, 130%).
//!
//! Every rejection test also proves the rejection changed nothing: every account and token
//! balance except the fee payer (who really pays a failed transaction's fee) is identical.
mod common;
use common::*;
use core_vault::errors::VaultError;
use solana_signer::Signer;
use std::collections::BTreeMap;

use anchor_lang::solana_program::pubkey::Pubkey;

/// 100K account bought for $859 (USDC has 6 decimals).
const SIZE_100K: u64 = 100_000_000_000;
const COST_859: u64 = 859_000_000;
/// The required lev-ladder prices for a tier that cost 859_000_000 (every one is exact).
const LADDER_AT_859: [u64; 5] = [859_000_000, 987_850_000, 1_030_800_000, 1_095_225_000, 1_116_700_000];

fn cfg_with(tiers: &[(u64, u64)]) -> Cfg {
    Cfg {
        tiers: tiers.iter().map(|&(size, cost)| ChallengeSize { size, cost }).collect(),
        ..Cfg::default()
    }
}

/// Product registered with `tiers` and the lev ladder; `w` bought `bought` as challenge 1
/// and then failed it.
fn failed_at(tiers: &[(u64, u64)], bought: (u64, u64)) -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(&cfg_with(tiers));
    let w = wallet();
    e.deposit_tier(&s, &w, 1, bought);
    e.flag(&s, &w, 1);
    (e, s, w)
}

fn reset(e: &mut Env, s: &Sector, w: &Pubkey, amount: u64, phase: u8) -> litesvm::types::TransactionResult {
    e.fund_wallet(w);
    let ix = reset_ix(e, s, w, 1, 2, amount, phase);
    e.send(ix)
}

fn fingerprint(e: &Env) -> BTreeMap<Pubkey, u64> {
    state_fingerprint(&e.svm, &[e.payer.pubkey()])
}

/// A reset that must be refused with `want`, leaving every account and balance as it was
/// (only the fee payer is excluded: it pays the fee of the failed transaction).
fn assert_refused(e: &mut Env, s: &Sector, w: &Pubkey, amount: u64, phase: u8, want: VaultError) {
    e.fund_wallet(w);
    let before = fingerprint(e);
    let tokens = e.token_snapshot();
    let r = reset(e, s, w, amount, phase);
    assert_vault_err(&r, want);
    assert_eq!(fingerprint(e), before, "a refused reset must change no account");
    assert_snapshot_unchanged(&tokens, &e.token_snapshot());
    assert!(!e.trader(s, w, 1).reset_used, "a refused reset must not burn reset_used");
    assert!(e.trader_opt(s, w, 2).is_none(), "a refused reset must not create the new record");
}

fn split(amount: u64, split_bps: u16) -> (u64, u64) {
    let pool = (amount as u128 * split_bps as u128 / 10_000) as u64;
    (pool, amount - pool)
}

// -------------------------------------------------------------- the lev ladder

#[test]
fn the_ladder_at_the_100k_tier_is_exactly_859_to_1116_point_7_dollars() {
    // The integers, spelled out (the const above is the contract; recompute to prove it).
    assert_eq!(LADDER_AT_859, [859_000_000u64, 987_850_000, 1_030_800_000, 1_095_225_000, 1_116_700_000]);
    for (bps, want) in LEV_LADDER.iter().zip(LADDER_AT_859) {
        assert_eq!(reset_amount(COST_859, *bps), Some(want), "independent reference, bps {bps}");
    }

    for (phase, want) in LADDER_AT_859.into_iter().enumerate() {
        let cfg = cfg_with(&[(SIZE_100K, COST_859)]);
        let (mut e, s) = Env::registered(&cfg);
        let w = wallet();
        e.deposit_tier(&s, &w, 1, (SIZE_100K, COST_859));
        e.advance(DAY);
        e.payout(&s, &w, 1, 500, 1); // one payout already taken: the reset must carry it over
        e.flag(&s, &w, 1);

        // one base unit under and over are refused, nothing moves
        assert_refused(&mut e, &s, &w, want - 1, phase as u8, VaultError::WrongAmount);
        assert_refused(&mut e, &s, &w, want + 1, phase as u8, VaultError::WrongAmount);

        let snap = e.token_snapshot();
        assert_ok(reset(&mut e, &s, &w, want, phase as u8));
        let after = e.token_snapshot();

        // split = the existing fee rule: pool = floor(amount * fee_split_bps / 10_000), SL8 the rest
        let (pool, sl8) = split(want, cfg.fee_split_bps);
        let trader_ta = e.wallet_ta(&w, Coin::Usdc);
        assert_eq!(snap[&trader_ta] - after[&trader_ta], want, "phase {phase}: trader pays exactly {want}");
        assert_eq!(after[&e.usdc_pool] - snap[&e.usdc_pool], pool, "phase {phase}: pool share");
        assert_eq!(after[&e.sl8_usdc] - snap[&e.sl8_usdc], sl8, "phase {phase}: SL8 share");
        assert_eq!(pool + sl8, want);

        let new = e.trader(&s, &w, 2);
        assert_eq!(new.account_size, SIZE_100K, "phase {phase}");
        assert_eq!(new.payout_count, 1, "phase {phase}");
        assert_eq!(new.status, TraderStatus::Active);
        assert!(!new.reset_used);
        assert!(e.trader(&s, &w, 1).reset_used);
    }
}

#[test]
fn prices_are_not_proportional_to_size_each_tier_uses_its_own_cost() {
    // $30 for 2.5K and $859 for 100K in the same registry: 100K is 40x the size, 28.6x the price.
    let tiers = [(2_500u64, 30u64), (100_000, 859)];
    // ceil(30 * bps / 10_000) and ceil(859 * bps / 10_000)
    let small = [30u64, 35, 36, 39, 39];
    let big = [859u64, 988, 1_031, 1_096, 1_117];
    for phase in 0..5usize {
        assert_eq!(reset_amount(30, LEV_LADDER[phase]), Some(small[phase]));
        assert_eq!(reset_amount(859, LEV_LADDER[phase]), Some(big[phase]));

        for (bought, own, other, size_based) in [
            (tiers[0], small[phase], big[phase], 2_500 * LEV_LADDER[phase] as u64 / 10_000),
            (tiers[1], big[phase], small[phase], 100_000 * LEV_LADDER[phase] as u64 / 10_000),
        ] {
            let (mut e, s, w) = failed_at(&tiers, bought);
            // the other tier's price, and the old size-based price, are wrong
            assert_refused(&mut e, &s, &w, other, phase as u8, VaultError::WrongAmount);
            assert_refused(&mut e, &s, &w, size_based, phase as u8, VaultError::WrongAmount);
            assert_ok(reset(&mut e, &s, &w, own, phase as u8));
            assert_eq!(e.trader(&s, &w, 2).account_size, bought.0);
        }
    }
}

// ------------------------------------------------------------------- rounding

#[test]
fn the_price_rounds_up_and_one_unit_either_side_is_wrong() {
    // costs where cost * bps is mostly NOT a multiple of 10_000
    for cost in [1u64, 3, 7, 77, 333, 999_999_937] {
        let (mut e, s, w) = {
            let (mut e, s) = Env::registered(&cfg_with(&[(10_000, cost)]));
            let w = wallet();
            // five failed records of the same tier, one per phase
            for id in 1..=5u64 {
                e.deposit_tier(&s, &w, id, (10_000, cost));
                e.flag(&s, &w, id);
            }
            (e, s, w)
        };
        for (phase, bps) in LEV_LADDER.iter().enumerate() {
            let want = reset_amount(cost, *bps).unwrap();
            let floor = (cost as u128 * *bps as u128 / 10_000) as u64;
            let exact = (cost as u128 * *bps as u128) % 10_000 == 0;
            assert_eq!(want, floor + if exact { 0 } else { 1 }, "cost {cost} bps {bps}: reference is a ceiling");
            let prev = phase as u64 + 1;
            e.fund_wallet(&w);
            for bad in [want.wrapping_sub(1), want + 1] {
                let ix = reset_ix(&e, &s, &w, prev, 100 + prev, bad, phase as u8);
                assert_vault_err(&e.send(ix), VaultError::WrongAmount);
                assert!(!e.trader(&s, &w, prev).reset_used, "cost {cost} phase {phase}");
            }
            if !exact {
                // the floor is not accepted (this is where a rounding-down bug shows)
                let ix = reset_ix(&e, &s, &w, prev, 100 + prev, floor, phase as u8);
                assert_vault_err(&e.send(ix), VaultError::WrongAmount);
            }
            let ix = reset_ix(&e, &s, &w, prev, 100 + prev, want, phase as u8);
            assert_ok(e.send(ix));
            assert!(e.trader(&s, &w, prev).reset_used);
        }
    }
}

#[test]
fn a_price_that_does_not_fit_in_u64_is_wrong_amount() {
    // bought at cost 1, then the tier is repriced to u64::MAX (a 2-of-2 admin action)
    let (mut e, s, w) = failed_at(&[(10_000, 1)], (10_000, 1));
    e.update(&s, &cfg_with(&[(10_000, u64::MAX)]));
    // 130% of u64::MAX does not fit: no amount can ever be right
    for amount in [0u64, 1, u64::MAX / 2, u64::MAX] {
        assert_refused(&mut e, &s, &w, amount, 4, VaultError::WrongAmount);
    }
    // 100% of u64::MAX fits exactly: the amount check passes and the (empty) wallet then fails
    e.fund_wallet_with(&w, 5, 5);
    assert_refused(&mut e, &s, &w, u64::MAX - 1, 0, VaultError::WrongAmount);
    let before = fingerprint(&e);
    let r = reset(&mut e, &s, &w, u64::MAX, 0);
    assert_vault_err(&r, VaultError::InsufficientTokenBalance);
    assert_eq!(fingerprint(&e), before);
}

#[test]
fn a_huge_cost_is_priced_with_wide_math_and_pays_out() {
    // cost * bps = 1e18 * 13_000 = 1.3e22 overflows a u64 (1.8e19) long before the price (1.3e18) does
    let cost = 1_000_000_000_000_000_000u64;
    let (mut e, s, w) = failed_at(&[(10_000, 1)], (10_000, 1));
    e.update(&s, &cfg_with(&[(10_000, cost)]));
    let want = reset_amount(cost, 13_000).unwrap();
    assert_eq!(want, 1_300_000_000_000_000_000);
    e.fund_wallet_with(&w, u64::MAX / 4, 0);
    let before = e.token_snapshot();
    assert_ok(reset(&mut e, &s, &w, want, 4));
    let after = e.token_snapshot();
    let ta = e.wallet_ta(&w, Coin::Usdc);
    assert_eq!(before[&ta] - after[&ta], want);
}

// --------------------------------------------------------- tier lookup by size

#[test]
fn a_size_that_is_not_a_registered_tier_is_an_invalid_tier() {
    // bought at 10_000, then the registry only has other sizes
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.update(&s, &cfg_with(&[(50_000, 400), (12_345, 77)]));
    let price_of_a_neighbour = reset_amount(400, LEV_LADDER[0]).unwrap();
    for amount in [0u64, 100, price_of_a_neighbour, reset_amount(77, LEV_LADDER[0]).unwrap(), u64::MAX] {
        assert_refused(&mut e, &s, &w, amount, 0, VaultError::InvalidChallengeTier);
    }
    // an empty tier list too
    e.update(&s, &cfg_with(&[]));
    assert_refused(&mut e, &s, &w, 100, 0, VaultError::InvalidChallengeTier);
}

#[test]
fn the_tier_is_matched_by_size_never_by_cost() {
    // a tier whose COST equals the record's size, and one whose size merely sits next to it
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.update(&s, &cfg_with(&[(5, 10_000), (9_999, 100), (10_001, 100)]));
    assert_refused(&mut e, &s, &w, 100, 0, VaultError::InvalidChallengeTier);
    assert_refused(&mut e, &s, &w, 10_000, 0, VaultError::InvalidChallengeTier);
    assert_refused(&mut e, &s, &w, 5, 0, VaultError::InvalidChallengeTier);
}

#[test]
fn the_matching_tier_is_found_wherever_it_sits_in_the_registry() {
    // 32 tiers; the record's tier is the first, a middle one, and the last
    let tiers: Vec<(u64, u64)> = (0..32u64).map(|i| (10_000 + i, 100 + 7 * i)).collect();
    for idx in [0usize, 13, 31] {
        let bought = tiers[idx];
        let (mut e, s, w) = failed_at(&tiers, bought);
        let want = reset_amount(bought.1, LEV_LADDER[2]).unwrap();
        // the FIRST tier's price is wrong unless it is the one bought
        if idx != 0 {
            assert_refused(&mut e, &s, &w, reset_amount(tiers[0].1, LEV_LADDER[2]).unwrap(), 2, VaultError::WrongAmount);
        }
        assert_ok(reset(&mut e, &s, &w, want, 2));
    }
}

#[test]
fn a_tier_removed_after_purchase_blocks_the_reset_until_it_is_re_added() {
    let (mut e, s, w) = failed_at(&[(10_000, 100), (50_000, 400)], (10_000, 100));
    let want = reset_amount(100, LEV_LADDER[1]).unwrap();
    assert_eq!(want, 115);

    // 2-of-2 removes the tier the trader bought (the other tier stays)
    e.update(&s, &cfg_with(&[(50_000, 400)]));
    assert_refused(&mut e, &s, &w, want, 1, VaultError::InvalidChallengeTier);
    assert_refused(&mut e, &s, &w, 0, 1, VaultError::InvalidChallengeTier); // no size-based fallback
    assert_refused(&mut e, &s, &w, 10_000 * LEV_LADDER[1] as u64 / 10_000, 1, VaultError::InvalidChallengeTier);
    assert!(!e.trader(&s, &w, 1).reset_used);

    // re-added: the very same reset works again
    e.update(&s, &cfg_with(&[(50_000, 400), (10_000, 100)]));
    assert_ok(reset(&mut e, &s, &w, want, 1));
    assert!(e.trader(&s, &w, 1).reset_used);
    assert_eq!(e.trader(&s, &w, 2).account_size, 10_000);
}

#[test]
fn a_tier_repriced_after_purchase_makes_the_reset_follow_the_current_cost() {
    // Pinned ON PURPOSE (SR-22): the vault stores no purchase price, only the account size, so the
    // reset is priced from the registry as it is when the reset is sent. A 2-of-2 reprice therefore
    // changes the price of resets that are already pending. The procedure (DEPLOY-CHECKLIST) is to
    // never reprice or remove a tier while Failed records exist.
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    let at_purchase = reset_amount(100, LEV_LADDER[1]).unwrap(); // 115
    e.update(&s, &cfg_with(&[(10_000, 200)]));
    let now = reset_amount(200, LEV_LADDER[1]).unwrap(); // 230
    assert_eq!((at_purchase, now), (115, 230));
    assert_refused(&mut e, &s, &w, at_purchase, 1, VaultError::WrongAmount);
    assert_ok(reset(&mut e, &s, &w, now, 1));

    // and downwards: repriced cheaper, the cheaper price applies
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.update(&s, &cfg_with(&[(10_000, 40)]));
    assert_refused(&mut e, &s, &w, at_purchase, 1, VaultError::WrongAmount);
    assert_ok(reset(&mut e, &s, &w, reset_amount(40, LEV_LADDER[1]).unwrap(), 1));
}

#[test]
fn a_changed_reset_table_applies_to_pending_resets_too() {
    // same reasoning as the reprice: the ladder is read at reset time
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.update(&s, &Cfg { reset_bps: vec![10_000, 20_000], ..cfg_with(&[(10_000, 100)]) });
    assert_refused(&mut e, &s, &w, 115, 1, VaultError::WrongAmount);
    assert_ok(reset(&mut e, &s, &w, 200, 1)); // 200% is allowed: there is no upper bound
}

// ------------------------------------------------------------ duplicate sizes

#[test]
fn two_tiers_of_the_same_size_with_different_costs_are_ambiguous() {
    // Allowed to be registered (no registration-time check); the trader can even buy either one
    // (deposit_fee matches the exact (size, cost) pair). The reset has no way to know which.
    let tiers = [(10_000u64, 100u64), (50_000, 400), (10_000, 150)];
    for bought in [tiers[0], tiers[2]] {
        let (mut e, s, w) = failed_at(&tiers, bought);
        for cost in [100u64, 150, 400] {
            for phase in [0u8, 1, 4] {
                let amount = reset_amount(cost, LEV_LADDER[phase as usize]).unwrap();
                assert_refused(&mut e, &s, &w, amount, phase, VaultError::InvalidChallengeTier);
            }
        }
        // ambiguity is about the size: the other tier's records are unaffected
        let w2 = wallet();
        e.deposit_tier(&s, &w2, 1, tiers[1]);
        e.flag(&s, &w2, 1);
        e.fund_wallet(&w2);
        let ix = reset_ix(&e, &s, &w2, 1, 2, 400, 0);
        assert_ok(e.send(ix));
    }
}

#[test]
fn ambiguity_is_detected_for_every_position_of_the_odd_one_out() {
    // [A, A, B], [A, B, A], [B, A, A] and a conflict that only shows in the last entry
    let (a, b) = ((10_000u64, 100u64), (10_000u64, 101u64));
    for tiers in [vec![a, a, b], vec![a, b, a], vec![b, a, a], vec![a, (7, 7), (8, 8), (9, 9), a, b]] {
        let (mut e, s, w) = failed_at(&tiers, a);
        assert_refused(&mut e, &s, &w, reset_amount(100, LEV_LADDER[0]).unwrap(), 0, VaultError::InvalidChallengeTier);
        assert_refused(&mut e, &s, &w, reset_amount(101, LEV_LADDER[0]).unwrap(), 0, VaultError::InvalidChallengeTier);
        // fixing the registry (dropping the odd one out) fixes the reset
        e.update(&s, &cfg_with(&[(10_000, 100)]));
        assert_ok(reset(&mut e, &s, &w, 100, 0));
    }
}

#[test]
fn two_tiers_of_the_same_size_and_the_same_cost_are_not_ambiguous() {
    for tiers in [vec![(10_000u64, 100u64), (10_000, 100)], vec![(10_000, 100), (50_000, 400), (10_000, 100), (10_000, 100)]] {
        let (mut e, s, w) = failed_at(&tiers, (10_000, 100));
        assert_refused(&mut e, &s, &w, 114, 1, VaultError::WrongAmount);
        assert_ok(reset(&mut e, &s, &w, 115, 1));
        assert_eq!(e.trader(&s, &w, 2).account_size, 10_000);
    }
}

// ------------------------------------------------- phase table and free tiers

#[test]
fn a_phase_beyond_the_table_is_invalid_even_when_the_tier_is_missing_or_the_amount_is_wrong() {
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    // LEV ladder has 5 entries: 5 is the first invalid index
    for phase in [5u8, 6, 7, 255] {
        assert_refused(&mut e, &s, &w, 100, phase, VaultError::InvalidResetPhase);
    }
    // the phase check comes BEFORE the tier lookup...
    e.update(&s, &cfg_with(&[(50_000, 400)]));
    assert_refused(&mut e, &s, &w, 100, 5, VaultError::InvalidResetPhase);
    // ...which comes before the amount check
    assert_refused(&mut e, &s, &w, 100, 4, VaultError::InvalidChallengeTier);
    // the last valid index is 4
    e.update(&s, &cfg_with(&[(10_000, 100)]));
    assert_ok(reset(&mut e, &s, &w, 130, 4));
}

#[test]
fn an_empty_reset_table_offers_no_resets_at_any_phase() {
    let (mut e, s) = Env::registered(&Cfg { reset_bps: vec![], ..cfg_with(&[(10_000, 100)]) });
    let w = wallet();
    e.deposit_tier(&s, &w, 1, (10_000, 100));
    e.flag(&s, &w, 1);
    for phase in [0u8, 1, 255] {
        assert_refused(&mut e, &s, &w, 100, phase, VaultError::InvalidResetPhase);
    }
    assert_refused(&mut e, &s, &w, 0, 0, VaultError::InvalidResetPhase);
}

#[test]
fn the_earlier_checks_keep_their_order() {
    // paused product -> ProductNotActive, whatever else is wrong
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.pause(&s);
    assert_refused(&mut e, &s, &w, 1, 99, VaultError::ProductNotActive);
    e.resume(&s);
    // a record that is not resettable -> ResetNotAllowed before the phase / tier / amount
    let w2 = wallet();
    e.deposit_tier(&s, &w2, 1, (10_000, 100)); // still Active
    e.fund_wallet(&w2);
    e.update(&s, &cfg_with(&[]));
    let ix = reset_ix(&e, &s, &w2, 1, 2, 1, 99);
    assert_vault_err(&e.send(ix), VaultError::ResetNotAllowed);
}

#[test]
fn a_free_tier_gives_a_free_reset_and_only_zero_is_the_right_amount() {
    // Documented (SR-09): a tier with cost 0 is free to buy and its reset is free too: 0 * bps = 0.
    let (mut e, s, w) = failed_at(&[(10_000, 0)], (10_000, 0));
    assert_eq!(reset_amount(0, 13_000), Some(0));
    for amount in [1u64, 100, u64::MAX] {
        assert_refused(&mut e, &s, &w, amount, 4, VaultError::WrongAmount);
    }
    let before = e.token_snapshot();
    assert_ok(reset(&mut e, &s, &w, 0, 4));
    assert_eq!(e.token_snapshot(), before, "a free reset moves no tokens");
    assert_eq!(e.trader(&s, &w, 2).status, TraderStatus::Active);
    assert!(e.trader(&s, &w, 1).reset_used);
}

#[test]
fn above_100_percent_is_by_design() {
    // 115%..130% are the lev ladder; there is no upper bound at all
    let (mut e, s, w) = failed_at(&[(10_000, 100)], (10_000, 100));
    e.update(&s, &Cfg { reset_bps: vec![65_535], ..cfg_with(&[(10_000, 100)]) });
    assert_eq!(reset_amount(100, 65_535), Some(656)); // 655.35 -> 656
    assert_refused(&mut e, &s, &w, 655, 0, VaultError::WrongAmount);
    assert_ok(reset(&mut e, &s, &w, 656, 0));
}
