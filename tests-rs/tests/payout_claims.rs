//! request_payout queues a PayoutClaim and moves NO tokens. Also holds the
//! queued-payout replacements for the retired instant-payout token tests (see the
//! mapping in the Module 3a report).
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::{pubkey::Pubkey, system_instruction};
use common::*;
use core_vault::errors::VaultError;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn rig(max_payout: u64) -> (Env, Sector, Pubkey) {
    let (mut e, s) = Env::registered(&Cfg { max_payout, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    (e, s, w)
}

/// Raw bytes of the accounts a request may write.
fn state_bytes(e: &Env, s: &Sector, w: &Pubkey) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        e.svm.get_account(&s.trader(w, 1)).unwrap().data,
        e.svm.get_account(&s.registry()).unwrap().data,
        e.svm.get_account(&e.vault).unwrap().data,
    )
}

/// A rejected request: exact error, no token moved, no state changed, no claim.
fn assert_rejected_cleanly(e: &mut Env, s: &Sector, w: &Pubkey, ix: anchor_lang::solana_program::instruction::Instruction, check: impl FnOnce(&litesvm::types::TransactionResult)) {
    let (bal, st) = (e.token_snapshot(), state_bytes(e, s, w));
    let claims = e.open_claims().len();
    let r = e.send(ix);
    check(&r);
    assert_eq!(e.token_snapshot(), bal, "no token may move");
    assert_eq!(state_bytes(e, s, w), st, "no state may change");
    assert_eq!(e.open_claims().len(), claims, "no claim may appear");
    e.assert_claim_invariant();
}

// ------------------------------------------------------------- claim contents

#[test]
fn a_request_creates_a_claim_with_every_field_exact() {
    let (mut e, s, w) = rig(5);
    let m = e.payout(&s, &w, 1, 700, 1);
    assert_payout_outcome(&m, PayoutOutcome::Paid);

    let ts_key = s.trader(&w, 1);
    let (addr, bump) = claim_pda(&ts_key, 1);
    let c = e.claim(&s, &w, 1, 1);
    assert_eq!(c.trader_wallet, w);
    assert_eq!(c.trader_state, ts_key);
    assert_eq!(c.product_program_id, s.id);
    assert_eq!(c.request_id, 1);
    assert_eq!(c.owed, 700);
    assert_eq!(c.created_in_cycle, 0);
    assert_eq!(c.last_settled_cycle, 0);
    assert_eq!(c.bump, bump, "the canonical bump is stored");

    let a = e.svm.get_account(&addr).unwrap();
    assert_eq!(a.owner, core_vault::ID);
    assert_eq!(a.data.len(), core_vault::state::PayoutClaim::SPACE);
    assert_eq!(a.lamports, e.svm.minimum_balance_for_rent_exemption(a.data.len()), "rent-exempt, exactly");

    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, 700));
    e.assert_claim_invariant();
}

#[test]
fn counters_track_every_claim_across_traders_and_requests() {
    let (mut e, s, w1) = rig(5);
    let w2 = wallet();
    e.deposit(&s, &w2, 1);
    e.payout(&s, &w1, 1, 100, 1);
    e.assert_claim_invariant();
    e.payout(&s, &w1, 1, 250, 2);
    e.payout(&s, &w2, 1, 4_000, 1);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (3, 4_350));
    assert_eq!(e.claim(&s, &w1, 1, 1).owed, 100);
    assert_eq!(e.claim(&s, &w1, 1, 2).owed, 250);
    assert_eq!(e.claim(&s, &w2, 1, 1).owed, 4_000);
    e.assert_claim_invariant();
}

#[test]
fn the_total_cannot_overflow_u64() {
    let (mut e, s, w) = rig(5);
    e.payout(&s, &w, 1, u64::MAX - 5, 1);
    let ix = payout_ix(&e, &s, &w, 1, 6, 2);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::MathOverflow));
    e.payout(&s, &w, 1, 5, 2); // exactly u64::MAX in total is fine
    assert_eq!(e.vault_state().open_claims_total, u64::MAX);
}

