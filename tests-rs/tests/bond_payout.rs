//! request_bond_payout: closes a bond and queues its worth as a kind-1 claim; the
//! ordinary heartbeat settles it.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::{pubkey::Pubkey, system_instruction};
use common::*;
use core_vault::constants::{
    BOND_6M_LOCK_SECS as LOCK6, BOND_6M_TERM_SECS as TERM6, BOND_9M_LOCK_SECS as LOCK9, BOND_9M_TERM_SECS as TERM9,
    BOND_MAX_PER_WALLET, HEARTBEAT_MIN_GAP_SECS as GAP,
};
use core_vault::errors::VaultError;
use core_vault::state::{BondCapTracker, BondPosition, BondTerm, PayoutClaim, CLAIM_KIND_BOND};
use solana_account::Account as RawAccount;
use solana_keypair::Keypair;
use solana_signer::Signer;

const M: u64 = 1_000_000;

fn fee_of(x: u64) -> u64 {
    ((x as u128 * 20 + 9_999) / 10_000) as u64
}

/// A depositor with USDC/USDT accounts and ATAs, one bond of `principal` at T0.
fn rig(principal: u64, term: BondTerm, coin: Coin) -> (Env, Keypair) {
    let mut e = Env::new();
    let k = e.new_depositor();
    e.make_atas(&k.pubkey());
    e.bond_deposit(&k, 0, principal, term, coin);
    (e, k)
}

fn rent(e: &Env, n: usize) -> u64 {
    e.svm.minimum_balance_for_rent_exemption(n)
}

fn digest_all(e: &Env, k: &Keypair) -> (Vec<u8>, Vec<Option<Vec<u8>>>, Option<Vec<u8>>, BTreeMapSnap) {
    let bonds = e.bond_addrs.borrow().iter().map(|a| e.svm.get_account(a).map(|x| x.data)).collect();
    (
        e.svm.get_account(&e.vault).unwrap().data,
        bonds,
        e.svm.get_account(&bond_cap_pda(&k.pubkey()).0).map(|x| x.data),
        e.token_snapshot(),
    )
}
type BTreeMapSnap = std::collections::BTreeMap<Pubkey, u64>;

fn assert_rejected(e: &mut Env, k: &Keypair, ix: anchor_lang::solana_program::instruction::Instruction, check: impl FnOnce(&litesvm::types::TransactionResult)) {
    let before = digest_all(e, k);
    let claims_before = e.open_claims().len();
    let r = e.send_as(ix, k);
    check(&r);
    assert_eq!(digest_all(e, k), before, "a rejected request changes nothing");
    assert_eq!(e.open_claims().len(), claims_before, "no claim appears");
}

// ------------------------------------------------------------- worked examples

