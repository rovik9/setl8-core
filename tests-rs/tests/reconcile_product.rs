//! reconcile_product: a permissionless tally check that auto-pauses a mismatching product.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::constants::{HEARTBEAT_MIN_GAP_SECS as GAP, PAUSE_PLANNED_UPGRADE, PAUSE_RECONCILIATION_DEFICIT};
use core_vault::errors::VaultError;
use solana_keypair::Keypair;
use setl8_shared_interfaces as si;
use solana_signer::Signer;

/// Registered product; trader `w` (challenge 1) with THREE accepted requests of
/// 100, 200 and 300: the vault's books read 3 / 600.
fn rig() -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    for (req, amount) in [(1u64, 100u64), (2, 200), (3, 300)] {
        e.payout(&s, &w, 1, amount, req);
    }
    let r = e.registry(&s);
    assert_eq!((r.total_requests_emitted, r.total_requested_amount), (3, 600));
    (e, s, w)
}

fn assert_active(e: &Env, s: &Sector) {
    let r = e.registry(s);
    assert!(r.active);
    assert_eq!((r.pause_reason, r.paused_since), (0, 0));
}

fn assert_paused_by_reconciliation(e: &Env, s: &Sector, at: i64) {
    let r = e.registry(s);
    assert!(!r.active, "must be paused");
    assert_eq!(r.pause_reason, PAUSE_RECONCILIATION_DEFICIT);
    assert_eq!(r.paused_since, at);
}

// ------------------------------------------------------------- worked examples

#[test]
fn a_matching_tally_changes_nothing() {
    let (mut e, s, _) = rig();
    e.set_tally(&s, 3, 600);
    let before = e.svm.get_account(&s.registry()).unwrap().data;
    e.reconcile(&s);
    assert_active(&e, &s);
    assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before, "no state change at all");
}

#[test]
fn any_difference_in_either_field_in_either_direction_pauses() {
    for (count, total) in [(3u64, 599u64), (3, 601), (2, 600), (4, 600), (2, 599), (4, 601), (2, 601), (4, 599), (0, 0), (3, 0), (0, 600)] {
        let (mut e, s, _) = rig();
        e.set_tally(&s, count, total);
        e.advance(1234);
        let m = e.reconcile(&s); // Ok: the pause persists
        assert_paused_by_reconciliation(&e, &s, e.now());
        let logs = m.logs.join("\n");
        assert!(logs.contains("MISMATCH"), "{logs}");
        assert!(
            logs.contains(&format!("tally_count={count} tally_total={total} vault_count=3 vault_total=600")),
            "the four numbers are logged: {logs}"
        );
    }
}

#[test]
fn zero_requests_and_no_tally_is_fine() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.reconcile(&s);
    assert_active(&e, &s);
}

#[test]
fn requests_with_no_tally_account_pause() {
    let (mut e, s, _) = rig();
    assert!(e.svm.get_account(&tally_addr(&s)).is_none());
    e.reconcile(&s);
    assert_paused_by_reconciliation(&e, &s, T0);
}

#[test]
fn a_dusted_tally_address_counts_as_missing() {
    // zero requests: still matches 0/0
    let (mut e, s) = Env::registered(&Cfg::default());
    e.dust_tally(&s, 5_000_000);
    e.reconcile(&s);
    assert_active(&e, &s);
    // with requests: a mismatch, exactly like a missing account
    let (mut e, s, _) = rig();
    e.dust_tally(&s, 5_000_000);
    e.reconcile(&s);
    assert_paused_by_reconciliation(&e, &s, T0);
}

