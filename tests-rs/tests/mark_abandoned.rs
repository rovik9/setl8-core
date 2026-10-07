//! mark_abandoned: permissionless, only for genuinely stale Active records.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::errors::VaultError;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn setup() -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    e.deposit(&s, &w, 1);
    (e, s, w)
}

#[test]
fn stale_record_is_abandoned_by_any_signer() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    // a complete stranger: not payer, not admin, not the trader, not the sector
    let caller = Keypair::new();
    e.fund(&caller.pubkey());
    assert_ok(e.abandon_as(&caller, &s, &w, 1));
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn the_trader_themselves_can_call_it() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let trader = Keypair::new();
    e.fund(&trader.pubkey());
    e.deposit(&s, &trader.pubkey(), 5);
    e.advance(8 * DAY);
    assert_ok(e.abandon_as(&trader, &s, &trader.pubkey(), 5));
    assert_eq!(e.trader(&s, &trader.pubkey(), 5).status, TraderStatus::Abandoned);
}

#[test]
fn abandoning_changes_only_the_status() {
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    let before = e.trader(&s, &w, 1);
    assert_ok(e.abandon(&s, &w, 1));
    let after = e.trader(&s, &w, 1);
    assert_eq!(after.status, TraderStatus::Abandoned);
    assert_eq!(after.last_activity_timestamp, before.last_activity_timestamp);
    assert_eq!(after.payout_count, before.payout_count);
    assert_eq!(after.account_size, before.account_size);
    assert_eq!(after.paused_secs_snapshot, before.paused_secs_snapshot);
    assert_eq!(after.reset_used, before.reset_used);
}

#[test]
fn fresh_record_is_not_abandonable() {
    let (mut e, s, w) = setup();
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    e.advance(DAY);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn exactly_at_the_limit_is_not_stale_one_second_over_is() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    e.advance(1);
    assert_ok(e.abandon(&s, &w, 1));
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn a_recent_heartbeat_resets_the_clock() {
    let (mut e, s, w) = setup();
    e.advance(5 * DAY);
    assert_activity(&e.record(&s, &w, 1), ActivityOutcome::Recorded);
    e.advance(5 * DAY); // 10 days since deposit, only 5 since activity
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
}

#[test]
fn already_abandoned_is_invalid_status() {
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    assert_ok(e.abandon(&s, &w, 1));
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn failed_records_are_never_abandoned_even_when_old() {
    let (mut e, s, w) = setup();
    e.flag(&s, &w, 1);
    e.advance(30 * DAY);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Failed);
}

#[test]
fn graduated_records_are_never_abandoned_even_when_old() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 1, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 10, 1);
    e.advance(30 * DAY);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Graduated);
}

#[test]
fn caller_must_still_sign() {
    let (mut e, s, w) = setup();
    e.advance(8 * DAY);
    // distinct from the fee payer (a fee payer is always a signer)
    let mut ix = abandon_ix(&Pubkey::new_unique(), &s, &w, 1);
    ix.accounts[0].is_signer = false;
    let r = e.send(ix);
    assert_anchor_err(&r, ErrorCode::AccountNotSigner);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn unknown_challenge_is_rejected() {
    let (mut e, s, w) = setup();
    let r = e.abandon(&s, &w, 404);
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}

// ------------------------------------------------- pause-adjusted, end to end

#[test]
fn pause_adjusted_end_to_end() {
    // Product A: deposit @ T0, paused T0+1d .. T0+4d (3 days), then 8 wall days in.
    // Product B (control, never paused) deposited at the same moment.
    let mut e = Env::new();
    let (a, b) = (Sector::new(), Sector::new());
    e.register(&a, &Cfg::default());
    e.register(&b, &Cfg::default());
    let w = wallet();
    e.deposit(&a, &w, 1);
    e.deposit(&b, &w, 1);

    e.advance(DAY);
    e.pause(&a);
    e.advance(3 * DAY);
    e.resume(&a);
    assert_eq!(e.registry(&a).total_paused_secs, 3 * DAY);
    e.advance(4 * DAY);
    assert_eq!(e.now() - T0, 8 * DAY, "8 days of wall clock");

    // A: idle = 8d - 3d paused = 5d  => NOT abandonable
    let r = e.abandon(&a, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    assert_eq!(e.trader(&a, &w, 1).status, TraderStatus::Active);

    // B (control): same 8 wall days, no pause => abandonable
    assert_ok(e.abandon(&b, &w, 1));
    assert_eq!(e.trader(&b, &w, 1).status, TraderStatus::Abandoned);

    // A, no further pause: stale only once idle (wall - 3d) > 7d, i.e. wall > 10d.
    e.set_time(T0 + 10 * DAY);
    let r = e.abandon(&a, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable); // exactly at the limit
    e.set_time(T0 + 10 * DAY + 1);
    assert_ok(e.abandon(&a, &w, 1));
    assert_eq!(e.trader(&a, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn an_ongoing_pause_freezes_the_clock_too() {
    let (mut e, s, w) = setup();
    e.advance(DAY);
    e.pause(&s);
    e.advance(30 * DAY); // still paused, not resumed
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    // resume: 30d banked; idle = 31d - 30d = 1d
    e.resume(&s);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    // idle is now 1d (31d wall - 30d banked). 6 more days => exactly 7d: not stale.
    e.advance(INACTIVITY_LIMIT_SECS - DAY);
    let r = e.abandon(&s, &w, 1);
    assert_vault_err(&r, VaultError::NotAbandonable);
    e.advance(1);
    assert_ok(e.abandon(&s, &w, 1));
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}
