//! settle_claims: pays queued claims pro rata, with one ratio per cycle.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::errors::VaultError;
use core_vault::state::PayoutClaim;
use litesvm::types::TransactionResult;
use solana_account::Account as RawAccount;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// One dollar in 6-decimal base units.
const D: u64 = 1_000_000;

fn world() -> (Env, Sector) {
    Env::registered(&Cfg { max_payout: 9, ..Cfg::default() })
}

/// A fresh trader with a queued claim and BOTH associated token accounts.
fn claimant(e: &mut Env, s: &Sector, owed: u64) -> (Pubkey, Triple) {
    let (w, _) = e.queue_claim(s, owed);
    e.make_atas(&w);
    (w, triple(e, s, &w, 1, 1))
}

fn settle_as(e: &mut Env, caller: &Keypair, t: &[Triple]) -> TransactionResult {
    let ix = settle_ix(&caller.pubkey(), e, t);
    let fee_payer = dup(&e.payer);
    e.send_with(&[ix], &fee_payer, &[caller])
}

/// A rejected instruction: the exact error, and nothing at all changed.
fn assert_rejected(e: &mut Env, ix: Instruction, check: impl FnOnce(&TransactionResult)) {
    let before = e.digest();
    let r = e.send(ix);
    check(&r);
    assert_eq!(e.digest(), before, "a rejected settle must change nothing");
}

/// `assert_rejected` for a plain settle of `triples` by the default caller.
fn reject(e: &mut Env, triples: &[Triple], check: impl FnOnce(&TransactionResult)) {
    let ix = settle_ix(&e.payer.pubkey(), e, triples);
    assert_rejected(e, ix, check);
}

/// Pools USDC/USDT set exactly, then a cycle opened.
fn open_cycle(e: &mut Env, usdc: u64, usdt: u64) {
    e.set_pool(Coin::Usdc, usdc);
    e.set_pool(Coin::Usdt, usdt);
    e.begin();
}

fn rent(e: &Env) -> u64 {
    e.svm.minimum_balance_for_rent_exemption(PayoutClaim::SPACE)
}

// ===================================================== the founder's worked example

#[test]
fn the_founders_worked_example_over_two_cycles() {
    let (mut e, s) = world();
    let (alice, a) = claimant(&mut e, &s, 3_000 * D);
    let (bob, b) = claimant(&mut e, &s, 3_400 * D);
    // Carol has no USDC account (only USDT), so she cannot be paid in cycle one.
    let (carol, _) = e.queue_claim(&s, 1_600 * D);
    e.make_ata(&carol, Coin::Usdt, 0);
    let c = triple(&e, &s, &carol, 1, 1);

    open_cycle(&mut e, 3_000 * D, 1_000 * D);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (8_000 * D, 4_000 * D)); // ratio 1/2
    let totals = e.totals();

    e.settle(&[a, b, c]);

    assert_eq!(e.pools(), (0, 800 * D));
    assert_eq!((e.token_balance(&a.1), e.token_balance(&a.2)), (1_500 * D, 0), "Alice: 1,500 all from USDC");
    assert_eq!((e.token_balance(&b.1), e.token_balance(&b.2)), (1_500 * D, 200 * D), "Bob: 1,500 USDC + 200 USDT");
    assert_eq!(e.token_balance(&c.2), 0, "Carol skipped: her USDT account is untouched too");
    assert_eq!(e.claim(&s, &alice, 1, 1).owed, 1_500 * D);
    assert_eq!(e.claim(&s, &bob, 1, 1).owed, 1_700 * D);
    assert_eq!(e.claim(&s, &carol, 1, 1).owed, 1_600 * D);
    for w in [&alice, &bob, &carol] {
        assert_eq!(e.claim(&s, w, 1, 1).last_settled_cycle, 1, "every claim was processed this cycle");
    }
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (3, 4_800 * D));
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (3, 3));
    assert_eq!(e.totals(), totals, "tokens conserved");
    e.assert_claim_invariant();

    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (0, 200 * D));
    assert_eq!(vs.floor_updated_at, T0);
    assert!(!vs.cycle_active);

    // ---- cycle two: 5 days later, Carol has made her USDC account, pools hold 4,800
    e.advance(GAP);
    e.make_ata(&carol, Coin::Usdc, 0);
    e.set_pool(Coin::Usdc, 4_000 * D);
    e.set_pool(Coin::Usdt, 800 * D);
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (2, 4_800 * D, 4_800 * D));
    let totals = e.totals();
    let caller = e.new_caller();
    let caller_before = e.lamports(&caller.pubkey());

    assert_ok(settle_as(&mut e, &caller, &[a, b, c]));

    assert_eq!(e.pools(), (0, 0));
    // everyone has now received exactly what they were owed
    assert_eq!(e.token_balance(&a.1) + e.token_balance(&a.2), 3_000 * D);
    assert_eq!(e.token_balance(&b.1) + e.token_balance(&b.2), 3_400 * D);
    assert_eq!(e.token_balance(&c.1) + e.token_balance(&c.2), 1_600 * D);
    assert_eq!((e.token_balance(&c.1), e.token_balance(&c.2)), (800 * D, 800 * D), "tie -> USDC first, USDT tops up");
    for w in [&alice, &bob, &carol] {
        assert!(e.claim_opt(&s, w, 1, 1).is_none());
        let acct = e.svm.get_account(&claim_key(&s, w, 1, 1));
        assert!(acct.map(|x| x.lamports == 0 && x.data.is_empty()).unwrap_or(true), "closed");
    }
    assert_eq!(e.lamports(&caller.pubkey()) - caller_before, 3 * rent(&e), "all three rents go to the caller");
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (0, 0));
    assert_eq!(e.totals(), totals);
    e.assert_claim_invariant();
    e.finalize();
    assert_eq!((e.vault_state().usdc_floor, e.vault_state().usdt_floor), (0, 0), "floors never limited the payout");
}

