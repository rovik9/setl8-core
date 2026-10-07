//! flag_trader_failed: Active -> Failed.
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
fn active_becomes_failed_and_nothing_else_changes() {
    let (mut e, s, w) = setup();
    e.advance(DAY);
    let before = e.trader(&s, &w, 1);
    e.flag(&s, &w, 1);
    let after = e.trader(&s, &w, 1);
    assert_eq!(after.status, TraderStatus::Failed);
    assert_eq!(after.last_activity_timestamp, before.last_activity_timestamp);
    assert_eq!(after.payout_count, before.payout_count);
    assert_eq!(after.account_size, before.account_size);
    assert_eq!(after.paused_secs_snapshot, before.paused_secs_snapshot);
    assert!(!after.reset_used);
}

#[test]
fn failed_is_terminal_for_flagging() {
    let (mut e, s, w) = setup();
    e.flag(&s, &w, 1);
    let r = e.send(flag_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn abandoned_cannot_be_flagged() {
    let (mut e, s, w) = setup();
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    assert!(e.abandon(&s, &w, 1).is_ok());
    let r = e.send(flag_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Abandoned);
}

#[test]
fn graduated_cannot_be_flagged() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 1, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 10, 1);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Graduated);
    let r = e.send(flag_ix(&s, &w, 1));
    assert_vault_err(&r, VaultError::InvalidTraderStatus);
}

#[test]
fn allowed_while_product_is_paused() {
    // "a breach is a breach"
    let (mut e, s, w) = setup();
    e.pause(&s);
    e.flag(&s, &w, 1);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Failed);
}

#[test]
fn stranger_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    let mut ix = flag_ix(&s, &w, 1);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn another_sectors_authority_is_unauthorized() {
    let (mut e, s, w) = setup();
    let other = Sector::new();
    let mut ix = flag_ix(&s, &w, 1);
    ix.accounts[0].pubkey = other.authority;
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::Unauthorized);
}

#[test]
fn authority_that_is_not_a_signer_is_rejected() {
    let (mut e, s, w) = setup();
    let mut ix = flag_ix(&s, &w, 1);
    ix.accounts[0].is_signer = false;
    let r = e.send(ix);
    assert_anchor_err(&r, ErrorCode::AccountNotSigner);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
}

#[test]
fn unknown_challenge_is_rejected() {
    let (mut e, s, w) = setup();
    let r = e.send(flag_ix(&s, &w, 999));
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}
