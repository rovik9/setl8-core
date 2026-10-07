//! register_product / update_product_config / pause_product / reactivate_product
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::{instruction::Instruction, pubkey::Pubkey};
use common::*;
use core_vault::errors::VaultError;
use solana_signer::Signer;
use solana_transaction_error::TransactionError;

// ------------------------------------------------------------------ register

#[test]
fn register_with_real_signatures_sigverify_on_and_missing_signature_fails() {
    let mut e = Env::new_sigverify_on();
    let s = Sector::new();
    let c = Cfg::default();

    // Both admins really sign: succeeds. (sl8 is the fee payer here.)
    let ix = register_ix(&e, &s, &c);
    let (sl8, rov) = (dup(&e.sl8), dup(&e.rov));
    assert_ok(e.send_with(&[ix], &sl8, &[&sl8, &rov]));
    assert!(e.registry(&s).active);

    // rov's signature absent: the runtime itself rejects the tx.
    let s2 = Sector::new();
    let ix = register_ix(&e, &s2, &c);
    let r = e.send_with(&[ix], &sl8, &[&sl8]);
    match r {
        Err(f) => assert_eq!(f.err, TransactionError::SignatureFailure, "logs: {:?}", f.meta.logs),
        Ok(_) => panic!("register_product must not succeed without rov's signature"),
    }
    assert!(e.svm.get_account(&s2.registry()).is_none());
}

#[test]
fn register_succeeds_when_the_fee_payer_is_a_third_party_not_sl8_admin() {
    // Regression for shared-interfaces v0.3.0, whose builder marked sl8_admin
    // read-only: the runtime then rejected the tx with PrivilegeEscalation
    // unless sl8_admin was also the fee payer. Builder used exactly as shipped.
    let mut e = Env::new();
    let s = Sector::new();
    let ix = register_ix(&e, &s, &Cfg::default());
    assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable, "builder: sl8_admin is [signer, writable]");
    assert!(ix.accounts[1].is_signer && !ix.accounts[1].is_writable, "builder: rov_admin is [signer]");

    assert_ne!(e.payer.pubkey(), e.sl8.pubkey(), "fee payer must be a third party");
    let (sl8_before, payer_before) = (
        e.svm.get_balance(&e.sl8.pubkey()).unwrap(),
        e.svm.get_balance(&e.payer.pubkey()).unwrap(),
    );
    // third-party payer pays the fee; sl8 and rov co-sign (really signed here too)
    assert_ok(e.send(ix));

    assert!(e.registry(&s).active);
    let rent = e.svm.minimum_balance_for_rent_exemption(core_vault::state::ProductRegistry::SPACE);
    assert_eq!(
        sl8_before - e.svm.get_balance(&e.sl8.pubkey()).unwrap(),
        rent,
        "sl8_admin (not the fee payer) funds the registry rent"
    );
    assert!(
        payer_before - e.svm.get_balance(&e.payer.pubkey()).unwrap() < rent,
        "the fee payer only paid the tx fee"
    );
}

#[test]
fn register_initializes_every_registry_field() {
    let (e, s) = Env::registered(&Cfg::default());
    let r = e.registry(&s);
    let c = Cfg::default();
    assert_eq!(r.product_program_id, s.id);
    assert_eq!(r.challenge_sizes, c.tiers);
    assert_eq!(r.fee_split_bps, 6500);
    assert_eq!(r.max_payout_count, 5);
    assert!(r.active);
    assert_eq!(r.total_requests_emitted, 0);
    assert_eq!(r.reset_price_bps, vec![100, 150, 450]);
    assert_eq!(r.pause_reason, PAUSE_NONE);
    assert_eq!(r.paused_since, 0);
    assert_eq!(r.total_paused_secs, 0);
    let (_, bump) = Pubkey::find_program_address(&[b"product_registry", s.id.as_ref()], &core_vault::ID);
    assert_eq!(r.bump, bump);
    let acct = e.svm.get_account(&s.registry()).unwrap();
    assert_eq!(acct.data.len(), core_vault::state::ProductRegistry::SPACE);
}

#[test]
fn register_duplicate_product_is_rejected_and_leaves_registry_untouched() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let before = e.svm.get_account(&s.registry()).unwrap().data;
    let other = Cfg { fee_split_bps: 1, max_payout: 99, ..Cfg::default() };
    // `init` fires before the handler, so ProductAlreadyRegistered is never
    // reached: the system program reports AccountAlreadyInUse instead.
    let r = e.send(register_ix(&e, &s, &other));
    assert_already_in_use(&r);
    assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before);
}