// ================================================================ the arithmetic

#[test]
fn rounding_dust_stays_owed() {
    let (mut e, s) = world();
    let (w1, t1) = claimant(&mut e, &s, 7);
    let (w2, t2) = claimant(&mut e, &s, 14);
    open_cycle(&mut e, 7, 0); // 7 available for 21 owed: ratio 1/3
    e.settle(&[t1, t2]);
    assert_eq!(e.token_balance(&t1.1), 2, "floor(7 * 7 / 21) = 2");
    assert_eq!(e.token_balance(&t2.1), 4, "floor(14 * 7 / 21) = 4");
    assert_eq!(e.claim(&s, &w1, 1, 1).owed, 5);
    assert_eq!(e.claim(&s, &w2, 1, 1).owed, 10);
    assert_eq!(e.pools(), (1, 0), "the unpaid rounding dust stays in the pool");
    assert_eq!(e.vault_state().open_claims_total, 15);
    e.assert_claim_invariant();
}

#[test]
fn ratio_one_pays_in_full_even_with_lots_of_spare_liquidity() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 300);
    open_cycle(&mut e, 10_000, 10_000);
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 300);
    assert_eq!(e.pools(), (9_700, 10_000));
    assert!(e.claim_opt(&s, &w, 1, 1).is_none(), "paid in full => closed");
    assert_eq!(e.vault_state().open_claims_count, 0);
    e.assert_claim_invariant();
}

#[test]
fn zero_available_pays_nothing_but_still_processes_the_claim() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 500);
    open_cycle(&mut e, 0, 0);
    let totals = e.totals();
    e.settle(&[t]);
    assert_eq!((e.token_balance(&t.1), e.token_balance(&t.2)), (0, 0));
    let c = e.claim(&s, &w, 1, 1);
    assert_eq!((c.owed, c.last_settled_cycle), (500, 1));
    assert_eq!(e.vault_state().cycle_processed_count, 1);
    assert_eq!(e.totals(), totals);
    e.assert_claim_invariant();
    e.finalize();
}