#[test]
fn the_three_worked_withdrawals_to_the_base_unit() {
    // (term, seconds after the deposit, gross, fee, claim)
    let cases = [
        (BondTerm::SixMonths, TERM6, 1_200 * M, 2_400_000, 1_197_600_000u64), // at maturity
        (BondTerm::SixMonths, 120 * 86_400, 1_000 * M, 2 * M, 998 * M),       // month 4
        (BondTerm::NineMonths, TERM9, 1_300 * M, 2_600_000, 1_297_400_000),   // at maturity
    ];
    for (term, after, gross, fee, net) in cases {
        for coin in [Coin::Usdc, Coin::Usdt] {
            let (mut e, k) = rig(1_000 * M, term, coin);
            let a = k.pubkey();
            let tokens = e.token_snapshot();
            let (pos_addr, claim_addr) = (bond_pda(&a, 0).0, bond_claim_pda(&a, 0).0);
            let pos_rent = e.lamports(&pos_addr);
            e.advance(after);
            let lamports_before = e.lamports(&a);

            let m = e.bond_request(&k, 0);
            println!("request_bond_payout: {} CU", m.compute_units_consumed);

            let c = e.bond_claim_opt(&a, 0).expect("claim");
            assert_eq!(c.owed, net, "{term:?} +{after}s");
            assert_eq!(
                (c.trader_wallet, c.trader_state, c.product_program_id, c.request_id, c.created_in_cycle, c.last_settled_cycle, c.kind),
                (a, pos_addr, Pubkey::default(), 0, 0, 0, CLAIM_KIND_BOND)
            );
            assert_eq!(c.bump, bond_claim_pda(&a, 0).1);
            let vs = e.vault_state();
            assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, net));
            assert_eq!(vs.bond_withdrawal_fees_retained, fee, "the fee is simply not owed");
            assert_eq!(gross - fee, net);
            assert_eq!(vs.bond_principal_open_total, 0);
            assert_eq!(e.tracker_of(&a).unwrap().open_principal_total, 0);
            assert_eq!(e.tracker_of(&a).unwrap().next_deposit_index, 1, "the index is not reused");
            assert_eq!(e.token_snapshot(), tokens, "NO tokens move at request time");

            // the position is closed and its rent went to the depositor, who paid the claim's rent
            assert!(e.bond(&a, 0).is_none());
            let gone = e.svm.get_account(&pos_addr);
            assert!(gone.map(|x| x.lamports == 0 && x.data.is_empty()).unwrap_or(true));
            assert_eq!(
                e.lamports(&a) as i128 - lamports_before as i128,
                pos_rent as i128 - rent(&e, PayoutClaim::SPACE) as i128
            );
            assert_eq!(e.svm.get_account(&claim_addr).unwrap().data.len(), PayoutClaim::SPACE);
            e.assert_bond_invariant();
            e.assert_claim_invariant();
        }
    }
}

#[test]
fn boundaries_to_the_second_for_both_terms() {
    for (term, lock, full, bps) in [(BondTerm::SixMonths, LOCK6, TERM6, 2_000u64), (BondTerm::NineMonths, LOCK9, TERM9, 3_000)] {
        let principal = 777 * M + 123;
        let gross_early = principal;
        let gross_full = principal + (principal as u128 * bps as u128 / 10_000) as u64;
        // (offset, expected gross or None = locked)
        for (off, want) in [
            (lock - 1, None),
            (lock, Some(gross_early)),
            (lock + 1, Some(gross_early)),
            (full - 1, Some(gross_early)),
            (full, Some(gross_full)),
            (full + 1, Some(gross_full)),
            (full * 3, Some(gross_full)),
        ] {
            let (mut e, k) = rig(principal, term, Coin::Usdc);
            let a = k.pubkey();
            e.advance(off);
            match want {
                None => {
                    let ix = request_bond_payout_ix(&e, &a, 0);
                    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondLocked));
                    assert!(e.bond(&a, 0).is_some(), "still open");
                }
                Some(gross) => {
                    e.bond_request(&k, 0);
                    assert_eq!(e.bond_claim_opt(&a, 0).unwrap().owed, gross - fee_of(gross), "{term:?} +{off}s");
                    assert_eq!(e.vault_state().bond_withdrawal_fees_retained, fee_of(gross));
                }
            }
        }
    }
}

#[test]
fn the_fee_rounds_up_for_awkward_principals_and_never_exceeds_the_amount() {
    for principal in [50_000_000u64, 50_000_001, 50_000_099, 999_999_999, 123_456_789, 1_000_000_001, 49_999_999_999] {
        for (term, bps, after) in [(BondTerm::SixMonths, 2_000u64, LOCK6), (BondTerm::SixMonths, 2_000, TERM6), (BondTerm::NineMonths, 3_000, TERM9)] {
            let (mut e, k) = rig(principal, term, Coin::Usdc);
            e.advance(after);
            e.bond_request(&k, 0);
            let gross = if after == LOCK6 { principal } else { principal + (principal as u128 * bps as u128 / 10_000) as u64 };
            let fee = fee_of(gross);
            assert!(fee < gross);
            let c = e.bond_claim_opt(&k.pubkey(), 0).unwrap();
            assert_eq!(c.owed, gross - fee, "principal {principal} {term:?}");
            assert_eq!(c.owed + e.vault_state().bond_withdrawal_fees_retained, gross, "claim + retained fee = gross, nothing lost");
        }
    }
}

