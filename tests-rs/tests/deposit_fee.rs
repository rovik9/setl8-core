//! deposit_fee: purchase of a new challenge (creates a TraderState).
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::errors::VaultError;
use core_vault::state::TraderState;
use solana_signer::Signer;

fn setup() -> (Env, Sector, Pubkey) {
    let (e, s) = Env::registered(&Cfg::default());
    (e, s, wallet())
}

#[test]
fn creates_active_trader_state_with_every_field_set() {
    let (mut e, s, w) = setup();
    e.advance(123);
    e.deposit_tier(&s, &w, 7, TIER_B);

    let ts = e.trader(&s, &w, 7);
    assert_eq!(ts.trader_wallet, w);
    assert_eq!(ts.product_program_id, s.id);
    assert_eq!(ts.challenge_id, 7);
    assert_eq!(ts.account_size, TIER_B.0);
    assert_eq!(ts.payout_count, 0);
    assert_eq!(ts.status, TraderStatus::Active);
    assert_eq!(ts.last_activity_timestamp, T0 + 123);
    assert_eq!(ts.paused_secs_snapshot, 0);
    assert!(!ts.reset_used);
    assert_eq!(ts.bump, s.trader_bump(&w, 7).1);

    let acct = e.svm.get_account(&s.trader(&w, 7)).unwrap();
    assert_eq!(acct.owner, core_vault::ID);
    assert_eq!(acct.data.len(), TraderState::SPACE);
}

#[test]
fn snapshots_previously_banked_pause_time() {
    let (mut e, s, w) = setup();
    e.pause(&s);
    e.advance(2 * DAY);
    e.resume(&s);
    e.deposit(&s, &w, 1);
    assert_eq!(e.trader(&s, &w, 1).paused_secs_snapshot, 2 * DAY);
}

#[test]
fn every_registered_tier_is_purchasable() {
    let (mut e, s, w) = setup();
    for (i, t) in [TIER_A, TIER_B, TIER_ODD].into_iter().enumerate() {
        e.deposit_tier(&s, &w, i as u64 + 1, t);
        assert_eq!(e.trader(&s, &w, i as u64 + 1).account_size, t.0);
    }
}

#[test]
fn rejects_unregistered_tier_combinations() {
    let (mut e, s, w) = setup();
    let payer = e.payer.pubkey();
    // (size, amount) pairs that are NOT an exact registered tier
    let bad = [
        (TIER_A.0, TIER_A.1 + 1),   // right size, wrong price
        (TIER_A.0, TIER_A.1 - 1),
        (TIER_A.0, 0),
        (TIER_A.0, TIER_B.1),       // size from tier A, price from tier B
        (TIER_B.0, TIER_A.1),       // size from tier B, price from tier A
        (99_999, TIER_A.1),         // unknown size
        (0, 0),
    ];
    for (i, (size, amount)) in bad.into_iter().enumerate() {
        let id = 100 + i as u64;
        let r = e.send(deposit_fee_ix(&s, &w, id, amount, size, &payer));
        assert_vault_err(&r, VaultError::InvalidChallengeTier);
        assert!(e.trader_opt(&s, &w, id).is_none(), "failed purchase must not create a record");
    }
}

#[test]
fn duplicate_challenge_id_is_rejected_and_original_is_untouched() {
    let (mut e, s, w) = setup();
    e.deposit(&s, &w, 1);
    e.advance(DAY);
    let before = e.svm.get_account(&s.trader(&w, 1)).unwrap().data;

    // even a *different* tier under the same id must not overwrite it
    let r = e.send(deposit_fee_ix(&s, &w, 1, TIER_B.1, TIER_B.0, &e.payer.pubkey()));
    assert_already_in_use(&r);
    assert_eq!(e.svm.get_account(&s.trader(&w, 1)).unwrap().data, before);
}

#[test]
fn same_id_for_different_wallets_and_different_ids_for_one_wallet_are_independent() {
    let (mut e, s, w) = setup();
    let w2 = wallet();
    e.deposit(&s, &w, 1);
    e.deposit(&s, &w2, 1);
    e.deposit(&s, &w, 2);
    assert_ne!(s.trader(&w, 1), s.trader(&w2, 1));
    assert_eq!(e.trader(&s, &w2, 1).trader_wallet, w2);
    assert_eq!(e.trader(&s, &w, 2).challenge_id, 2);
}

#[test]
fn rejected_while_product_is_paused() {
    let (mut e, s, w) = setup();
    e.pause(&s);
    let r = e.send(deposit_fee_ix(&s, &w, 1, TIER_A.1, TIER_A.0, &e.payer.pubkey()));
    assert_vault_err(&r, VaultError::ProductNotActive);
    assert!(e.trader_opt(&s, &w, 1).is_none());
    // and works again after reactivation
    e.resume(&s);
    e.deposit(&s, &w, 1);
}

#[test]
fn rejects_a_stranger_as_sector_authority() {
    let (mut e, s, w) = setup();
    let mut ix = deposit_fee_ix(&s, &w, 1, TIER_A.1, TIER_A.0, &e.payer.pubkey());
    ix.accounts[0].pubkey = Pubkey::new_unique();
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
    assert!(e.trader_opt(&s, &w, 1).is_none());
}

#[test]
fn rejects_another_sectors_valid_authority_against_this_registry() {
    // Impersonation: sector B's genuine authority PDA must not act on sector A.
    let (mut e, a, w) = setup();
    let b = Sector::new();
    e.register(&b, &Cfg::default());
    let mut ix = deposit_fee_ix(&a, &w, 1, TIER_A.1, TIER_A.0, &e.payer.pubkey());
    ix.accounts[0].pubkey = b.authority;
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
}

#[test]
fn rejects_the_right_authority_when_it_is_not_a_signer() {
    let (mut e, s, w) = setup();
    let mut ix = deposit_fee_ix(&s, &w, 1, TIER_A.1, TIER_A.0, &e.payer.pubkey());
    ix.accounts[0].is_signer = false;
    let r = e.send(ix);
    assert_anchor_err(&r, ErrorCode::AccountNotSigner);
}

#[test]
fn rejects_an_unregistered_product() {
    let mut e = Env::new();
    let ghost = Sector::new();
    let r = e.send(deposit_fee_ix(&ghost, &wallet(), 1, TIER_A.1, TIER_A.0, &e.payer.pubkey()));
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}