#[test]
fn the_larger_pool_pays_first_and_the_other_tops_up() {
    // (usdc, usdt, owed) -> (from usdc, from usdt); available covers owed so the ratio is 1
    for (usdc, usdt, owed, want_usdc, want_usdt) in [
        (900u64, 200u64, 150u64, 150u64, 0u64), // USDC larger: all from USDC
        (200, 900, 150, 0, 150),                // USDT larger: all from USDT, USDC untouched
        (500, 500, 600, 500, 100),              // tie -> USDC first, USDT tops up
        (200, 900, 1_000, 100, 900),            // USDT first (900), USDC tops up (100)
        (700, 300, 1_000, 700, 300),            // exactly drains both
        (400, 400, 400, 400, 0),                // tie, fits in USDC alone
    ] {
        let (mut e, s) = world();
        let (_, t) = claimant(&mut e, &s, owed);
        open_cycle(&mut e, usdc, usdt);
        e.settle(&[t]);
        assert_eq!((e.token_balance(&t.1), e.token_balance(&t.2)), (want_usdc, want_usdt), "pools {usdc}/{usdt} owed {owed}");
        assert_eq!(e.pools(), (usdc - want_usdc, usdt - want_usdt));
    }
}

#[test]
fn one_cycle_one_ratio_regardless_of_claim_order() {
    // Same claims, same pools, opposite order: each claim gets the same amount.
    let mut got = vec![];
    for reversed in [false, true] {
        let (mut e, s) = world();
        let (_, t1) = claimant(&mut e, &s, 1_000);
        let (_, t2) = claimant(&mut e, &s, 3_000);
        open_cycle(&mut e, 1_000, 0); // ratio 1/4
        let batch = if reversed { vec![t2, t1] } else { vec![t1, t2] };
        e.settle(&batch);
        got.push((e.token_balance(&t1.1), e.token_balance(&t2.1)));
    }
    assert_eq!(got[0], (250, 750));
    assert_eq!(got[0], got[1]);
}

#[test]
fn pools_reduced_mid_cycle_cap_each_payment_at_the_live_balance() {
    let (mut e, s) = world();
    let (wa, a) = claimant(&mut e, &s, 600);
    let (wb, b) = claimant(&mut e, &s, 400);
    open_cycle(&mut e, 1_000, 0); // ratio 1 at begin
    e.set_pool(Coin::Usdc, 500); // ... but only 500 is left by the time claims are settled
    let totals = e.totals();
    e.settle(&[a, b]);
    assert_eq!(e.token_balance(&a.1), 500, "capped by the live balance (target was 600)");
    assert_eq!(e.token_balance(&b.1), 0, "nothing left for the second claim");
    assert_eq!(e.claim(&s, &wa, 1, 1).owed, 100, "the shortfall stays owed");
    assert_eq!(e.claim(&s, &wb, 1, 1).owed, 400);
    assert_eq!(e.claim(&s, &wb, 1, 1).last_settled_cycle, 1, "processed, not forgotten");
    assert_eq!(e.pools(), (0, 0));
    assert_eq!(e.vault_state().open_claims_total, 500);
    assert_eq!(e.totals(), totals);
    e.assert_claim_invariant();
    e.finalize();
}

#[test]
fn the_live_cap_sums_both_pools() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 1_000);
    open_cycle(&mut e, 800, 700); // ratio 1
    e.set_pool(Coin::Usdc, 300);
    e.set_pool(Coin::Usdt, 400);
    e.settle(&[t]);
    assert_eq!((e.token_balance(&t.1), e.token_balance(&t.2)), (300, 400));
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 300);
}

#[test]
fn two_claims_of_one_wallet_share_its_associated_accounts() {
    let (mut e, s) = world();
    let (w, t1) = claimant(&mut e, &s, 100);
    e.deposit(&s, &w, 2);
    e.payout(&s, &w, 2, 250, 1);
    let t2 = triple(&e, &s, &w, 2, 1);
    assert_eq!((t1.1, t1.2), (t2.1, t2.2), "same wallet => same ATAs");
    open_cycle(&mut e, 10_000, 0);
    e.settle(&[t1, t2]);
    assert_eq!(e.token_balance(&t1.1), 350);
    assert_eq!(e.vault_state().open_claims_count, 0);
    e.assert_claim_invariant();
}

// ======================================================= eligibility / double use

