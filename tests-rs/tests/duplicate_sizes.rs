//! `register_product` and `update_product_config` refuse a `challenge_sizes` list in which two
//! entries share a `size` (`DuplicateChallengeSize`, code 6044), whether the costs are equal or not.
//!
//! A reset finds its tier by size, so a second entry of that size can only be redundant or make the
//! price ambiguous. Every refusal here also proves that it changed nothing: every account except the
//! fee payer (who really pays a failed transaction's fee) is identical afterwards.
mod common;
use common::*;
use core_vault::errors::VaultError;
use solana_signer::Signer;
use std::collections::BTreeMap;

use anchor_lang::solana_program::pubkey::Pubkey;

fn cfg(tiers: &[(u64, u64)]) -> Cfg {
    Cfg { tiers: tiers.iter().map(|&(size, cost)| ChallengeSize { size, cost }).collect(), ..Cfg::default() }
}

fn fp(e: &Env) -> BTreeMap<Pubkey, u64> {
    state_fingerprint(&e.svm, &[e.payer.pubkey()])
}

/// 32 entries, distinct sizes.
fn thirty_two() -> Vec<(u64, u64)> {
    (0..32u64).map(|i| (10_000 + i, 100 + i)).collect()
}

/// Lists with a repeated size, each with the position of the pair noted.
fn duplicates() -> Vec<(&'static str, Vec<(u64, u64)>)> {
    let mut last_pair = thirty_two();
    last_pair[31].0 = last_pair[30].0; // the very last pair of a full list, different cost
    let mut first_last = thirty_two();
    first_last[31] = (first_last[0].0, first_last[0].1); // first and last, same cost
    vec![
        ("different cost, adjacent", vec![(10, 5), (10, 6)]),
        ("same cost, adjacent", vec![(10, 5), (10, 5)]),
        ("different cost, apart", vec![(10, 5), (20, 5), (30, 5), (10, 9)]),
        ("same cost, apart", vec![(10, 5), (20, 5), (30, 5), (10, 5)]),
        ("odd one out last of three", vec![(10, 5), (10, 5), (10, 6)]),
        ("size 0 twice", vec![(0, 1), (0, 2)]),
        ("size u64::MAX twice", vec![(u64::MAX, 0), (u64::MAX, 0)]),
        ("the last pair of 32", last_pair),
        ("first and last of 32", first_last),
    ]
}

#[test]
fn the_new_error_has_the_next_code_after_the_claims_ceiling() {
    assert_eq!(u32::from(VaultError::ClaimsCeilingExceeded), 6043);
    assert_eq!(u32::from(VaultError::DuplicateChallengeSize), 6044);
}

#[test]
fn register_refuses_a_repeated_size_with_code_6044_and_creates_nothing() {
    for (name, tiers) in duplicates() {
        let mut e = Env::new();
        let s = Sector::new();
        let before = fp(&e);
        let r = e.send(register_ix(&e, &s, &cfg(&tiers)));
        assert_vault_err(&r, VaultError::DuplicateChallengeSize);
        assert_custom_code(&r, 6044, name);
        assert!(e.svm.get_account(&s.registry()).is_none(), "{name}: no registry was created");
        assert_eq!(fp(&e), before, "{name}: a refused register must change no account");
        // the product id is not burnt: the same product registers with a fixed list
        let mut fixed = tiers.clone();
        let mut seen = vec![];
        fixed.retain(|t| {
            let new = !seen.contains(&t.0);
            seen.push(t.0);
            new
        });
        e.register(&s, &cfg(&fixed));
        assert!(e.registry(&s).active, "{name}");
    }
}

#[test]
fn update_refuses_a_repeated_size_with_code_6044_and_keeps_the_old_config() {
    for (name, tiers) in duplicates() {
        let old = cfg(&[(10_000, 100), (50_000, 400)]);
        let (mut e, s) = Env::registered(&old);
        e.pause(&s); // state that update must not touch
        let reg_before = e.registry(&s);
        let before = fp(&e);
        let r = e.send(update_ix(&e, &s, &Cfg { fee_split_bps: 1, max_payout: 99, reset_bps: vec![7], ..cfg(&tiers) }));
        assert_vault_err(&r, VaultError::DuplicateChallengeSize);
        assert_custom_code(&r, 6044, name);
        assert_eq!(fp(&e), before, "{name}: a refused update must change no account");
        let after = e.registry(&s);
        assert_eq!(after.challenge_sizes, reg_before.challenge_sizes, "{name}: the old tiers stay");
        assert_eq!(after.fee_split_bps, reg_before.fee_split_bps, "{name}");
        assert_eq!(after.max_payout_count, reg_before.max_payout_count, "{name}");
        assert_eq!(after.reset_price_bps, reg_before.reset_price_bps, "{name}");
        assert!(!after.active && after.paused_since == reg_before.paused_since, "{name}");
        // and the old config still sells: a deposit of an old tier works after the refused update
        e.resume(&s);
        let w = wallet();
        e.deposit_tier(&s, &w, 1, (10_000, 100));
    }
}