#[test]
fn an_unparseable_tally_owned_by_the_sector_pauses_even_with_zero_requests() {
    let valid = {
        let mut d = vec![0u8; 25];
        si::PayoutTally { requested_count: 0, requested_total: 0 }.write_into(&mut d).unwrap();
        d
    };
    let mut wrong_magic = valid.clone();
    wrong_magic[0] ^= 1;
    let mut wrong_version = valid.clone();
    wrong_version[8] = 2;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("all zero 25 bytes", vec![0u8; 25]),
        ("wrong magic", wrong_magic),
        ("wrong version", wrong_version),
        ("24 bytes", valid[..24].to_vec()),
        ("empty but sector-owned", vec![]),
    ];
    for (label, data) in cases {
        let (mut e, s) = Env::registered(&Cfg::default()); // zero requests
        let owner = s.id;
        e.set_tally_raw(&s, data, owner);
        if label == "empty but sector-owned" {
            // an owned empty account can exist in the SVM only as a raw account
            e.svm
                .set_account(
                    tally_addr(&s),
                    solana_account::Account { lamports: 1_000_000, data: vec![], owner: s.id, executable: false, rent_epoch: 0 },
                )
                .unwrap();
        }
        e.reconcile(&s);
        let r = e.registry(&s);
        assert!(!r.active, "{label}: must pause");
        assert_eq!(r.pause_reason, PAUSE_RECONCILIATION_DEFICIT, "{label}");
    }
}

#[test]
fn a_longer_tally_with_matching_numbers_is_fine() {
    let (mut e, s, _) = rig();
    let mut data = vec![0u8; 25 + 40];
    si::PayoutTally { requested_count: 3, requested_total: 600 }.write_into(&mut data).unwrap();
    data[25..].fill(0xAB); // the sector's own data
    let owner = s.id;
    e.set_tally_raw(&s, data, owner);
    e.reconcile(&s);
    assert_active(&e, &s);
}

#[test]
fn data_owned_by_someone_other_than_the_sector_is_a_mismatch_even_if_it_looks_right() {
    for owner in [core_vault::ID, anchor_lang::system_program::ID, Pubkey::new_unique(), anchor_spl::token::spl_token::ID] {
        let (mut e, s, _) = rig();
        let mut data = vec![0u8; 25];
        si::PayoutTally { requested_count: 3, requested_total: 600 }.write_into(&mut data).unwrap();
        e.set_tally_raw(&s, data, owner);
        e.reconcile(&s);
        assert_paused_by_reconciliation(&e, &s, T0);
    }
}

#[test]
fn foreign_owned_data_is_a_mismatch_even_when_the_books_are_empty() {
    // With zero requests a MISSING tally matches (0/0); data owned by someone
    // else must still be a mismatch, not be mistaken for "missing".
    for owner in [core_vault::ID, anchor_lang::system_program::ID, Pubkey::new_unique()] {
        let (mut e, s) = Env::registered(&Cfg::default());
        let mut data = vec![0u8; 25];
        si::PayoutTally { requested_count: 0, requested_total: 0 }.write_into(&mut data).unwrap();
        e.set_tally_raw(&s, data, owner);
        e.reconcile(&s);
        assert_paused_by_reconciliation(&e, &s, T0);
    }
}

// -------------------------------------------------------------- the address

#[test]
fn a_wrong_tally_address_is_a_hard_error_and_changes_nothing() {
    let (mut e, s, _) = rig();
    let other = Sector::new();
    e.set_tally(&s, 0, 0); // would be a mismatch if it were read
    e.set_tally(&other, 3, 600);
    for bad in [tally_addr(&other), Pubkey::new_unique(), e.vault, s.registry(), s.id] {
        let mut ix = reconcile_ix(&e.payer.pubkey(), &s);
        ix.accounts[2].pubkey = bad;
        let before = e.svm.get_account(&s.registry()).unwrap().data;
        assert_vault_err(&e.send(ix), VaultError::InvalidTally);
        assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before);
        assert_active(&e, &s);
    }
}

#[test]
fn another_products_perfectly_matching_tally_cannot_save_this_one() {
    let (mut e, s, _) = rig();
    let other = Sector::new();
    e.register(&other, &Cfg::default());
    e.set_tally(&other, 3, 600);
    // s has no tally: it must pause on its own evidence
    e.reconcile(&s);
    assert_paused_by_reconciliation(&e, &s, T0);
}

#[test]
fn the_registry_must_be_the_one_named_by_the_argument() {
    let (mut e, s, _) = rig();
    let other = Sector::new();
    e.register(&other, &Cfg::default());
    let mut ix = reconcile_ix(&e.payer.pubkey(), &s);
    ix.accounts[1].pubkey = other.registry(); // registry of another product, args of s
    assert_anchor_err(&e.send(ix), ErrorCode::ConstraintSeeds);
}