#[test]
fn a_claim_created_during_the_cycle_is_not_eligible_until_the_next() {
    let (mut e, s) = world();
    let (_, old) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 10_000, 0);
    let (w_new, new) = claimant(&mut e, &s, 200); // created mid-cycle
    assert_eq!(e.claim(&s, &w_new, 1, 1).created_in_cycle, 1);

    reject(&mut e, &[new], |r| {
        assert_vault_err(r, VaultError::ClaimNotEligible)
    });
    // one bad claim reverts the whole batch, including the eligible one before it
    reject(&mut e, &[old, new], |r| {
        assert_vault_err(r, VaultError::ClaimNotEligible)
    });
    e.settle(&[old]);
    e.finalize();

    e.advance(GAP);
    e.begin();
    e.settle(&[new]);
    assert_eq!(e.token_balance(&new.1), 200);
    assert!(e.claim_opt(&s, &w_new, 1, 1).is_none());
}

#[test]
fn a_claim_cannot_be_settled_twice_in_one_cycle() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 1_000);
    open_cycle(&mut e, 400, 0); // ratio 2/5: partial, so the claim survives
    e.settle(&[t]);
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 600);
    e.set_pool(Coin::Usdc, 5_000); // even with money available, once per cycle
    reject(&mut e, &[t], |r| {
        assert_vault_err(r, VaultError::ClaimAlreadySettled)
    });
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 600);
}

#[test]
fn a_skipped_claim_cannot_be_retried_in_the_same_cycle_either() {
    let (mut e, s) = world();
    let (w, _) = e.queue_claim(&s, 100); // no ATAs at all
    let t = triple(&e, &s, &w, 1, 1);
    open_cycle(&mut e, 1_000, 0);
    e.settle(&[t]); // skipped
    e.make_atas(&w); // the trader fixes it mid-cycle...
    reject(&mut e, &[t], |r| {
        assert_vault_err(r, VaultError::ClaimAlreadySettled)
    });
    // ...and is paid next cycle
    e.finalize();
    e.advance(GAP);
    e.begin();
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 100);
}

#[test]
fn the_same_claim_twice_in_one_batch_is_caught_when_it_survives() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 1_000);
    open_cycle(&mut e, 400, 0); // partial payment => claim persists after the first listing
    reject(&mut e, &[t, t], |r| {
        assert_vault_err(r, VaultError::ClaimAlreadySettled)
    });
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 1_000, "the whole transaction reverted");
    // the same claim once is fine afterwards
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 400);
}

#[test]
fn the_same_claim_twice_in_one_batch_is_caught_when_it_was_closed() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0); // paid in full => closed after the first listing
    reject(&mut e, &[t, t], |r| {
        assert_vault_err(r, VaultError::InvalidClaim)
    });
    assert!(e.claim_opt(&s, &w, 1, 1).is_some(), "the closing reverted too");
}

#[test]
fn a_closed_claim_stays_dead() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 100);
    e.finalize();
    e.advance(GAP);
    e.begin();
    // even re-funded with lamports, the address is a system account again
    let addr = claim_key(&s, &w, 1, 1);
    let p = dup(&e.payer);
    assert_ok(e.send_with(
        &[anchor_lang::solana_program::system_instruction::transfer(&p.pubkey(), &addr, 5_000_000)],
        &p,
        &[],
    ));
    reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
}

// ================================================================ forged claims

fn claim_bytes(e: &Env, s: &Sector, w: &Pubkey) -> Vec<u8> {
    e.svm.get_account(&claim_key(s, w, 1, 1)).unwrap().data
}

fn put(e: &mut Env, addr: Pubkey, data: Vec<u8>, owner: Pubkey) {
    e.set_raw(&addr, data, owner);
}