#[test]
fn the_interest_rate_comes_from_the_position_not_the_constants() {
    let (mut e, k) = rig(1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    let mut p = e.bond(&a, 0).unwrap();
    p.interest_bps = 5_000; // as if the constants had said 50% when this bond was opened
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&p, &mut data).unwrap();
    e.set_raw(&bond_pda(&a, 0).0, data, core_vault::ID);
    e.advance(TERM6);
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&a, 0).unwrap().owed, 1_500 * M - 3 * M, "1,500 gross less the 3.0 fee");
}

// ----------------------------------------------------------------- the lock rule

#[test]
fn two_positions_of_one_wallet_have_independent_timers() {
    let mut e = Env::new();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc); // T0
    e.advance(40 * 86_400);
    e.bond_deposit(&k, 1, 200 * M, BondTerm::SixMonths, Coin::Usdc); // T0 + 40d
    e.set_time(T0 + LOCK6); // position 0 unlocked, position 1 still 40 days short
    e.bond_request(&k, 0);
    let ix = request_bond_payout_ix(&e, &k.pubkey(), 1);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondLocked));
    e.set_time(T0 + 40 * 86_400 + LOCK6);
    e.bond_request(&k, 1);
    assert_eq!(e.tracker_of(&k.pubkey()).unwrap().open_principal_total, 0);
    e.assert_bond_invariant();
    e.assert_claim_invariant();
}

// ------------------------------------------------------------------------- caps

#[test]
fn a_request_frees_the_cap_room_but_never_the_index() {
    let mut e = Env::new();
    let k = e.new_depositor();
    let a = k.pubkey();
    e.bond_deposit(&k, 0, 49_950_000_000, BondTerm::SixMonths, Coin::Usdc);
    e.bond_deposit(&k, 1, 50_000_000, BondTerm::SixMonths, Coin::Usdc); // exactly at the cap
    let ix = deposit_bond_ix(&e, &a, 2, 50_000_000, BondTerm::SixMonths, Coin::Usdc);
    assert_vault_err(&e.send_as(ix, &k), VaultError::BondWalletCapExceeded);

    e.advance(LOCK6);
    e.bond_request(&k, 1); // frees $50
    assert_eq!(e.tracker_of(&a).unwrap().open_principal_total, 49_950_000_000);
    assert_eq!(e.vault_state().bond_principal_open_total, 49_950_000_000);
    // index 1 is gone for good; the next deposit is 2
    let ix = deposit_bond_ix(&e, &a, 1, 50_000_000, BondTerm::SixMonths, Coin::Usdc);
    assert_vault_err(&e.send_as(ix, &k), VaultError::BondIndexMismatch);
    e.bond_deposit(&k, 2, 50_000_000, BondTerm::NineMonths, Coin::Usdt);
    assert_eq!(e.tracker_of(&a).unwrap().open_principal_total, BOND_MAX_PER_WALLET);
    e.assert_bond_invariant();
}

#[test]
fn the_global_cap_is_freed_by_a_request_too() {
    let mut e = Env::new();
    let mut ks = vec![];
    for _ in 0..12 {
        let k = e.new_depositor();
        e.bond_deposit(&k, 0, BOND_MAX_PER_WALLET, BondTerm::SixMonths, Coin::Usdc);
        ks.push(k);
    }
    let late = e.new_depositor();
    let ix = deposit_bond_ix(&e, &late.pubkey(), 0, 50_000_000, BondTerm::SixMonths, Coin::Usdc);
    assert_vault_err(&e.send_as(ix, &late), VaultError::BondGlobalCapExceeded);
    e.advance(LOCK6);
    e.bond_request(&ks[3], 0);
    e.bond_deposit(&late, 0, 50_000_000_000, BondTerm::SixMonths, Coin::Usdc); // takes the freed $50K
    assert_eq!(e.vault_state().bond_principal_open_total, 600_000_000_000);
    e.assert_bond_invariant();
}