// ------------------------------------------------------------ already paused

#[test]
fn a_paused_product_is_not_re_paused_or_re_reasoned() {
    let (mut e, s, _) = rig();
    e.advance(100);
    e.pause(&s); // planned upgrade
    e.advance(50);
    let before = e.svm.get_account(&s.registry()).unwrap().data;
    // mismatch present, but the product is already paused
    e.set_tally(&s, 0, 0);
    assert_vault_err(&e.reconcile_result(&s), VaultError::ProductAlreadyPaused);
    assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before);
    let r = e.registry(&s);
    assert_eq!((r.pause_reason, r.paused_since), (PAUSE_PLANNED_UPGRADE, T0 + 100));
    // the active check comes before the address check
    let mut ix = reconcile_ix(&e.payer.pubkey(), &s);
    ix.accounts[2].pubkey = Pubkey::new_unique();
    assert_vault_err(&e.send(ix), VaultError::ProductAlreadyPaused);
}

#[test]
fn an_auto_paused_product_cannot_be_paused_again() {
    let (mut e, s, _) = rig();
    e.reconcile(&s);
    e.advance(10);
    assert_vault_err(&e.reconcile_result(&s), VaultError::ProductAlreadyPaused);
    assert_eq!(e.registry(&s).paused_since, T0, "the original pause time stays");
}

// -------------------------------------------------------- who can call it

#[test]
fn any_fresh_keypair_can_call_it() {
    let (mut e, s, _) = rig();
    let stranger = Keypair::new(); // never funded: only signs
    let ix = reconcile_ix(&stranger.pubkey(), &s);
    let fee_payer = dup(&e.payer);
    assert_ok(e.send_with(&[ix], &fee_payer, &[&stranger]));
    assert_paused_by_reconciliation(&e, &s, T0);
}

#[test]
fn the_caller_must_sign() {
    let (mut e, s, _) = rig();
    let mut ix = reconcile_ix(&Pubkey::new_unique(), &s);
    ix.accounts[0].is_signer = false;
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
    assert_active(&e, &s);
}

// ------------------------------------------------------- registry counters

#[test]
fn the_amount_counter_grows_only_for_accepted_requests() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 3, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    let counters = |e: &Env| {
        let r = e.registry(&s);
        (r.total_requests_emitted, r.total_requested_amount)
    };
    assert_eq!(counters(&e), (0, 0), "register_product initialises both to zero");

    // rejected requests change neither counter
    for (amount, req, want) in [(0u64, 1u64, VaultError::ZeroAmount), (50, 7, VaultError::RequestIdMismatch)] {
        let ix = payout_ix(&e, &s, &w, 1, amount, req);
        assert_vault_err(&e.send(ix), want);
        assert_eq!(counters(&e), (0, 0));
    }
    e.pause(&s);
    let ix = payout_ix(&e, &s, &w, 1, 50, 1);
    assert_vault_err(&e.send(ix), VaultError::ProductNotActive);
    e.resume(&s);
    assert_eq!(counters(&e), (0, 0));

    e.payout(&s, &w, 1, 100, 1);
    assert_eq!(counters(&e), (1, 100));
    e.payout(&s, &w, 1, 250, 2);
    assert_eq!(counters(&e), (2, 350));

    // the final accepted request graduates the record; then the cap rejects more
    e.payout(&s, &w, 1, 7, 3);
    assert_eq!(counters(&e), (3, 357));
    let ix = payout_ix(&e, &s, &w, 1, 9, 4);
    assert_vault_err(&e.send(ix), VaultError::InvalidTraderStatus);
    assert_eq!(counters(&e), (3, 357));
}