#[test]
fn forged_or_foreign_claim_accounts_are_invalid_claims() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    let good = claim_bytes(&e, &s, &w);
    let program = core_vault::ID;
    let sys = anchor_lang::system_program::ID;

    // each case: (label, claim account address to pass)
    let mut cases: Vec<(&str, Pubkey)> = vec![];

    let a = Pubkey::new_unique();
    put(&mut e, a, good.clone(), sys);
    cases.push(("valid claim bytes but owned by the system program", a));

    let a = Pubkey::new_unique();
    put(&mut e, a, good.clone(), anchor_spl::token::spl_token::ID);
    cases.push(("valid claim bytes but owned by the token program", a));

    let a = Pubkey::new_unique();
    put(&mut e, a, good.clone(), program);
    cases.push(("our program owns it, valid bytes, but it is not the canonical address", a));

    let mut bad_bump = good.clone();
    *bad_bump.last_mut().unwrap() ^= 1;
    let a = Pubkey::new_unique();
    put(&mut e, a, bad_bump.clone(), program);
    cases.push(("wrong stored bump at a random address", a));

    let a = Pubkey::new_unique();
    cases.push(("an address with no account at all", a));

    cases.push(("a TraderState account", s.trader(&w, 1)));
    cases.push(("the VaultState account", e.vault));
    cases.push(("a pool token account", e.usdc_pool));

    for (label, addr) in cases {
        let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
        ix.accounts[ST].pubkey = addr;
        assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidClaim));
        let _ = label;
    }

    // at the canonical address, genuine bytes, but not owned by our program
    let canonical = claim_key(&s, &w, 1, 1);
    let lamports = e.svm.get_account(&canonical).unwrap().lamports;
    for foreign in [sys, anchor_spl::token::spl_token::ID, Pubkey::new_unique()] {
        e.svm
            .set_account(canonical, RawAccount { lamports, data: good.clone(), owner: foreign, executable: false, rent_epoch: 0 })
            .unwrap();
        reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
    }
    e.svm
        .set_account(canonical, RawAccount { lamports, data: good.clone(), owner: program, executable: false, rent_epoch: 0 })
        .unwrap();

    // at the canonical address but with tampered content
    let tamper = |e: &mut Env, f: &dyn Fn(&mut Vec<u8>)| {
        let mut d = good.clone();
        f(&mut d);
        e.svm
            .set_account(canonical, RawAccount { lamports, data: d, owner: program, executable: false, rent_epoch: 0 })
            .unwrap();
    };
    // layout: 8 disc | 32 wallet | 32 trader_state | 32 product | 8 request_id | 8 owed | 8 created | 8 last | 1 bump
    tamper(&mut e, &|d| d[104] ^= 1); // request_id
    reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
    tamper(&mut e, &|d| d[40] ^= 1); // trader_state
    reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
    tamper(&mut e, &|d| d[0] ^= 1); // discriminator
    reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
    tamper(&mut e, &|d| d.truncate(20)); // too short
    reject(&mut e, &[t], |r| assert_vault_err(r, VaultError::InvalidClaim));
    // restore: the genuine claim settles fine
    tamper(&mut e, &|_| {});
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 100);
}

#[test]
fn a_claim_account_that_is_not_writable_is_rejected() {
    let (mut e, s) = world();
    let (_, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
    ix.accounts[ST].is_writable = false;
    assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidClaim));
}

// ============================================================ destination checks

#[test]
fn a_destination_that_is_not_the_associated_account_is_a_hard_error() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    let (_victim, v) = claimant(&mut e, &s, 50);
    let plain = e.fund_wallet(&w); // the wallet's own, non-associated token accounts
    open_cycle(&mut e, 1_000, 1_000);

    let wrong_usdc: Vec<(&str, Pubkey)> = vec![
        ("random address", Pubkey::new_unique()),
        ("another wallet's ATA", v.1),
        ("the wallet's non-associated token account", plain.usdc),
        ("a USDT ATA in the USDC slot", t.2),
        ("the vault's own pool", e.usdc_pool),
        ("SL8's token account", e.sl8_usdc),
    ];
    for (label, bad) in wrong_usdc {
        let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
        ix.accounts[ST + 1].pubkey = bad;
        assert_rejected(&mut e, ix, |r| {
            let _ = label;
            assert_vault_err(r, VaultError::InvalidTokenAccount)
        });
    }
    let wrong_usdt: Vec<Pubkey> = vec![Pubkey::new_unique(), v.2, plain.usdt, t.1, e.usdt_pool];
    for bad in wrong_usdt {
        let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
        ix.accounts[ST + 2].pubkey = bad;
        assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
    }
    // swapped
    let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
    ix.accounts.swap(ST + 1, ST + 2);
    assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));

    // a garbage address can never get a claim marked processed
    assert_eq!(e.vault_state().cycle_processed_count, 0);
    assert_eq!(e.claim(&s, &w, 1, 1).last_settled_cycle, 0);
    // the right accounts then pay normally
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 100);
}