fn tiers(n: usize) -> Vec<ChallengeSize> {
    (0..n).map(|i| ChallengeSize { size: 1_000 + i as u64, cost: 10 + i as u64 }).collect()
}

#[test]
fn register_challenge_size_bound() {
    let mut e = Env::new();
    // exactly at the bound: ok
    let ok_s = Sector::new();
    e.register(&ok_s, &Cfg { tiers: tiers(MAX_CHALLENGE_SIZES), ..Cfg::default() });
    assert_eq!(e.registry(&ok_s).challenge_sizes.len(), MAX_CHALLENGE_SIZES);
    // one over: TooManyChallengeSizes, nothing created
    let bad = Sector::new();
    let r = e.send(register_ix(&e, &bad, &Cfg { tiers: tiers(MAX_CHALLENGE_SIZES + 1), ..Cfg::default() }));
    assert_vault_err(&r, VaultError::TooManyChallengeSizes);
    assert!(e.svm.get_account(&bad.registry()).is_none());
}

#[test]
fn register_reset_phase_bound() {
    let mut e = Env::new();
    let ok_s = Sector::new();
    e.register(&ok_s, &Cfg { reset_bps: vec![100; MAX_RESET_PHASES], ..Cfg::default() });
    assert_eq!(e.registry(&ok_s).reset_price_bps.len(), MAX_RESET_PHASES);
    let bad = Sector::new();
    let r = e.send(register_ix(&e, &bad, &Cfg { reset_bps: vec![100; MAX_RESET_PHASES + 1], ..Cfg::default() }));
    assert_vault_err(&r, VaultError::TooManyResetPhases);
    assert!(e.svm.get_account(&bad.registry()).is_none());
}

// -------------------------------------------------------------------- update

#[test]
fn update_replaces_config_and_leaves_state_fields_alone() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.pause(&s);
    let before = e.registry(&s);
    let new = Cfg {
        fee_split_bps: 7000,
        tiers: vec![ChallengeSize { size: 77, cost: 7 }],
        max_payout: 10,
        reset_bps: vec![5],
    };
    e.update(&s, &new);
    let after = e.registry(&s);
    assert_eq!(after.fee_split_bps, 7000);
    assert_eq!(after.challenge_sizes, new.tiers);
    assert_eq!(after.max_payout_count, 10);
    assert_eq!(after.reset_price_bps, vec![5]);
    // not config => untouched (a config edit must not un-pause or reset counters)
    assert_eq!(after.active, before.active);
    assert_eq!(after.pause_reason, before.pause_reason);
    assert_eq!(after.paused_since, before.paused_since);
    assert_eq!(after.total_paused_secs, before.total_paused_secs);
    assert_eq!(after.total_requests_emitted, before.total_requests_emitted);
    assert_eq!(after.product_program_id, before.product_program_id);
    assert_eq!(after.bump, before.bump);
}

#[test]
fn update_bounds() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.update(&s, &Cfg { tiers: tiers(MAX_CHALLENGE_SIZES), reset_bps: vec![1; MAX_RESET_PHASES], ..Cfg::default() });
    let before = e.svm.get_account(&s.registry()).unwrap().data;

    let r = e.send(update_ix(&e, &s, &Cfg { tiers: tiers(MAX_CHALLENGE_SIZES + 1), ..Cfg::default() }));
    assert_vault_err(&r, VaultError::TooManyChallengeSizes);
    let r = e.send(update_ix(&e, &s, &Cfg { reset_bps: vec![1; MAX_RESET_PHASES + 1], ..Cfg::default() }));
    assert_vault_err(&r, VaultError::TooManyResetPhases);
    assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before, "failed updates must not write");
}

#[test]
fn update_unregistered_product_fails_account_not_initialized() {
    let mut e = Env::new();
    let ghost = Sector::new();
    let r = e.send(update_ix(&e, &ghost, &Cfg::default()));
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}

// ------------------------------------------------------------- 2-of-2 gating