#[test]
fn the_stale_path_leaves_both_counters_untouched() {
    let (mut e, s, w) = rig();
    let w2 = wallet();
    e.deposit(&s, &w2, 1); // fresh, will go stale; w stays recent
    e.advance(5 * DAY);
    e.payout(&s, &w, 1, 10, 4); // w stays active: (4, 610)
    e.advance(3 * DAY); // w2 is now 8 days idle
    assert_eq!((e.registry(&s).total_requests_emitted, e.registry(&s).total_requested_amount), (4, 610));
    let m = e.payout(&s, &w2, 1, 999, 1);
    assert_payout_outcome(&m, PayoutOutcome::Abandoned);
    let r = e.registry(&s);
    assert_eq!((r.total_requests_emitted, r.total_requested_amount), (4, 610), "Abandoned is not counted");
}

#[test]
fn overflowing_the_amount_counter_fails_cleanly() {
    let (mut e, s, w) = rig();
    e.set_registry(&s, |r| r.total_requested_amount = u64::MAX - 5);
    let ix = payout_ix(&e, &s, &w, 1, 6, 4); // (registers the candidate claim address first)
    let before = (e.svm.get_account(&s.registry()).unwrap().data, e.digest());
    assert_vault_err(&e.send(ix), VaultError::MathOverflow);
    assert!(e.svm.get_account(&s.registry()).unwrap().data == before.0 && e.digest() == before.1, "no state change");
    // exactly reaching the maximum is fine
    e.payout(&s, &w, 1, 5, 4);
    assert_eq!(e.registry(&s).total_requested_amount, u64::MAX);
}

#[test]
fn a_zero_amount_request_is_rejected_and_never_counted() {
    let (mut e, s, w) = rig();
    let ix = payout_ix(&e, &s, &w, 1, 0, 4);
    assert_vault_err(&e.send(ix), VaultError::ZeroAmount);
    assert_eq!(e.registry(&s).total_requested_amount, 600);
}

// ------------------------------------------------------- effects of the pause

#[test]
fn after_an_auto_pause_fees_resets_and_payouts_are_refused() {
    let (mut e, s, w) = rig();
    let f = wallet();
    e.deposit(&s, &f, 1);
    e.flag(&s, &f, 1); // a Failed record that could otherwise be reset
    e.reconcile(&s); // no tally: pauses
    assert_paused_by_reconciliation(&e, &s, T0);

    let buyer = wallet();
    e.fund_wallet(&buyer);
    let ix = deposit_fee_ix(&e, &s, &buyer, 5, TIER_A.1, TIER_A.0);
    assert_vault_err(&e.send(ix), VaultError::ProductNotActive);
    e.fund_wallet(&f);
    let ix = reset_ix(&e, &s, &f, 1, 2, 100, 0);
    assert_vault_err(&e.send(ix), VaultError::ProductNotActive);
    let ix = payout_ix(&e, &s, &w, 1, 10, 4);
    assert_vault_err(&e.send(ix), VaultError::ProductNotActive);
    let r = e.registry(&s);
    assert_eq!((r.total_requests_emitted, r.total_requested_amount), (3, 600));
}

#[test]
fn queued_claims_of_a_paused_product_still_settle_in_a_full_heartbeat() {
    let (mut e, s, w) = rig();
    e.make_atas(&w);
    e.reconcile(&s);
    assert!(!e.registry(&s).active);

    e.set_pool(Coin::Usdc, 300);
    e.set_pool(Coin::Usdt, 0);
    e.begin(); // owed 600, available 300: ratio 1/2
    let ts = [triple(&e, &s, &w, 1, 1), triple(&e, &s, &w, 1, 2), triple(&e, &s, &w, 1, 3)];
    e.settle(&ts);
    assert_eq!(e.token_balance(&ata(&w, &e.usdc.clone())), 50 + 100 + 150);
    assert_eq!(e.pools(), (0, 0));
    e.finalize();
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 50);
    assert_eq!(e.claim(&s, &w, 1, 3).owed, 150);
    e.assert_claim_invariant();
    assert!(!e.registry(&s).active, "the heartbeat never un-pauses");
}