#[test]
fn a_wrong_address_in_the_second_claim_reverts_the_first_too() {
    let (mut e, s) = world();
    let (w1, t1) = claimant(&mut e, &s, 100);
    let (_, mut t2) = claimant(&mut e, &s, 100);
    t2.1 = Pubkey::new_unique();
    open_cycle(&mut e, 1_000, 0);
    reject(&mut e, &[t1, t2], |r| {
        assert_vault_err(r, VaultError::InvalidTokenAccount)
    });
    assert_eq!(e.claim(&s, &w1, 1, 1).owed, 100);
    assert_eq!(e.token_balance(&t1.1), 0);
}

/// Breaks one of a trader's destination accounts in a particular way.
type Breaker = fn(&mut Env, &Pubkey /*ata*/, &Pubkey /*wallet*/, &Pubkey /*mint*/);

/// An address-correct but unusable destination means SKIP: the claim stays open
/// and owed, counts as processed, pays NOTHING (not even to the healthy account),
/// and does not stop the next claim in the batch from being paid.
fn skip_case(break_dest: Breaker) {
    for break_usdc in [true, false] {
        let (mut e, s) = world();
        let (w, t) = claimant(&mut e, &s, 600);
        let (hw, h) = claimant(&mut e, &s, 400);
        open_cycle(&mut e, 10_000, 10_000);
        let (ata_addr, mint) = if break_usdc { (t.1, e.usdc) } else { (t.2, e.usdt) };
        break_dest(&mut e, &ata_addr, &w, &mint);
        let totals = e.totals();

        e.settle(&[t, h]);

        let c = e.claim(&s, &w, 1, 1);
        assert_eq!((c.owed, c.last_settled_cycle), (600, 1), "still owed, but processed this cycle");
        assert!(e.claim_opt(&s, &hw, 1, 1).is_none(), "the healthy claim behind it was paid and closed");
        assert_eq!(e.token_balance(&h.1), 400);
        // the healthy one of the skipped trader's two accounts received nothing
        let healthy = if break_usdc { t.2 } else { t.1 };
        assert_eq!(e.token_balance(&healthy), 0);
        let vs = e.vault_state();
        assert_eq!((vs.cycle_processed_count, vs.cycle_eligible_count), (2, 2));
        assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, 600));
        assert_eq!(e.pools(), (9_600, 10_000));
        assert_eq!(e.totals(), totals, "conserved");
        e.assert_claim_invariant();
        e.finalize(); // a skipped claim counts as processed
    }
}

fn remove(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.svm.set_account(*a, RawAccount::default()).unwrap();
}
fn dusted_system(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.svm
        .set_account(*a, RawAccount { lamports: 2_039_280, data: vec![], owner: anchor_lang::system_program::ID, executable: false, rent_epoch: 0 })
        .unwrap();
}
fn other_owner(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.edit_token_account(a, |t| t.owner = Pubkey::new_unique());
}
fn frozen(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.edit_token_account(a, |t| t.state = AccountState::Frozen);
}
fn wrong_mint(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.edit_token_account(a, |t| t.mint = Pubkey::new_unique());
}
fn not_token_program(e: &mut Env, a: &Pubkey, w: &Pubkey, m: &Pubkey) {
    let data = valid_token_bytes(m, w);
    e.set_raw(a, data, Pubkey::new_unique());
}
fn token_2022_owned(e: &mut Env, a: &Pubkey, w: &Pubkey, m: &Pubkey) {
    let data = valid_token_bytes(m, w);
    e.set_raw(a, data, TOKEN_2022_ID);
}
fn uninitialised(e: &mut Env, a: &Pubkey, _w: &Pubkey, _m: &Pubkey) {
    e.set_raw(a, vec![0u8; 165], anchor_spl::token::spl_token::ID);
}
fn wrong_length(e: &mut Env, a: &Pubkey, w: &Pubkey, m: &Pubkey) {
    let mut data = valid_token_bytes(m, w);
    data.truncate(100);
    e.set_raw(a, data, anchor_spl::token::spl_token::ID);
}
fn valid_token_bytes(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
    use anchor_spl::token::spl_token::{
        solana_program::{program_option::COption, program_pack::Pack},
        state::Account as SplAccount,
    };
    let mut data = vec![0u8; SplAccount::LEN];
    SplAccount::pack(
        SplAccount {
            mint: *mint,
            owner: *owner,
            amount: 0,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        },
        &mut data,
    )
    .unwrap();
    data
}