// ----------------------------------------------------------------------- safety

#[test]
fn only_the_depositor_may_request_a_bond() {
    let (mut e, a) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    let mallory = e.new_depositor();
    e.bond_deposit(&mallory, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc); // has a tracker of her own
    let bob = e.new_depositor(); // has nothing
    for who in [&mallory, &bob] {
        // A's position (and A's claim address) named under the attacker's signature
        for use_claim_of in [who.pubkey(), a.pubkey()] {
            let mut ix = request_bond_payout_ix(&e, &who.pubkey(), 0);
            ix.accounts[BR.position].pubkey = bond_pda(&a.pubkey(), 0).0;
            ix.accounts[BR.claim].pubkey = bond_claim_pda(&use_claim_of, 0).0;
            let before = digest_all(&e, &a);
            let r = e.send_as(ix, who);
            // a mismatched claim address is refused by its seeds; a matching one by the position check
            if use_claim_of == who.pubkey() {
                assert_vault_err(&r, VaultError::InvalidBondPosition);
            } else {
                assert_anchor_err(&r, ErrorCode::ConstraintSeeds);
            }
            assert_eq!(digest_all(&e, &a), before);
        }
    }
    // naming A as the depositor without A's signature
    let mut ix = request_bond_payout_ix(&e, &a.pubkey(), 0);
    ix.accounts[BR.depositor].is_signer = false;
    assert_rejected(&mut e, &mallory, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
    assert!(e.bond(&a.pubkey(), 0).is_some());
    e.bond_request(&a, 0);
}

#[test]
fn a_bond_can_be_withdrawn_only_once() {
    let (mut e, k) = rig(100 * M, BondTerm::NineMonths, Coin::Usdc);
    e.advance(TERM9);
    e.bond_request(&k, 0);
    let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // not even if the closed address is re-funded
    let p = dup(&e.payer);
    assert_ok(e.send_with(&[system_instruction::transfer(&p.pubkey(), &bond_pda(&k.pubkey(), 0).0, 5_000_000)], &p, &[]));
    let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    assert_eq!(e.open_claims().len(), 1);
}

#[test]
fn missing_mismatched_and_forged_positions_are_invalid_positions() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.bond_deposit(&k, 1, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    // an index that was never opened
    let ix = request_bond_payout_ix(&e, &a, 7);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // position 1 passed for argument 0
    let mut ix = request_bond_payout_ix(&e, &a, 0);
    ix.accounts[BR.position].pubkey = bond_pda(&a, 1).0;
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // random, vault and tracker accounts as the position
    for bad in [Pubkey::new_unique(), e.vault, bond_cap_pda(&a).0, e.usdc_pool] {
        let mut ix = request_bond_payout_ix(&e, &a, 0);
        ix.accounts[BR.position].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    }
    // a genuine-looking position planted at a non-canonical address
    let forged = Pubkey::new_unique();
    let real = e.svm.get_account(&bond_pda(&a, 0).0).unwrap();
    e.set_raw(&forged, real.data.clone(), core_vault::ID);
    let mut ix = request_bond_payout_ix(&e, &a, 0);
    ix.accounts[BR.position].pubkey = forged;
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // tracker: someone else's, or missing
    let other = e.new_depositor();
    e.bond_deposit(&other, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    let mut ix = request_bond_payout_ix(&e, &a, 0);
    ix.accounts[BR.tracker].pubkey = bond_cap_pda(&other.pubkey()).0;
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    let mut ix = request_bond_payout_ix(&e, &a, 0);
    ix.accounts[BR.tracker].pubkey = Pubkey::new_unique();
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    e.bond_request(&k, 0);
}

#[test]
fn the_depositor_must_sign_the_request() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    let mut ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
    ix.accounts[BR.depositor].is_signer = false;
    assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
}

#[test]
fn the_claim_address_must_be_canonical_and_a_dusted_one_still_works() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    for bad in [Pubkey::new_unique(), bond_claim_pda(&a, 1).0, bond_claim_pda(&Pubkey::new_unique(), 0).0, claim_pda(&bond_pda(&a, 0).0, 0).0] {
        let mut ix = request_bond_payout_ix(&e, &a, 0);
        ix.accounts[BR.claim].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::ConstraintSeeds));
    }
    let addr = bond_claim_pda(&a, 0).0;
    let p = dup(&e.payer);
    assert_ok(e.send_with(&[system_instruction::transfer(&p.pubkey(), &addr, 7_777)], &p, &[]));
    e.bond_request(&k, 0);
    let acct = e.svm.get_account(&addr).unwrap();
    assert_eq!(acct.owner, core_vault::ID);
    assert_eq!(acct.lamports, rent(&e, PayoutClaim::SPACE), "topped up to rent");
    assert!(e.bond_claim_opt(&a, 0).is_some());
}

