//! request_payout: books one payout against the cap (no token movement yet).
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::errors::VaultError;

fn setup_with(cfg: &Cfg) -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(cfg);
    let w = wallet();
    e.deposit(&s, &w, 1);
    (e, s, w)
}
fn setup() -> (Env, Sector, Pubkey) {
    setup_with(&Cfg::default())
}

#[test]
fn paid_path_books_the_payout_and_refreshes_activity() {
    let (mut e, s, w) = setup();
    e.pause(&s);
    e.advance(DAY);
    e.resume(&s);
    e.advance(2 * DAY);
    let m = e.payout(&s, &w, 1, 500, 1);
    assert_payout_outcome(&m, PayoutOutcome::Paid);
    let ts = e.trader(&s, &w, 1);
    assert_eq!(ts.payout_count, 1);
    assert_eq!(ts.status, TraderStatus::Active);
    assert_eq!(ts.last_activity_timestamp, e.now());
    assert_eq!(ts.paused_secs_snapshot, DAY);
    assert_eq!(e.registry(&s).total_requests_emitted, 1);
}

#[test]
fn ids_must_be_sequential_and_each_paid_call_counts() {
    let (mut e, s, w) = setup();
    for id in 1..=4u64 {
        assert_payout_outcome(&e.payout(&s, &w, 1, 10 * id, id), PayoutOutcome::Paid);
        assert_eq!(e.trader(&s, &w, 1).payout_count, id);
        assert_eq!(e.registry(&s).total_requests_emitted, id);
    }
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active, "4 of 5: not yet graduated");
}

#[test]
fn graduates_exactly_at_max_payout_count() {
    let (mut e, s, w) = setup_with(&Cfg { max_payout: 3, ..Cfg::default() });
    e.payout(&s, &w, 1, 10, 1);
    e.payout(&s, &w, 1, 10, 2);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
    let m = e.payout(&s, &w, 1, 10, 3);
    assert_payout_outcome(&m, PayoutOutcome::Paid);
    let ts = e.trader(&s, &w, 1);
    assert_eq!(ts.status, TraderStatus::Graduated);
    assert_eq!(ts.payout_count, 3);
    assert_eq!(e.registry(&s).total_requests_emitted, 3);
    // Graduated is terminal for payouts
    let r = e.send(payout_ix(&s, &w, 1, 10, 4));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
    assert_eq!(e.registry(&s).total_requests_emitted, 3);
}

#[test]
fn stale_challenge_is_abandoned_with_ok_return_data_and_pays_nothing() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    let m = e.payout(&s, &w, 1, 500, 1); // asserts Ok
    assert_payout_outcome(&m, PayoutOutcome::Abandoned);
    let ts = e.trader(&s, &w, 1);
    assert_eq!(ts.status, TraderStatus::Abandoned, "status write must persist");
    assert_eq!(ts.payout_count, 0, "no payout counted");
    assert_eq!(ts.last_activity_timestamp, T0, "abandoning is not activity");
    assert_eq!(e.registry(&s).total_requests_emitted, 0, "no request emitted");
}

#[test]
fn exactly_at_the_limit_is_still_payable() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS);
    assert_payout_outcome(&e.payout(&s, &w, 1, 500, 1), PayoutOutcome::Paid);
}