#[test] fn skip_when_the_account_is_missing() { skip_case(remove) }
#[test] fn skip_when_the_address_is_a_system_account_holding_lamports() { skip_case(dusted_system) }
#[test] fn skip_when_the_token_owner_was_reassigned() { skip_case(other_owner) }
#[test] fn skip_when_the_account_is_frozen() { skip_case(frozen) }
#[test] fn skip_when_the_account_holds_a_different_mint() { skip_case(wrong_mint) }
#[test] fn skip_when_the_account_is_not_owned_by_the_token_program() { skip_case(not_token_program) }
#[test] fn skip_when_the_account_belongs_to_token_2022() { skip_case(token_2022_owned) }
#[test] fn skip_when_the_token_account_is_uninitialised() { skip_case(uninitialised) }
#[test] fn skip_when_the_token_account_data_has_the_wrong_length() { skip_case(wrong_length) }

#[test]
fn a_claim_with_both_destinations_unusable_is_skipped_once() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 1_000);
    let (usdc, usdt) = (e.usdc, e.usdt);
    remove(&mut e, &t.1, &w, &usdc);
    frozen(&mut e, &t.2, &w, &usdt);
    e.settle(&[t]);
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 100);
    assert_eq!(e.vault_state().cycle_processed_count, 1);
}

#[test]
fn a_skipped_claim_is_never_expired_and_is_paid_when_fixed() {
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    let (usdc, usdt) = (e.usdc, e.usdt);
    remove(&mut e, &t.1, &w, &usdc);
    for cycle in 1..=3u64 {
        e.set_pool(Coin::Usdc, 1_000);
        e.begin();
        e.settle(&[t]);
        assert_eq!(e.claim(&s, &w, 1, 1).owed, 100, "cycle {cycle}: still owed, reserved forever");
        assert_eq!(e.vault_state().open_claims_total, 100);
        e.finalize();
        e.advance(GAP);
    }
    e.make_ata(&w, Coin::Usdc, 0);
    e.begin();
    e.settle(&[t]);
    assert_eq!(e.token_balance(&t.1), 100);
    assert!(e.claim_opt(&s, &w, 1, 1).is_none());
}

// =========================================================== batches and cycles

#[test]
fn an_empty_batch_is_rejected() {
    let (mut e, s) = world();
    claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    reject(&mut e, &[], |r| assert_vault_err(r, VaultError::EmptyBatch));
}

#[test]
fn a_batch_whose_length_is_not_a_multiple_of_three_is_rejected() {
    let (mut e, s) = world();
    let (_, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    for drop in [1usize, 2] {
        let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
        for _ in 0..drop {
            ix.accounts.pop();
        }
        assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidClaim));
    }
    let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t, t]);
    ix.accounts.pop();
    assert_rejected(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidClaim));
}

#[test]
fn settling_needs_an_open_cycle() {
    let (mut e, s) = world();
    let (_, t) = claimant(&mut e, &s, 100);
    e.set_pool(Coin::Usdc, 1_000);
    reject(&mut e, &[t], |r| {
        assert_vault_err(r, VaultError::NoCycleInProgress)
    });
    e.begin();
    e.settle(&[t]);
    e.finalize();
    reject(&mut e, &[t], |r| {
        assert_vault_err(r, VaultError::NoCycleInProgress)
    });
    // the no-cycle check comes before the batch checks
    reject(&mut e, &[], |r| {
        assert_vault_err(r, VaultError::NoCycleInProgress)
    });
}

