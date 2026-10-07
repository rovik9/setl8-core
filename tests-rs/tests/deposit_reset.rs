//! deposit_reset: one phase-priced restart from a Failed record.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::errors::VaultError;
use solana_signer::Signer;

fn price(size: u64, bps: u16) -> u64 {
    (size as u128 * bps as u128 / 10_000) as u64
}

/// Registered product + a Failed record `id 1` at `tier`.
fn failed(cfg: &Cfg, tier: (u64, u64)) -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(cfg);
    let w = wallet();
    e.deposit_tier(&s, &w, 1, tier);
    e.flag(&s, &w, 1);
    (e, s, w)
}

fn reset(e: &mut Env, s: &Sector, w: &Pubkey, prev: u64, new: u64, amount: u64, phase: u8) -> litesvm::types::TransactionResult {
    let ix = reset_ix(s, w, prev, new, amount, phase, &e.payer.pubkey());
    e.send(ix)
}

#[test]
fn price_is_account_size_times_bps_over_10000_for_every_phase_and_tier() {
    let cfg = Cfg::default();
    for tier in [TIER_A, TIER_B, TIER_ODD] {
        for (phase, bps) in cfg.reset_bps.iter().enumerate() {
            let (mut e, s, w) = failed(&cfg, tier);
            let want = price(tier.0, *bps);
            assert_ok(reset(&mut e, &s, &w, 1, 2, want, phase as u8));
            assert_eq!(e.trader(&s, &w, 2).status, TraderStatus::Active, "tier {tier:?} phase {phase}");
        }
    }
}

#[test]
fn price_table_spot_values_including_floor_rounding() {
    // tier A (10_000): 100 / 150 / 450 ; tier ODD (12_345): floor(123.45)=123, floor(185.175)=185, floor(555.525)=555
    assert_eq!([100u16, 150, 450].map(|b| price(TIER_A.0, b)), [100, 150, 450]);
    assert_eq!([100u16, 150, 450].map(|b| price(TIER_ODD.0, b)), [123, 185, 555]);
    let (mut e, s, w) = failed(&Cfg::default(), TIER_ODD);
    // one lamport above the floored price is wrong, the floor is right
    assert_vault_err(&reset(&mut e, &s, &w, 1, 2, 186, 1), VaultError::WrongAmount);
    assert_ok(reset(&mut e, &s, &w, 1, 2, 185, 1));
}

#[test]
fn wrong_amount_is_rejected_without_side_effects() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let right = price(TIER_A.0, 150);
    for bad in [right + 1, right - 1, 0, TIER_A.1, TIER_B.1, u64::MAX] {
        let r = reset(&mut e, &s, &w, 1, 2, bad, 1);
        assert_vault_err(&r, VaultError::WrongAmount);
    }
    assert!(!e.trader(&s, &w, 1).reset_used);
    assert!(e.trader_opt(&s, &w, 2).is_none());
}

#[test]
fn phase_beyond_the_table_is_invalid() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    for phase in [3u8, 4, 255] {
        let r = reset(&mut e, &s, &w, 1, 2, 100, phase);
        assert_vault_err(&r, VaultError::InvalidResetPhase);
    }
    assert!(!e.trader(&s, &w, 1).reset_used);
}

#[test]
fn product_without_a_reset_table_offers_no_resets() {
    let (mut e, s, w) = failed(&Cfg { reset_bps: vec![], ..Cfg::default() }, TIER_A);
    let r = reset(&mut e, &s, &w, 1, 2, 0, 0);
    assert_vault_err(&r, VaultError::InvalidResetPhase);
}

#[test]
fn only_failed_records_can_be_reset() {
    let amount = price(TIER_A.0, 100);

    // Active
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    e.deposit(&s, &w, 1);
    assert_vault_err(&reset(&mut e, &s, &w, 1, 2, amount, 0), VaultError::ResetNotAllowed);
    assert!(e.trader_opt(&s, &w, 2).is_none());

    // Abandoned
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    assert_ok(e.abandon(&s, &w, 1));
    assert_vault_err(&reset(&mut e, &s, &w, 1, 2, amount, 0), VaultError::ResetNotAllowed);
    assert!(e.trader_opt(&s, &w, 2).is_none());

    // Graduated
    let (mut e, s) = Env::registered(&Cfg { max_payout: 1, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 10, 1);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Graduated);
    assert_vault_err(&reset(&mut e, &s, &w, 1, 2, amount, 0), VaultError::ResetNotAllowed);
    assert!(e.trader_opt(&s, &w, 2).is_none());
}

#[test]
fn a_failed_record_can_be_reset_only_once() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let amount = price(TIER_A.0, 100);
    assert_ok(reset(&mut e, &s, &w, 1, 2, amount, 0));
    let first_new = e.svm.get_account(&s.trader(&w, 2)).unwrap().data;

    // same prev, different new id, any phase/price
    for phase in 0..3u8 {
        let amt = price(TIER_A.0, [100u16, 150, 450][phase as usize]);
        let r = reset(&mut e, &s, &w, 1, 3 + phase as u64, amt, phase);
        assert_vault_err(&r, VaultError::ResetNotAllowed);
        assert!(e.trader_opt(&s, &w, 3 + phase as u64).is_none());
    }
    assert_eq!(e.svm.get_account(&s.trader(&w, 2)).unwrap().data, first_new);
}