#[test]
fn a_claim_made_during_an_active_cycle_records_that_cycle_id() {
    let (mut e, s, w) = rig(5);
    e.set_vault_state(|v| {
        v.cycle_id = 3;
        v.cycle_active = true;
    });
    e.payout(&s, &w, 1, 10, 1);
    assert_eq!(e.claim(&s, &w, 1, 1).created_in_cycle, 3);
}

// ----------------------------------------------------------- no token movement

#[test]
fn a_request_moves_no_tokens_whatever_the_pools_hold() {
    // Replaces the retired instant-payout pool tests (larger pool picked at request
    // time, InsufficientPoolBalance, no fallback/summing): the request never looks
    // at the pools, so even an amount no pool could cover is accepted and queued.
    for (usdc, usdt) in [(0u64, 0u64), (10, 20), (5_000, 0), (0, 5_000)] {
        let (mut e, s, w) = rig(5);
        e.set_pool(Coin::Usdc, usdc);
        e.set_pool(Coin::Usdt, usdt);
        let before = e.token_snapshot();
        let m = e.payout(&s, &w, 1, 1_000_000_000_000, 1);
        assert_payout_outcome(&m, PayoutOutcome::Paid);
        assert_eq!(e.token_snapshot(), before, "ZERO tokens move");
        assert_eq!(e.claim(&s, &w, 1, 1).owed, 1_000_000_000_000);
        e.assert_claim_invariant();
    }
}

#[test]
fn a_request_needs_no_token_accounts_at_all() {
    // Replaces the retired destination-attack tests: there is no destination at
    // request time (settle_claims validates it later).
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = wallet();
    e.deposit(&s, &w, 1);
    let ix = payout_ix(&e, &s, &w, 1, 10, 1);
    let tokenish: Vec<_> = ix
        .accounts
        .iter()
        .filter(|m| [spl_token_id(), e.usdc, e.usdt, e.usdc_pool, e.usdt_pool].contains(&m.pubkey))
        .collect();
    assert!(tokenish.is_empty(), "request_payout takes no mint, pool or token program");
    assert_ok(e.send(ix));
}

fn spl_token_id() -> Pubkey {
    anchor_spl::token::spl_token::ID
}

// ----------------------------------------------------------------- stale path

#[test]
fn stale_path_abandons_ok_creates_no_claim_and_moves_no_tokens() {
    for (usdc, usdt) in [(0u64, 0u64), (5_000, 9_000)] {
        let (mut e, s, w) = rig(5);
        e.set_pool(Coin::Usdc, usdc);
        e.set_pool(Coin::Usdt, usdt);
        e.advance(INACTIVITY_LIMIT_SECS + 1);
        let before = e.token_snapshot();
        let vault_before = e.svm.get_account(&e.vault).unwrap().data;
        let ix = payout_ix(&e, &s, &w, 1, 1_000_000, 1);
        let m = assert_ok(e.send(ix));
        assert_payout_outcome(&m, PayoutOutcome::Abandoned);
        assert_eq!(e.token_snapshot(), before, "ZERO tokens move");
        let ts = e.trader(&s, &w, 1);
        assert_eq!((ts.status, ts.payout_count), (TraderStatus::Abandoned, 0));
        assert_eq!(e.registry(&s).total_requests_emitted, 0);
        let claim = claim_key(&s, &w, 1, 1);
        assert!(e.svm.get_account(&claim).is_none(), "NO claim account may exist");
        assert_eq!(e.svm.get_account(&e.vault).unwrap().data, vault_before, "counters untouched");
        e.assert_claim_invariant();
    }
}

#[test]
fn stale_path_still_needs_the_right_claim_address() {
    // Replaces `stale_path_still_needs_valid_destination_accounts`. Account
    // constraints run before the handler, so a wrong claim address fails even a
    // stale call, and then the Abandoned write does NOT persist.
    let (mut e, s, w) = rig(5);
    e.advance(INACTIVITY_LIMIT_SECS + 1);
    let mut ix = payout_ix(&e, &s, &w, 1, 10, 1);
    ix.accounts[PO.claim].pubkey = Pubkey::new_unique();
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_anchor_err(r, ErrorCode::ConstraintSeeds));
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
    // the same stale call with the right address then abandons
    let ix = payout_ix(&e, &s, &w, 1, 10, 1);
    assert_payout_outcome(&assert_ok(e.send(ix)), PayoutOutcome::Abandoned);
}