#[test]
fn an_update_that_would_create_a_duplicate_out_of_a_valid_registry_is_refused() {
    // the registry is fine, the update adds a second entry of an existing size
    let (mut e, s) = Env::registered(&cfg(&[(10_000, 100), (50_000, 400)]));
    let before = fp(&e);
    for extra in [(10_000u64, 100u64), (10_000, 101), (50_000, 400), (50_000, 0)] {
        let r = e.send(update_ix(&e, &s, &cfg(&[(10_000, 100), (50_000, 400), extra])));
        assert_vault_err(&r, VaultError::DuplicateChallengeSize);
        assert_eq!(fp(&e), before, "{extra:?}");
    }
    assert_eq!(e.registry(&s).challenge_sizes.len(), 2);
}

#[test]
fn an_update_of_a_registry_that_already_holds_a_duplicate_is_refused_unless_it_removes_it() {
    // A registry written before the check existed (written directly here) can still be cured by an update,
    // but not "re-saved" with the duplicate in it.
    let (mut e, s) = Env::registered(&cfg(&[(10_000, 100), (50_000, 400)]));
    e.set_registry(&s, |r| r.challenge_sizes.push(ChallengeSize { size: 10_000, cost: 150 }));
    let r = e.send(update_ix(&e, &s, &cfg(&[(10_000, 100), (50_000, 400), (10_000, 150)])));
    assert_vault_err(&r, VaultError::DuplicateChallengeSize);
    assert_eq!(e.registry(&s).challenge_sizes.len(), 3, "the legacy registry is unchanged by the refusal");
    e.update(&s, &cfg(&[(10_000, 100), (50_000, 400)]));
    assert_eq!(e.registry(&s).challenge_sizes.len(), 2);
}

#[test]
fn thirty_two_distinct_sizes_are_accepted_by_both() {
    let tiers = thirty_two();
    let mut e = Env::new();
    let s = Sector::new();
    e.register(&s, &cfg(&tiers));
    assert_eq!(e.registry(&s).challenge_sizes.len(), 32);
    // an update to another full list of distinct sizes, in a different order and with other costs
    let mut other: Vec<(u64, u64)> = (0..32u64).map(|i| (5_000 + 31 - i, 7)).collect();
    other.rotate_left(5);
    e.update(&s, &cfg(&other));
    let got: Vec<(u64, u64)> = e.registry(&s).challenge_sizes.iter().map(|t| (t.size, t.cost)).collect();
    assert_eq!(got, other);
}

#[test]
fn distinct_sizes_with_equal_costs_and_the_extreme_sizes_are_fine() {
    let mut e = Env::new();
    let s = Sector::new();
    // the same cost on every tier, size 0, u64::MAX, and a cost-0 tier
    let tiers = [(0u64, 5u64), (1, 5), (2, 5), (u64::MAX, 5), (u64::MAX - 1, 0), (7, u64::MAX)];
    e.register(&s, &cfg(&tiers));
    assert_eq!(e.registry(&s).challenge_sizes.len(), 6);
    e.update(&s, &cfg(&tiers[..1]));
    e.update(&s, &cfg(&[]));
    assert!(e.registry(&s).challenge_sizes.is_empty());
}

#[test]
fn the_existing_checks_keep_precedence_over_the_duplicate_check() {
    // a list that is both too long AND repeats a size: still TooManyChallengeSizes (as before)
    let mut too_many: Vec<(u64, u64)> = (0..33u64).map(|i| (10_000 + i, 1)).collect();
    too_many[32].0 = too_many[0].0;
    let dup2 = [(5u64, 1u64), (5, 2)];
    // (config, expected error)
    let cases: Vec<(Cfg, VaultError)> = vec![
        (cfg(&too_many), VaultError::TooManyChallengeSizes),
        (Cfg { reset_bps: vec![1; 9], ..cfg(&dup2) }, VaultError::TooManyResetPhases),
        (Cfg { fee_split_bps: 10_001, ..cfg(&dup2) }, VaultError::InvalidFeeSplit),
        (cfg(&dup2), VaultError::DuplicateChallengeSize),
    ];
    for (c, want) in cases {
        let mut e = Env::new();
        let s = Sector::new();
        assert_vault_err(&e.send(register_ix(&e, &s, &c)), want);
        let (mut e, s) = Env::registered(&Cfg::default());
        assert_vault_err(&e.send(update_ix(&e, &s, &c)), want);
    }
}

#[test]
fn the_admin_signature_checks_come_before_the_list_check() {
    // the account constraints run before the handler: with a duplicate list AND a missing
    // signature, a signer reads the signature error, and nothing is created
    for slot in [0usize, 1] {
        let mut e = Env::new();
        let s = Sector::new();
        let mut ix = register_ix(&e, &s, &cfg(&[(5, 1), (5, 1)]));
        unsign_slot(&mut ix, slot);
        assert_anchor_err(&e.send(ix), anchor_lang::error::ErrorCode::AccountNotSigner);
        assert!(e.svm.get_account(&s.registry()).is_none());

        let (mut e, s) = Env::registered(&Cfg::default());
        let before = fp(&e);
        let mut ix = update_ix(&e, &s, &cfg(&[(5, 1), (5, 1)]));
        unsign_slot(&mut ix, slot);
        assert_anchor_err(&e.send(ix), anchor_lang::error::ErrorCode::AccountNotSigner);
        assert_eq!(fp(&e), before);
    }
}