#[test]
fn a_claim_made_during_an_open_cycle_waits_for_the_next() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    e.begin();
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().created_in_cycle, 1);
    let t = (bond_claim_pda(&k.pubkey(), 0).0, ata(&k.pubkey(), &e.usdc), ata(&k.pubkey(), &e.usdt));
    assert_vault_err(&e.settle_result(&[t]), VaultError::ClaimNotEligible);
    assert_eq!(e.vault_state().cycle_eligible_count, 0);
    e.finalize();
}

#[test]
fn counters_cannot_go_negative_or_overflow() {
    // Module 4b (listed adaptation): the "claim total at the maximum" tamper used to reach the
    // checked add and fail with MathOverflow. The claims ceiling (SR-21) is now checked first
    // and refuses with ClaimsCeilingExceeded; the add itself can no longer be reached with a
    // total that high. The other four cases are unchanged (still MathOverflow).
    let cases: Vec<(&str, Box<dyn Fn(&mut Env, &Keypair)>)> = vec![
        ("tracker total below the position", Box::new(|e, k| {
            let t = e.tracker_of(&k.pubkey()).unwrap();
            let mut d = Vec::new();
            anchor_lang::AccountSerialize::try_serialize(&BondCapTracker { open_principal_total: 1, ..t }, &mut d).unwrap();
            e.set_raw(&bond_cap_pda(&k.pubkey()).0, d, core_vault::ID);
        })),
        ("global total below the position", Box::new(|e, _| e.set_vault_state(|v| v.bond_principal_open_total = 1))),
        ("retained fees at the maximum", Box::new(|e, _| e.set_vault_state(|v| v.bond_withdrawal_fees_retained = u64::MAX))),
        ("claim count at the maximum", Box::new(|e, _| e.set_vault_state(|v| v.open_claims_count = u64::MAX))),
        ("claim total at the maximum", Box::new(|e, _| e.set_vault_state(|v| v.open_claims_total = u64::MAX))),
    ];
    for (label, tamper) in cases {
        let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
        e.advance(LOCK6);
        tamper(&mut e, &k);
        let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
        let want = if label == "claim total at the maximum" { VaultError::ClaimsCeilingExceeded } else { VaultError::MathOverflow };
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, want));
    }
}

#[test]
fn bond_state_is_untouched_by_the_product_pause_and_admins_cannot_reach_it() {
    // bonds have no pause: a paused product does not matter, and there is no admin
    // instruction that reads or writes bond state
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let s = Sector::new();
    e.register(&s, &Cfg::default());
    e.pause(&s);
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    assert!(e.bond_claim_opt(&k.pubkey(), 0).is_some());
}

// ------------------------------------------------------------------ settlement
// A bond claim is an ordinary claim as far as the heartbeat is concerned.

fn bond_triple(e: &Env, dep: &Pubkey, idx: u64) -> Triple {
    (bond_claim_pda(dep, idx).0, ata(dep, &e.usdc), ata(dep, &e.usdt))
}