// ------------------------------------------------------------ claim address

#[test]
fn a_wrong_claim_address_is_rejected() {
    let (mut e, s, w) = rig(5);
    let other = wallet();
    e.deposit(&s, &other, 1);
    let ts = s.trader(&w, 1);
    for bad in [
        Pubkey::new_unique(),                      // random
        claim_pda(&ts, 2).0,                       // right trader, wrong request id
        claim_pda(&s.trader(&other, 1), 1).0,      // another trader's claim address
        claim_pda(&Pubkey::new_unique(), 1).0,     // PDA of a different trader-state key
        e.vault,                                   // some other program account
        w,                                         // a wallet
    ] {
        let mut ix = payout_ix(&e, &s, &w, 1, 10, 1);
        ix.accounts[PO.claim].pubkey = bad;
        assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_anchor_err(r, ErrorCode::ConstraintSeeds));
    }
    e.payout(&s, &w, 1, 10, 1);
}

#[test]
fn a_dusted_claim_address_still_works() {
    // Anyone can send lamports to a predictable PDA. create_account alone would
    // fail on it; the claim must still be creatable. Below, at and above rent.
    let rent = |e: &Env| e.svm.minimum_balance_for_rent_exemption(core_vault::state::PayoutClaim::SPACE);
    for dust in [1u64, 890_880, 5_000_000, 100_000_000] {
        let (mut e, s, w) = rig(5);
        let addr = claim_key(&s, &w, 1, 1);
        let p = dup(&e.payer);
        assert_ok(e.send_with(&[system_instruction::transfer(&p.pubkey(), &addr, dust)], &p, &[]));
        assert_eq!(e.svm.get_balance(&addr).unwrap(), dust);
        assert!(e.svm.get_account(&addr).map(|a| a.owner == anchor_lang::system_program::ID).unwrap_or(true));

        let m = e.payout(&s, &w, 1, 321, 1);
        assert_payout_outcome(&m, PayoutOutcome::Paid);
        let c = e.claim(&s, &w, 1, 1);
        assert_eq!((c.owed, c.request_id, c.trader_wallet), (321, 1, w));
        let a = e.svm.get_account(&addr).unwrap();
        assert_eq!(a.owner, core_vault::ID);
        assert_eq!(a.data.len(), core_vault::state::PayoutClaim::SPACE);
        assert_eq!(a.lamports, dust.max(rent(&e)), "kept the dust if it covers rent, topped up otherwise");
        e.assert_claim_invariant();
    }
}

// ---------------------------------------------------------------------- payer

#[test]
fn the_payer_must_sign() {
    let (mut e, s, w) = rig(5);
    let mut ix = payout_ix(&e, &s, &w, 1, 10, 1);
    ix.accounts[PO.payer].pubkey = Pubkey::new_unique(); // distinct from the fee payer
    ix.accounts[PO.payer].is_signer = false;
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
}

#[test]
fn an_unfunded_payer_cannot_create_the_claim() {
    let (mut e, s, w) = rig(5);
    let poor = Keypair::new();
    let mut ix = payout_ix(&e, &s, &w, 1, 10, 1);
    ix.accounts[PO.payer].pubkey = poor.pubkey();
    let (bal, st) = (e.token_snapshot(), state_bytes(&e, &s, &w));
    let fee_payer = dup(&e.payer);
    let r = e.send_with(&[ix], &fee_payer, &[&poor]);
    // system program: the payer cannot fund the new account (lamports would go negative)
    assert_custom_code(&r, 1, "unfunded payer");
    assert_eq!(e.token_snapshot(), bal);
    assert_eq!(state_bytes(&e, &s, &w), st);
    assert!(e.open_claims().is_empty());
}