#[test]
fn new_challenge_id_equal_to_prev_fails() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let before = e.svm.get_account(&s.trader(&w, 1)).unwrap().data;
    let r = reset(&mut e, &s, &w, 1, 1, price(TIER_A.0, 100), 0);
    assert_already_in_use(&r);
    assert_eq!(e.svm.get_account(&s.trader(&w, 1)).unwrap().data, before, "prev must be untouched");
}

#[test]
fn new_challenge_id_that_already_exists_fails() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    e.deposit(&s, &w, 9); // an unrelated live record
    let before = e.svm.get_account(&s.trader(&w, 9)).unwrap().data;
    let r = reset(&mut e, &s, &w, 1, 9, price(TIER_A.0, 100), 0);
    assert_already_in_use(&r);
    assert_eq!(e.svm.get_account(&s.trader(&w, 9)).unwrap().data, before);
    assert!(!e.trader(&s, &w, 1).reset_used, "failed reset must not burn the single use");
}

#[test]
fn new_record_inherits_from_prev_and_prev_is_marked_used() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    // a bought-at-TIER_B challenge that already took one payout, then failed
    e.deposit_tier(&s, &w, 1, TIER_B);
    e.advance(DAY);
    e.payout(&s, &w, 1, 500, 1);
    e.flag(&s, &w, 1);
    // some banked pause time to snapshot
    e.pause(&s);
    e.advance(2 * DAY);
    e.resume(&s);
    e.advance(DAY);

    let prev_before = e.trader(&s, &w, 1);
    assert_eq!(prev_before.payout_count, 1);
    assert_ok(reset(&mut e, &s, &w, 1, 2, price(TIER_B.0, 150), 1));

    let new = e.trader(&s, &w, 2);
    assert_eq!(new.trader_wallet, w);
    assert_eq!(new.product_program_id, s.id);
    assert_eq!(new.challenge_id, 2);
    assert_eq!(new.account_size, TIER_B.0, "copied from prev, not sector-supplied");
    assert_eq!(new.payout_count, 1, "copied from prev, so a reset can't mint payouts");
    assert_eq!(new.status, TraderStatus::Active);
    assert_eq!(new.last_activity_timestamp, e.now());
    assert_eq!(new.paused_secs_snapshot, 2 * DAY);
    assert!(!new.reset_used);
    assert_eq!(new.bump, s.trader_bump(&w, 2).1);

    let prev = e.trader(&s, &w, 1);
    assert!(prev.reset_used);
    assert_eq!(prev.status, TraderStatus::Failed, "prev stays Failed");
    assert_eq!(prev.payout_count, prev_before.payout_count);
    assert_eq!(prev.account_size, prev_before.account_size);
    assert_eq!(prev.last_activity_timestamp, prev_before.last_activity_timestamp);

    // The new record continues the payout sequence where prev stopped.
    let r = e.send(payout_ix(&s, &w, 2, 10, 1));
    assert_vault_err(&r, VaultError::RequestIdMismatch);
    e.payout(&s, &w, 2, 10, 2);
    assert_eq!(e.trader(&s, &w, 2).payout_count, 2);
}

#[test]
fn rejected_while_product_is_paused() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    e.pause(&s);
    let amount = price(TIER_A.0, 100);
    assert_vault_err(&reset(&mut e, &s, &w, 1, 2, amount, 0), VaultError::ProductNotActive);
    assert!(e.trader_opt(&s, &w, 2).is_none());
    assert!(!e.trader(&s, &w, 1).reset_used);
    e.resume(&s);
    assert_ok(reset(&mut e, &s, &w, 1, 2, amount, 0));
}

#[test]
fn stranger_authority_is_unauthorized() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let mut ix = reset_ix(&s, &w, 1, 2, 100, 0, &e.payer.pubkey());
    ix.accounts[0].pubkey = Pubkey::new_unique();
    assert_vault_err(&e.send(ix), VaultError::Unauthorized);
    assert!(e.trader_opt(&s, &w, 2).is_none());
}

#[test]
fn another_sectors_authority_is_unauthorized() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let mut ix = reset_ix(&s, &w, 1, 2, 100, 0, &e.payer.pubkey());
    ix.accounts[0].pubkey = Sector::new().authority;
    assert_vault_err(&e.send(ix), VaultError::Unauthorized);
}

#[test]
fn authority_that_is_not_a_signer_is_rejected() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let mut ix = reset_ix(&s, &w, 1, 2, 100, 0, &e.payer.pubkey());
    ix.accounts[0].is_signer = false;
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
}

#[test]
fn unknown_prev_record_is_rejected() {
    let (mut e, s, w) = failed(&Cfg::default(), TIER_A);
    let r = reset(&mut e, &s, &w, 77, 2, 100, 0);
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}