/// A product with one trader claim of `trader_owed`, plus a $100 six-month bond
/// withdrawn early (claim 99_800_000). Returns (env, sector, trader wallet, depositor).
fn mixed(trader_owed: u64) -> (Env, Sector, Pubkey, Keypair) {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let (w, _) = e.queue_claim(&s, trader_owed);
    e.make_atas(&w);
    let k = e.new_depositor();
    e.make_atas(&k.pubkey());
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, 99_800_000);
    (e, s, w, k)
}

#[test]
fn bond_and_trader_claims_get_identical_ratios_when_the_pool_is_short() {
    let mut paid = vec![];
    for bond_first in [false, true] {
        let (mut e, s, w, k) = mixed(100 * M);
        let (tt, bt) = (triple(&e, &s, &w, 1, 1), bond_triple(&e, &k.pubkey(), 0));
        e.set_pool(Coin::Usdc, 50 * M);
        e.set_pool(Coin::Usdt, 0);
        e.begin();
        let vs = e.vault_state();
        assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (199_800_000, 50 * M));
        let totals = e.totals();
        e.settle(&if bond_first { [bt, tt] } else { [tt, bt] });
        // the same ratio 50 / 199.8 for both, floor division each
        let trader_pay = (100 * M as u128 * (50 * M) as u128 / 199_800_000u128) as u64;
        let bond_pay = (99_800_000u128 * (50 * M) as u128 / 199_800_000u128) as u64;
        assert_eq!(e.token_balance(&tt.1), trader_pay);
        assert_eq!(e.token_balance(&bt.1), bond_pay);
        assert_eq!(e.claim(&s, &w, 1, 1).owed, 100 * M - trader_pay);
        assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, 99_800_000 - bond_pay);
        assert_eq!(e.totals(), totals);
        e.assert_claim_invariant();
        e.finalize();
        paid.push((trader_pay, bond_pay));
    }
    assert_eq!(paid[0], paid[1], "the order of claims does not change anyone's payment");
}

#[test]
fn a_bond_claim_is_paid_from_both_pools_larger_first() {
    let (mut e, k) = rig(50 * M, BondTerm::SixMonths, Coin::Usdt);
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, 49_900_000);
    let t = bond_triple(&e, &k.pubkey(), 0);
    e.set_pool(Coin::Usdc, 30 * M);
    e.set_pool(Coin::Usdt, 25 * M);
    e.begin();
    e.settle(&[t]);
    assert_eq!((e.token_balance(&t.1), e.token_balance(&t.2)), (30 * M, 19_900_000));
    assert_eq!(e.pools(), (0, 5_100_000));
    assert!(e.bond_claim_opt(&k.pubkey(), 0).is_none(), "paid in full => closed");
    e.assert_claim_invariant();
}

#[test]
fn a_depositor_without_an_ata_is_skipped_and_the_claim_is_kept() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    let t = bond_triple(&e, &a, 0);
    e.svm.set_account(t.1, RawAccount::default()).unwrap(); // the USDC ATA disappears
    e.set_pool(Coin::Usdc, 500 * M);
    e.begin();
    e.settle(&[t]);
    let c = e.bond_claim_opt(&a, 0).unwrap();
    assert_eq!((c.owed, c.last_settled_cycle), (99_800_000, 1));
    assert_eq!(e.vault_state().cycle_processed_count, 1);
    e.finalize();

    e.advance(GAP);
    e.make_ata(&a, Coin::Usdc, 0); // fixed
    e.begin();
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 99_800_000);
    assert!(e.bond_claim_opt(&a, 0).is_none());
}

#[test]
fn finalize_waits_for_bond_claims_too() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    e.set_pool(Coin::Usdc, 500 * M);
    e.begin();
    assert_eq!(e.vault_state().cycle_eligible_count, 1, "the bond claim is counted as eligible");
    assert_vault_err(&e.finalize_result(), VaultError::CycleIncomplete);
    e.settle(&[bond_triple(&e, &k.pubkey(), 0)]);
    e.finalize();
}