#[test]
fn the_payer_pays_the_claims_rent_and_nobody_else() {
    let (mut e, s, w) = rig(5);
    let payer = Keypair::new();
    e.fund(&payer.pubkey());
    let mut ix = payout_ix(&e, &s, &w, 1, 10, 1);
    ix.accounts[PO.payer].pubkey = payer.pubkey();
    let before = e.svm.get_balance(&payer.pubkey()).unwrap();
    let fee_payer = dup(&e.payer);
    assert_ok(e.send_with(&[ix], &fee_payer, &[&payer]));
    let rent = e.svm.minimum_balance_for_rent_exemption(core_vault::state::PayoutClaim::SPACE);
    assert_eq!(before - e.svm.get_balance(&payer.pubkey()).unwrap(), rent);
}

// ------------------------------------------------------------ failed requests

#[test]
fn every_rejected_request_leaves_counters_and_claims_unchanged() {
    let (mut e, s, w) = rig(3);
    e.payout(&s, &w, 1, 40, 1);
    let ix = payout_ix(&e, &s, &w, 1, 0, 2);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::ZeroAmount));
    let ix = payout_ix(&e, &s, &w, 1, 10, 9);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::RequestIdMismatch));
    let mut ix = payout_ix(&e, &s, &w, 1, 10, 2);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::Unauthorized));
    e.pause(&s);
    let ix = payout_ix(&e, &s, &w, 1, 10, 2);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::ProductNotActive));
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, 40));
}

#[test]
fn each_request_gets_its_own_claim_and_ids_never_repeat() {
    let (mut e, s, w) = rig(5);
    e.payout(&s, &w, 1, 10, 1);
    e.payout(&s, &w, 1, 20, 2);
    // asking for id 1 again is a mismatch (the vault expects 3), and the old claim is untouched
    let ix = payout_ix(&e, &s, &w, 1, 99, 1);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::RequestIdMismatch));
    assert_eq!((e.claim(&s, &w, 1, 1).owed, e.claim(&s, &w, 1, 2).owed), (10, 20));
}

// ------------------------------------------------- ported non-token behaviours

#[test]
fn exactly_at_the_limit_is_not_stale_and_queues_a_claim() {
    let (mut e, s, w) = rig(5);
    e.advance(INACTIVITY_LIMIT_SECS);
    let before = e.token_snapshot();
    assert_payout_outcome(&e.payout(&s, &w, 1, 250, 1), PayoutOutcome::Paid);
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 250);
    assert_eq!(e.token_snapshot(), before);
}

#[test]
fn the_final_request_queues_a_claim_and_graduates() {
    let (mut e, s, w) = rig(2);
    e.payout(&s, &w, 1, 100, 1);
    assert_eq!(e.trader(&s, &w, 1).status, TraderStatus::Active);
    e.payout(&s, &w, 1, 200, 2);
    let ts = e.trader(&s, &w, 1);
    assert_eq!((ts.status, ts.payout_count), (TraderStatus::Graduated, 2));
    assert_eq!((e.claim(&s, &w, 1, 1).owed, e.claim(&s, &w, 1, 2).owed), (100, 200));
    e.assert_claim_invariant();

    // a request after graduation is rejected as before, and queues nothing
    let ix = payout_ix(&e, &s, &w, 1, 1, 3);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::InvalidTraderStatus));
}

#[test]
fn payout_cap_reached_rejects_without_queuing_anything() {
    let (mut e, s, w) = rig(5);
    e.payout(&s, &w, 1, 10, 1);
    e.payout(&s, &w, 1, 10, 2);
    e.update(&s, &Cfg { max_payout: 2, ..Cfg::default() });
    let ix = payout_ix(&e, &s, &w, 1, 10, 3);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::PayoutCapReached));
}

#[test]
fn request_id_mismatch_rejects_without_queuing_anything() {
    let (mut e, s, w) = rig(5);
    let ix = payout_ix(&e, &s, &w, 1, 10, 9);
    assert_rejected_cleanly(&mut e, &s, &w, ix, |r| assert_vault_err(r, VaultError::RequestIdMismatch));
}