#[test]
fn the_caller_must_sign_and_may_be_anyone() {
    let (mut e, s) = world();
    let (_, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    let mut ix = settle_ix(&Pubkey::new_unique(), &e, &[t]);
    ix.accounts[0].is_signer = false;
    assert_rejected(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
    let stranger = e.new_caller();
    assert_ok(settle_as(&mut e, &stranger, &[t]));
    assert_eq!(e.token_balance(&t.1), 100);
}

#[test]
fn the_fixed_accounts_must_be_the_vaults_own() {
    let (mut e, s) = world();
    let (_, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 1_000);
    let (usdc, usdt) = (e.usdc, e.usdt);
    let cases: Vec<(usize, Pubkey, Box<dyn Fn(&TransactionResult)>)> = vec![
        (1, Pubkey::new_unique(), Box::new(|r| assert_anchor_err(r, ErrorCode::AccountNotInitialized))),
        (2, usdt, Box::new(|r| assert_vault_err(r, VaultError::InvalidMint))),
        (3, usdc, Box::new(|r| assert_vault_err(r, VaultError::InvalidMint))),
        (4, e.usdt_pool, Box::new(|r| assert_vault_err(r, VaultError::InvalidTokenAccount))),
        (5, e.usdc_pool, Box::new(|r| assert_vault_err(r, VaultError::InvalidTokenAccount))),
        (6, TOKEN_2022_ID, Box::new(|r| assert_anchor_err(r, ErrorCode::InvalidProgramId))),
    ];
    for (slot, bad, check) in cases {
        let mut ix = settle_ix(&e.payer.pubkey(), &e, &[t]);
        ix.accounts[slot].pubkey = bad;
        assert_rejected(&mut e, ix, |r| check(r));
    }
}

#[test]
fn the_per_call_batch_limit_is_enforced() {
    let (mut e, s) = world();
    let mut ts = vec![];
    for _ in 0..core_vault::constants::MAX_SETTLE_BATCH + 1 {
        ts.push(claimant(&mut e, &s, 10).1);
    }
    open_cycle(&mut e, 10_000, 0);
    reject(&mut e, &ts, |r| {
        assert_vault_err(r, VaultError::BatchTooLarge)
    });
    e.settle(&ts[..core_vault::constants::MAX_SETTLE_BATCH]);
    e.settle(&ts[core_vault::constants::MAX_SETTLE_BATCH..]);
    assert_eq!(e.vault_state().open_claims_count, 0);
    e.finalize();
}

#[test]
fn settling_spans_several_calls_with_a_consistent_ratio() {
    let (mut e, s) = world();
    let (_, t1) = claimant(&mut e, &s, 1_000);
    let (_, t2) = claimant(&mut e, &s, 1_000);
    let (_, t3) = claimant(&mut e, &s, 2_000);
    open_cycle(&mut e, 2_000, 0); // ratio 1/2
    e.settle(&[t3]);
    e.settle(&[t1]);
    assert_eq!(e.vault_state().cycle_processed_count, 2);
    assert_vault_err(&e.finalize_result(), VaultError::CycleIncomplete);
    e.settle(&[t2]);
    assert_eq!((e.token_balance(&t1.1), e.token_balance(&t2.1), e.token_balance(&t3.1)), (500, 500, 1_000));
    e.finalize();
}

#[test]
fn a_closed_claim_cannot_be_revived_inside_the_same_transaction() {
    // Closing assigns the account to the system program and empties it, so even
    // re-funding it later in the SAME transaction leaves a plain system account,
    // not a program-owned shell that could be reinterpreted.
    let (mut e, s) = world();
    let (w, t) = claimant(&mut e, &s, 100);
    open_cycle(&mut e, 1_000, 0);
    let addr = claim_key(&s, &w, 1, 1);
    let p = dup(&e.payer);
    let settle = settle_ix(&p.pubkey(), &e, &[t]);
    let refund = anchor_lang::solana_program::system_instruction::transfer(&p.pubkey(), &addr, 5_000_000);
    assert_ok(e.send_with(&[settle, refund], &p, &[]));
    let acct = e.svm.get_account(&addr).expect("re-funded, so it exists");
    assert_eq!(acct.owner, anchor_lang::system_program::ID);
    assert!(acct.data.is_empty());
    assert!(e.claim_opt(&s, &w, 1, 1).is_none());
}