#[test]
fn the_unpaid_remainder_carries_over_and_the_rent_goes_to_the_settle_caller() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    let t = bond_triple(&e, &a, 0);
    e.set_pool(Coin::Usdc, 50 * M);
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 50 * M, "ratio 50 / 99.8");
    // 99.8 * 50 / 99.8 = exactly 50
    assert_eq!(e.bond_claim_opt(&a, 0).unwrap().owed, 49_800_000);
    assert_eq!(e.vault_state().open_claims_total, 49_800_000);
    e.finalize();

    e.advance(GAP);
    e.set_pool(Coin::Usdc, 60 * M);
    e.begin();
    let caller = e.new_caller();
    let before = e.lamports(&caller.pubkey());
    let ix = settle_ix(&caller.pubkey(), &e, &[t]);
    let fp = dup(&e.payer);
    assert_ok(e.send_with(&[ix], &fp, &[&caller]));
    assert_eq!(e.token_balance(&t.1), 99_800_000, "everything is paid in the end");
    assert_eq!(e.lamports(&caller.pubkey()) - before, rent(&e, PayoutClaim::SPACE), "the closed claim's rent goes to the caller");
    assert_eq!((e.vault_state().open_claims_count, e.vault_state().open_claims_total), (0, 0));
    e.assert_claim_invariant();
}

#[test]
fn settle_claims_validates_the_claim_kind() {
    let (mut e, s, w, k) = mixed(100 * M);
    let (tt, bt) = (triple(&e, &s, &w, 1, 1), bond_triple(&e, &k.pubkey(), 0));
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.begin();
    let tamper_kind = |e: &mut Env, addr: Pubkey, kind: u8| {
        let a = e.svm.get_account(&addr).unwrap();
        let mut d = a.data.clone();
        d[136] = kind; // layout: ... last_settled_cycle (128..136) | kind (136) | bump (137)
        e.svm.set_account(addr, RawAccount { data: d, ..a }).unwrap();
    };
    let reject = |e: &mut Env, t: Triple| {
        let before = e.digest();
        assert_vault_err(&e.settle_result(&[t]), VaultError::InvalidClaim);
        assert_eq!(e.digest(), before);
    };
    // unknown kinds
    for bad in [2u8, 3, 255] {
        tamper_kind(&mut e, bt.0, bad);
        reject(&mut e, bt);
        tamper_kind(&mut e, tt.0, bad);
        reject(&mut e, tt);
    }
    // each kind's claim presented as the other kind
    tamper_kind(&mut e, bt.0, 0);
    reject(&mut e, bt);
    tamper_kind(&mut e, tt.0, 1);
    reject(&mut e, tt);
    // a bond claim planted at a trader-claim style address, and the reverse
    tamper_kind(&mut e, bt.0, 1);
    tamper_kind(&mut e, tt.0, 0);
    let real = e.svm.get_account(&bt.0).unwrap();
    let stray = Pubkey::new_unique();
    e.set_raw(&stray, real.data.clone(), core_vault::ID);
    reject(&mut e, (stray, bt.1, bt.2));
    // everything restored: both settle
    e.settle(&[tt, bt]);
    assert!(e.bond_claim_opt(&k.pubkey(), 0).is_none());
    assert!(e.claim_opt(&s, &w, 1, 1).is_none());
}

#[test]
fn a_wallet_with_a_trader_claim_and_a_bond_claim_shares_its_atas() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let k = e.new_depositor();
    let a = k.pubkey();
    e.make_atas(&a);
    e.deposit(&s, &a, 1);
    e.payout(&s, &a, 1, 100 * M, 1);
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(LOCK6);
    e.bond_request(&k, 0);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.begin();
    e.settle(&[triple(&e, &s, &a, 1, 1), bond_triple(&e, &a, 0)]);
    assert_eq!(e.token_balance(&ata(&a, &e.usdc.clone())), 100 * M + 99_800_000);
    assert_eq!(e.vault_state().open_claims_count, 0);
}