#[test]
fn the_pause_freezes_the_inactivity_clock() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.advance(6 * DAY);
    e.set_tally(&s, 1, 5); // vault books say 0/0
    e.reconcile(&s);
    assert_paused_by_reconciliation(&e, &s, T0 + 6 * DAY);

    e.advance(3 * DAY); // 9 days since the last activity, 3 of them paused => 6 effective
    assert_eq!(e.registry(&s).paused_secs_at(e.now()), 3 * DAY);
    assert_vault_err(&e.abandon(&s, &w, 1), VaultError::NotAbandonable);
    e.resume(&s); // banks the 3 paused days
    assert_vault_err(&e.abandon(&s, &w, 1), VaultError::NotAbandonable);
    e.advance(DAY + 1); // 10 days + 1s elapsed, 3 paused => 7 days + 1s idle
    assert_ok(e.abandon(&s, &w, 1));
}

#[test]
fn other_products_are_unaffected() {
    let (mut e, a, wa) = rig();
    let b = Sector::new();
    e.register(&b, &Cfg::default());
    let wb = wallet();
    e.deposit(&b, &wb, 1);
    e.payout(&b, &wb, 1, 40, 1);
    e.set_tally(&b, 1, 40); // B is consistent
    e.reconcile(&a); // A has no tally: pauses
    e.reconcile(&b);
    assert!(!e.registry(&a).active);
    assert_active(&e, &b);

    // B keeps taking fees and payouts; A refuses
    let buyer = wallet();
    e.deposit(&b, &buyer, 7);
    e.payout(&b, &wb, 1, 60, 2);
    assert_eq!(e.registry(&b).total_requested_amount, 100);
    let ix = payout_ix(&e, &a, &wa, 1, 1, 4);
    assert_vault_err(&e.send(ix), VaultError::ProductNotActive);
}

// ---------------------------------------------------------------- un-pausing

#[test]
fn only_both_admins_together_can_reactivate() {
    let (mut e, s, _) = rig();
    e.reconcile(&s);
    for (slot, label) in [(0usize, "sl8 only"), (1, "rov only")] {
        let mut ix = reactivate_ix(&e, &s);
        ix.accounts[1 - slot].is_signer = false; // the other admin does not sign
        assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
        assert!(!e.registry(&s).active, "{label} cannot reactivate");
    }
    // a stranger cannot stand in for either admin
    let mut ix = reactivate_ix(&e, &s);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    assert_vault_err(&e.send(ix), VaultError::MissingMultisigSignature);
    assert!(!e.registry(&s).active);

    e.advance(500);
    e.resume(&s); // both
    let r = e.registry(&s);
    assert!(r.active);
    assert_eq!((r.pause_reason, r.paused_since, r.total_paused_secs), (0, 0, 500));
}

#[test]
fn a_still_mismatching_product_can_be_paused_again_by_anyone_after_reactivation() {
    let (mut e, s, _) = rig();
    e.reconcile(&s);
    e.advance(100);
    e.resume(&s);
    assert_active(&e, &s);
    e.advance(100);
    e.reconcile(&s); // still no tally
    assert_paused_by_reconciliation(&e, &s, T0 + 200);
    assert_eq!(e.registry(&s).total_paused_secs, 100, "the first pause stays banked");
}

#[test]
fn a_product_that_now_matches_stays_active_after_reactivation() {
    let (mut e, s, _) = rig();
    e.set_tally(&s, 2, 300); // the sector under-reports...
    e.reconcile(&s);
    assert!(!e.registry(&s).active);
    e.resume(&s);
    e.set_tally(&s, 3, 600); // ...and catches up
    e.reconcile(&s);
    assert_active(&e, &s);
}

#[test]
fn an_over_reporting_tally_can_never_match_again() {
    // counts only go up on both sides: the sector's 4/700 > the vault's 3/600 cannot be undone
    let (mut e, s, w) = rig();
    e.set_tally(&s, 4, 700);
    e.reconcile(&s);
    assert!(!e.registry(&s).active);
    e.resume(&s);
    e.payout(&s, &w, 1, 100, 4); // vault catches up to 4/700 ...
    e.set_tally(&s, 4, 700);
    e.reconcile(&s);
    assert_active(&e, &s); // ... so equal numbers DO match (the tally is just ahead until then)
    // but a sector that is ahead by a request the vault never accepted stays ahead
    e.set_tally(&s, 6, 900);
    e.reconcile(&s);
    assert!(!e.registry(&s).active);
}