/// For one admin instruction: every way of not presenting the two exact admin
/// keys as signers must fail with a precise error.
///  * wrong key in a slot (flagged signer)  -> VaultError::MissingMultisigSignature
///  * the right key, but not a signer       -> Anchor AccountNotSigner (the
///    `Signer` type check runs before the `address = ... @` constraint)
///  * the two admin keys swapped            -> MissingMultisigSignature
fn gate_cases(name: &str, build: impl Fn(&Env, &Sector) -> Instruction) {
    let (mut e, s) = Env::registered(&Cfg::default());
    let stranger = Pubkey::new_unique();

    for slot in [0usize, 1] {
        let who = if slot == 0 { "sl8_admin" } else { "rov_admin" };

        let mut ix = build(&e, &s);
        ix.accounts[slot].pubkey = stranger;
        let r = e.send(ix);
        assert_custom_code(&r, u32::from(VaultError::MissingMultisigSignature), &format!("{name}: wrong key as {who}"));

        let mut ix = build(&e, &s);
        ix.accounts[slot].is_signer = false;
        let r = e.send(ix);
        assert_anchor_err(&r, ErrorCode::AccountNotSigner);
    }

    let mut ix = build(&e, &s);
    let (a, b) = (ix.accounts[0].pubkey, ix.accounts[1].pubkey);
    ix.accounts[0].pubkey = b;
    ix.accounts[1].pubkey = a;
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::MissingMultisigSignature);

}

#[test]
fn register_requires_both_exact_admin_signers() {
    // register targets an unregistered product, so use a fresh sector per case
    let mut e = Env::new();
    // Funded, so that `init` (which runs BEFORE the `address = .. @` checks
    // and is paid by whoever sits in the sl8 slot) can succeed and the real
    // admin-key constraint is what rejects.
    let stranger = Pubkey::new_unique();
    e.fund(&stranger);
    for slot in [0usize, 1] {
        let s = Sector::new();
        let mut ix = register_ix(&e, &s, &Cfg::default());
        ix.accounts[slot].pubkey = stranger;
        let r = e.send(ix);
        assert_vault_err(&r, VaultError::MissingMultisigSignature);
        assert!(e.svm.get_account(&s.registry()).is_none(), "reverted: no registry may exist");

        let s = Sector::new();
        let mut ix = register_ix(&e, &s, &Cfg::default());
        ix.accounts[slot].is_signer = false;
        let r = e.send(ix);
        assert_anchor_err(&r, ErrorCode::AccountNotSigner);
        assert!(e.svm.get_account(&s.registry()).is_none());
    }
    // sl8/rov swapped (rov is funded and now pays): still rejected on the address check.
    let s = Sector::new();
    let mut ix = register_ix(&e, &s, &Cfg::default());
    let (a, b) = (ix.accounts[0].pubkey, ix.accounts[1].pubkey);
    ix.accounts[0].pubkey = b;
    ix.accounts[1].pubkey = a;
    let r = e.send(ix);
    assert_vault_err(&r, VaultError::MissingMultisigSignature);
    assert!(e.svm.get_account(&s.registry()).is_none());
}

#[test]
fn register_with_unfunded_wrong_sl8_key_fails_in_init_before_the_address_check() {
    // Characterisation: the sl8 slot is the `init` payer and Anchor runs `init`
    // before it evaluates `address = SL8_ADMIN_PUBKEY @ MissingMultisigSignature`.
    // With an unfunded impostor the system program therefore fails first
    // (InsufficientFunds = 1). Still a rejection, but not the vault error.
    let mut e = Env::new();
    let s = Sector::new();
    let mut ix = register_ix(&e, &s, &Cfg::default());
    ix.accounts[0].pubkey = Pubkey::new_unique();
    let r = e.send(ix);
    assert_custom_code(&r, 1, "system InsufficientFunds");
    assert!(e.svm.get_account(&s.registry()).is_none());
}

#[test]
fn update_requires_both_exact_admin_signers() {
    gate_cases("update_product_config", |e, s| update_ix(e, s, &Cfg::default()));
}

#[test]
fn pause_requires_both_exact_admin_signers() {
    gate_cases("pause_product", pause_ix);
}

#[test]
fn reactivate_requires_both_exact_admin_signers() {
    gate_cases("reactivate_product", reactivate_ix);
}

#[test]
fn gated_calls_with_correct_admins_but_no_effect_on_failure() {
    // A rejected pause must leave the product active.
    let (mut e, s) = Env::registered(&Cfg::default());
    let mut ix = pause_ix(&e, &s);
    ix.accounts[1].is_signer = false;
    let _ = e.send(ix);
    let r = e.registry(&s);
    assert!(r.active);
    assert_eq!(r.paused_since, 0);
}