// ---------------------------------------------- forged accounts at canonical addresses
// Only the program creates these accounts, so these states cannot arise on a real
// chain; they pin that each field is checked on its own, not only through the address.

fn put_position(e: &mut Env, addr: Pubkey, p: &BondPosition) {
    let mut d = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(p, &mut d).unwrap();
    e.set_raw(&addr, d, core_vault::ID);
}

#[test]
fn each_position_field_is_checked_on_its_own() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    let real = e.bond(&a, 0).unwrap();
    let addr = bond_pda(&a, 0).0;
    // the depositor field names someone else (the address is still the signer's)
    put_position(&mut e, addr, &BondPosition { depositor: Pubkey::new_unique(), ..real.clone() });
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // the index field disagrees with the argument
    put_position(&mut e, addr, &BondPosition { deposit_index: 5, ..real.clone() });
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // a wrong stored bump
    put_position(&mut e, addr, &BondPosition { bump: real.bump ^ 1, ..real.clone() });
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    put_position(&mut e, addr, &real);
    e.bond_request(&k, 0);
}

#[test]
fn the_tracker_must_belong_to_the_depositor_and_sit_at_its_address() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    let t = e.tracker_of(&a).unwrap();
    let put = |e: &mut Env, t: &BondCapTracker| {
        let mut d = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(t, &mut d).unwrap();
        e.set_raw(&bond_cap_pda(&a).0, d, core_vault::ID);
    };
    put(&mut e, &BondCapTracker { depositor: Pubkey::new_unique(), ..t.clone() });
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    put(&mut e, &BondCapTracker { bump: t.bump ^ 1, ..t.clone() });
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    // a tracker planted at a non-canonical address
    put(&mut e, &t);
    let stray = Pubkey::new_unique();
    let real = e.svm.get_account(&bond_cap_pda(&a).0).unwrap();
    e.set_raw(&stray, real.data, core_vault::ID);
    let mut ix = request_bond_payout_ix(&e, &a, 0);
    ix.accounts[BR.tracker].pubkey = stray;
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
    e.bond_request(&k, 0);
}

#[test]
fn a_position_worth_nothing_after_the_fee_is_refused() {
    let (mut e, k) = rig(100 * M, BondTerm::SixMonths, Coin::Usdc);
    let a = k.pubkey();
    e.advance(LOCK6);
    let real = e.bond(&a, 0).unwrap();
    put_position(&mut e, bond_pda(&a, 0).0, &BondPosition { principal: 1, ..real }); // gross 1, fee 1, net 0
    let ix = request_bond_payout_ix(&e, &a, 0);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::ZeroAmount));
}

#[test]
fn several_bonds_of_one_wallet_each_settle_at_their_own_claim_address() {
    let mut e = Env::new();
    let k = e.new_depositor();
    let a = k.pubkey();
    e.make_atas(&a);
    for i in 0..3u64 {
        e.bond_deposit(&k, i, (60 + 10 * i) * M, BondTerm::SixMonths, Coin::Usdc);
    }
    e.advance(LOCK6);
    for i in [2u64, 0, 1] {
        e.bond_request(&k, i);
    }
    assert_eq!(e.open_claims().len(), 3);
    let ts: Vec<Triple> = (0..3).map(|i| bond_triple(&e, &a, i)).collect();
    for (i, t) in ts.iter().enumerate() {
        assert_eq!(e.claim_at(&t.0).unwrap().request_id, i as u64, "the claim carries its deposit index");
    }
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.begin();
    e.settle(&ts);
    let want: u64 = (0..3u64).map(|i| (60 + 10 * i) * M - fee_of((60 + 10 * i) * M)).sum();
    assert_eq!(e.token_balance(&ts[0].1), want);
    assert!(e.open_claims().is_empty());
}
