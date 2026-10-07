//! record_activity: heartbeat / throttle / abandon-on-stale.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::errors::VaultError;

fn setup() -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    e.deposit(&s, &w, 1);
    (e, s, w)
}

#[test]
fn inside_the_throttle_window_nothing_changes() {
    let (mut e, s, w) = setup();
    e.advance(ACTIVITY_THROTTLE_SECS - 1);
    let before = e.svm.get_account(&s.trader(&w, 1)).unwrap().data;
    let m = e.record(&s, &w, 1);
    assert_activity(&m, ActivityOutcome::Throttled);
    assert_eq!(e.svm.get_account(&s.trader(&w, 1)).unwrap().data, before);
}

#[test]
fn exactly_at_the_throttle_boundary_is_recorded() {
    let (mut e, s, w) = setup();
    e.advance(ACTIVITY_THROTTLE_SECS);
    let m = e.record(&s, &w, 1);
    assert_activity(&m, ActivityOutcome::Recorded);
    let ts = e.trader(&s, &w, 1);
    assert_eq!(ts.last_activity_timestamp, T0 + ACTIVITY_THROTTLE_SECS);
    assert_eq!(ts.status, TraderStatus::Active);
}

#[test]
fn recorded_then_immediately_throttled() {
    let (mut e, s, w) = setup();
    e.advance(2 * DAY);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Recorded);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Throttled);
    assert_eq!(e.trader(&s, &w, 1).last_activity_timestamp, T0 + 2 * DAY);
}

#[test]
fn recorded_snapshots_the_current_banked_pause_time() {
    let (mut e, s, w) = setup();
    e.pause(&s);
    e.advance(DAY);
    e.resume(&s);
    e.advance(2 * DAY);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Recorded);
    assert_eq!(e.trader(&s, &w, 1).paused_secs_snapshot, DAY);
}

#[test]
fn exactly_at_seven_days_is_not_stale() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS);
    let m = e.record(&s, &w, 1);
    assert_activity(&m, ActivityOutcome::Recorded);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn one_second_past_seven_days_abandons_ok_with_return_data_and_persists() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    // must be Ok (not Err), otherwise the status write would be rolled back
    let m = e.record(&s, &w, 1);
    assert_activity(&m, ActivityOutcome::Abandoned);
    let ts = e.trader(&s, &w, 1);
    assert_eq!(ts.status, TraderStatus::Abandoned);
    assert_eq!(ts.last_activity_timestamp, T0, "abandoning must not refresh activity");
}

#[test]
fn eight_days_abandons() {
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Abandoned);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn abandoned_stays_abandoned_and_is_then_invalid_status() {
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    e.record(&s, &w, 1);
    let r = e.send(record_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn paused_time_does_not_count_as_idle() {
    // 8 wall-clock days, 3 of them paused => 5 idle days: not stale.
    let (mut e, s, w) = setup();
    e.advance(DAY);
    e.pause(&s);
    e.advance(3 * DAY);
    e.resume(&s);
    e.advance(4 * DAY);
    assert_eq!(e.now() - T0, 8 * DAY);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Recorded);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn failed_record_is_invalid_status() {
    let (mut e, s, w) = setup();
    e.flag(&s, &w, 1);
    e.advance(2 * DAY);
    let r = e.send(record_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn graduated_record_is_invalid_status() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 1, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 10, 1);
    e.advance(2 * DAY);
    let r = e.send(record_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn stranger_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    e.advance(2 * DAY);
    let mut ix = record_ix(&s, &w, 1);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
    assert_eq!(e.trader(&s, &w, 1).last_activity_timestamp, T0);
}

#[test]
fn another_sectors_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    e.advance(2 * DAY);
    let mut ix = record_ix(&s, &w, 1);
    ix.accounts[0].pubkey = Sector::new().authority;
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
}

#[test]
fn authority_that_is_not_a_signer_is_rejected() {
    let (mut e, s, w) = setup();
    e.advance(2 * DAY);
    let mut ix = record_ix(&s, &w, 1);
    ix.accounts[0].is_signer = false;
    let r = e.send(ix);
    assert_anchor_err(&r, ErrorCode::AccountNotSigner);
}

#[test]
fn unauthorized_is_checked_before_any_abandon_write() {
    // A stranger must not be able to abandon someone's challenge by calling it
    // after the window: the auth check comes first.
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    let mut ix = record_ix(&s, &w, 1);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}