#[test]
fn staleness_is_decided_before_the_id_and_cap_checks() {
    // A stale challenge is flipped (Ok) even if the sector's request is also
    // malformed; the abandonment must not be lost to an Err rollback.
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    let m = e.payout(&s, &w, 1, 500, 99);
    assert_payout_outcome(&m, PayoutOutcome::Abandoned);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn a_payout_resets_the_idle_clock() {
    let (mut e, s, w) = setup();
    e.advance(6 * DAY);
    e.payout(&s, &w, 1, 10, 1);
    e.advance(6 * DAY); // 12d since deposit, 6d since the payout
    assert_payout_outcome(&e.payout(&s, &w, 1, 10, 2), PayoutOutcome::Paid);
}

#[test]
fn payout_cap_reached_after_the_cap_is_lowered_below_the_count() {
    // The only way to see PayoutCapReached: hitting the cap by paying flips the
    // record to Graduated first, so the cap check fires only if an admin lowers
    // `max_payout_count` under an Active record's count.
    let (mut e, s, w) = setup();
    e.payout(&s, &w, 1, 10, 1);
    e.payout(&s, &w, 1, 10, 2);
    e.update(&s, &Cfg { max_payout: 2, ..Cfg::default() });
    for bad_id in [3u64, 99] {
        let r = e.send(payout_ix(&s, &w, 1, 10, bad_id));
        assert_vault_err(&r, VaultError::PayoutCapReached); // cap is checked before the id
    }
    let ts = e.trader(&s, &w, 1);
    assert_eq!((ts.payout_count, ts.status), (2, TraderStatus::Active));
    assert_eq!(e.registry(&s).total_requests_emitted, 2);
}

#[test]
fn request_id_must_equal_count_plus_one() {
    let (mut e, s, w) = setup();
    for bad in [0u64, 2, 5, u64::MAX] {
        let r = e.send(payout_ix(&s, &w, 1, 10, bad));
        assert_vault_err(&r, VaultError::RequestIdMismatch);
    }
    assert_eq!(e.trader(&s, &w, 1).payout_count, 0);
    e.payout(&s, &w, 1, 10, 1);
    // replay (too low) and skip-ahead (too high)
    for bad in [0u64, 1, 3, 4] {
        let r = e.send(payout_ix(&s, &w, 1, 10, bad));
        assert_vault_err(&r, VaultError::RequestIdMismatch);
    }
    assert_eq!(e.trader(&s, &w, 1).payout_count, 1);
    assert_eq!(e.registry(&s).total_requests_emitted, 1, "failed calls never emit");
}

#[test]
fn zero_amount_is_rejected() {
    let (mut e, s, w) = setup();
    let r = e.send(payout_ix(&s, &w, 1, 0, 1));
    assert_vault_err(&r, VaultError::ZeroAmount);
    assert_eq!(e.trader(&s, &w, 1).payout_count, 0);
}

#[test]
fn rejected_while_product_is_paused() {
    let (mut e, s, w) = setup();
    e.pause(&s);
    let r = e.send(payout_ix(&s, &w, 1, 10, 1));
    assert_vault_err(&r, VaultError::ProductNotActive);
    e.resume(&s);
    e.payout(&s, &w, 1, 10, 1);
}

#[test]
fn failed_and_abandoned_records_are_invalid_status() {
    let (mut e, s, w) = setup();
    e.deposit(&s, &w, 2);
    e.flag(&s, &w, 1);
    assert_vault_err(&e.send(payout_ix(&s, &w, 1, 10, 1)), VaultError::InvalidTraderStatus);

    e.advance(8 * DAY);
    assert_ok(e.abandon(&s, &w, 2));
    assert_vault_err(&e.send(payout_ix(&s, &w, 2, 10, 1)), VaultError::InvalidTraderStatus);
    assert_eq!(e.registry(&s).total_requests_emitted, 0);
}

#[test]
fn total_requests_emitted_counts_only_paid_outcomes() {
    let (mut e, s, w1) = setup();
    let w2 = wallet();
    e.deposit(&s, &w2, 1);

    e.payout(&s, &w1, 1, 10, 1); // Paid => 1
    assert_eq!(e.registry(&s).total_requests_emitted, 1);
    assert_vault_err(&e.send(payout_ix(&s, &w1, 1, 10, 7)), VaultError::RequestIdMismatch); // Err => 1
    assert_vault_err(&e.send(payout_ix(&s, &w1, 1, 0, 2)), VaultError::ZeroAmount); // Err => 1
    assert_eq!(e.registry(&s).total_requests_emitted, 1);

    // w1 stays fresh via its payout; make only w2 stale by not touching it...
    e.advance(5 * DAY);
    e.payout(&s, &w1, 1, 10, 2); // Paid => 2, refreshes w1
    e.advance(3 * DAY); // w2: 8d idle (stale); w1: 3d idle
    assert_payout_outcome(&e.payout(&s, &w2, 1, 10, 1), PayoutOutcome::Abandoned); // Ok but not counted
    assert_eq!(e.registry(&s).total_requests_emitted, 2);
    e.payout(&s, &w1, 1, 10, 3); // Paid => 3
    assert_eq!(e.registry(&s).total_requests_emitted, 3);
}

#[test]
fn stranger_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    let mut ix = payout_ix(&s, &w, 1, 10, 1);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    assert_vault_err(&e.send(ix), VaultError::Unauthorized);
    assert_eq!(e.trader(&s, &w, 1).payout_count, 0);
}

#[test]
fn another_sectors_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    let mut ix = payout_ix(&s, &w, 1, 10, 1);
    ix.accounts[0].pubkey = Sector::new().authority;
    assert_vault_err(&e.send(ix), VaultError::Unauthorized);
}

#[test]
fn authority_that_is_not_a_signer_is_rejected() {
    let (mut e, s, w) = setup();
    let mut ix = payout_ix(&s, &w, 1, 10, 1);
    ix.accounts[0].is_signer = false;
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
}

#[test]
fn unknown_challenge_is_rejected() {
    let (mut e, s, w) = setup();
    let r = e.send(payout_ix(&s, &w, 404, 10, 1));
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}