// --------------------------------------------------------------------- pause

#[test]
fn pause_sets_reason_and_start_time() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.advance(500);
    e.pause(&s);
    let r = e.registry(&s);
    assert!(!r.active);
    assert_eq!(r.pause_reason, PAUSE_PLANNED_UPGRADE);
    assert_eq!(r.paused_since, T0 + 500);
    assert_eq!(r.total_paused_secs, 0, "nothing banked until resume");
}

#[test]
fn pausing_twice_fails_and_does_not_restart_the_window() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.pause(&s);
    e.advance(1_000);
    let r = e.send(pause_ix(&e, &s));
    assert_vault_err(&r, VaultError::ProductAlreadyPaused);
    assert_eq!(e.registry(&s).paused_since, T0, "pause window must keep its original start");
}

#[test]
fn pause_unregistered_product_fails_account_not_initialized() {
    let mut e = Env::new();
    let r = e.send(pause_ix(&e, &Sector::new()));
    assert_anchor_err(&r, ErrorCode::AccountNotInitialized);
}

// ---------------------------------------------------------------- reactivate

#[test]
fn reactivate_banks_paused_time_and_clears_pause_state() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.pause(&s);
    e.advance(3 * DAY);
    e.resume(&s);
    let r = e.registry(&s);
    assert!(r.active);
    assert_eq!(r.pause_reason, PAUSE_NONE);
    assert_eq!(r.paused_since, 0);
    assert_eq!(r.total_paused_secs, 3 * DAY);
}

#[test]
fn repeated_pauses_accumulate_banked_time() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.pause(&s);
    e.advance(3 * DAY);
    e.resume(&s);
    e.advance(10 * DAY); // active time is never banked
    assert_eq!(e.registry(&s).total_paused_secs, 3 * DAY);
    e.pause(&s);
    e.advance(2 * DAY);
    // an ongoing pause counts in paused_secs_at but is not yet banked
    let mid = e.registry(&s);
    assert_eq!(mid.total_paused_secs, 3 * DAY);
    assert_eq!(mid.paused_secs_at(e.now()), 5 * DAY);
    e.resume(&s);
    assert_eq!(e.registry(&s).total_paused_secs, 5 * DAY);
}

#[test]
fn product_can_be_paused_again_after_reactivation() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.pause(&s);
    e.resume(&s);
    e.pause(&s);
    assert!(!e.registry(&s).active);
}

#[test]
fn signer_keys_in_tests_are_the_configured_admin_keys() {
    let e = Env::new();
    assert_eq!(e.sl8.pubkey(), core_vault::constants::SL8_ADMIN_PUBKEY);
    assert_eq!(e.rov.pubkey(), core_vault::constants::ROV_ADMIN_PUBKEY);
}

// ------------------------------------------------------------- fee_split_bps

#[test]
fn register_rejects_fee_split_above_10000_and_accepts_the_edges() {
    let mut e = Env::new();
    for bad in [10_001u16, 20_000, u16::MAX] {
        let s = Sector::new();
        let r = e.send(register_ix(&e, &s, &Cfg { fee_split_bps: bad, ..Cfg::default() }));
        assert_vault_err(&r, VaultError::InvalidFeeSplit);
        assert!(e.svm.get_account(&s.registry()).is_none(), "nothing may be registered ({bad})");
    }
    for ok in [0u16, 1, 6500, 9_999, 10_000] {
        let s = Sector::new();
        e.register(&s, &Cfg { fee_split_bps: ok, ..Cfg::default() });
        assert_eq!(e.registry(&s).fee_split_bps, ok);
    }
}

#[test]
fn update_rejects_fee_split_above_10000_and_accepts_the_edges() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let before = e.svm.get_account(&s.registry()).unwrap().data;
    for bad in [10_001u16, 20_000, u16::MAX] {
        let r = e.send(update_ix(&e, &s, &Cfg { fee_split_bps: bad, max_payout: 99, ..Cfg::default() }));
        assert_vault_err(&r, VaultError::InvalidFeeSplit);
        assert_eq!(e.svm.get_account(&s.registry()).unwrap().data, before, "a rejected update writes nothing ({bad})");
    }
    for ok in [0u16, 1, 9_999, 10_000, 6500] {
        e.update(&s, &Cfg { fee_split_bps: ok, ..Cfg::default() });
        assert_eq!(e.registry(&s).fee_split_bps, ok);
    }
}
