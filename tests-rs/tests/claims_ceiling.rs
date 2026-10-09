//! SR-21: the vault's open claims are capped at OPEN_CLAIMS_CEILING = 2,500,000,000,000
//! base units ($2,500,000). `request_payout` used to accept any `u64` amount, so one request
//! of ~u64::MAX saturated `open_claims_total` and from then on every `request_bond_payout`
//! and every other `request_payout` failed with `MathOverflow`: bond holders could not exit.
//! Both request kinds now refuse, with `ClaimsCeilingExceeded` and no change at all, a claim
//! that would take the total above the ceiling, and succeed again once heartbeat payments
//! have brought the total down. The figure is pinned here by value, not by constant name.
mod common;
use common::*;
use core_vault::constants::{BOND_6M_LOCK_SECS, BOND_GLOBAL_CAP, OPEN_CLAIMS_CEILING};
use core_vault::errors::VaultError;
use core_vault::state::BondTerm;
use solana_signer::Signer;

const M: u64 = 1_000_000;
const CEILING: u64 = 2_500_000_000_000;
/// A 100-coin six-month bond withdrawn at the end of the hard lock owes 100 less the 0.2%
/// fee (rounded up): 99.8 coins.
const NET: u64 = 100 * M - 200_000;

fn rig() -> (Env, Sector) {
    Env::registered(&Cfg { max_payout: 9, ..Cfg::default() })
}

/// A depositor with one 100-coin bond (index 0) that is withdrawable.
fn bonded(e: &mut Env) -> solana_keypair::Keypair {
    let k = e.new_depositor();
    e.make_atas(&k.pubkey());
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(BOND_6M_LOCK_SECS);
    k
}

/// Queues one trader claim of `amount` for a fresh wallet; returns the wallet.
fn queue(e: &mut Env, s: &Sector, amount: u64) -> anchor_lang::solana_program::pubkey::Pubkey {
    let w = wallet();
    e.deposit(s, &w, 1);
    e.payout(s, &w, 1, amount, 1);
    w
}

fn total(e: &Env) -> u64 {
    e.vault_state().open_claims_total
}

/// Everything a refused bond exit must leave alone.
fn bond_digest(e: &Env, k: &solana_keypair::Keypair) -> (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>, std::collections::BTreeMap<anchor_lang::solana_program::pubkey::Pubkey, u64>) {
    let a = k.pubkey();
    let data = |p: &anchor_lang::solana_program::pubkey::Pubkey| e.svm.get_account(p).map(|x| x.data).filter(|d| !d.is_empty());
    (
        e.svm.get_account(&e.vault).unwrap().data,
        data(&bond_pda(&a, 0).0),
        data(&bond_cap_pda(&a).0),
        data(&bond_claim_pda(&a, 0).0),
        e.token_snapshot(),
    )
}

#[test]
fn the_ceiling_is_two_and_a_half_million_dollars_to_the_base_unit() {
    assert_eq!(OPEN_CLAIMS_CEILING, CEILING);
    assert_eq!(CEILING, 2_500_000 * M);
    // bonds alone can owe at most the global cap plus 30% interest, well under the ceiling
    assert_eq!(BOND_GLOBAL_CAP as u128 * 130 / 100, 780_000_000_000);
    assert!((BOND_GLOBAL_CAP as u128 * 130 / 100) < CEILING as u128);
}

#[test]
fn the_ceiling_itself_is_reachable_and_one_base_unit_more_is_refused() {
    let (mut e, s) = rig();
    let w1 = queue(&mut e, &s, CEILING);
    assert_eq!(total(&e), CEILING, "exactly the ceiling is accepted");
    e.assert_claim_invariant();
    // one base unit more, from anyone, is refused
    let w2 = wallet();
    e.deposit(&s, &w2, 1);
    let ix = payout_ix(&e, &s, &w2, 1, 1, 1);
    let before = e.digest();
    assert_vault_err(&e.send(ix), VaultError::ClaimsCeilingExceeded);
    assert_eq!(e.digest(), before, "a refused request changes nothing");
    assert_eq!(e.trader(&s, &w2, 1).payout_count, 0);
    let _ = w1;
}

#[test]
fn a_single_request_of_ceiling_plus_one_is_refused() {
    let (mut e, s) = rig();
    let w = wallet();
    e.deposit(&s, &w, 1);
    let ix = payout_ix(&e, &s, &w, 1, CEILING + 1, 1); // registers the claim address for the digest
    let before = e.digest();
    let reg = e.registry(&s);
    assert_vault_err(&e.send(ix), VaultError::ClaimsCeilingExceeded);
    assert_eq!(e.digest(), before);
    assert_eq!(e.registry(&s).total_requests_emitted, reg.total_requests_emitted);
    assert_eq!(e.registry(&s).total_requested_amount, reg.total_requested_amount);
    assert_eq!(e.trader(&s, &w, 1).payout_count, 0);
    // and the exact ceiling is fine for the same trader
    e.payout(&s, &w, 1, CEILING, 1);
    assert_eq!(total(&e), CEILING);
}

#[test]
fn a_request_near_u64_max_is_refused_and_bonds_can_still_exit() {
    let (mut e, s) = rig();
    let k = bonded(&mut e);
    let w = wallet();
    e.deposit(&s, &w, 1);
    let ix = payout_ix(&e, &s, &w, 1, u64::MAX - 5, 1);
    assert_vault_err(&e.send(ix), VaultError::ClaimsCeilingExceeded);
    // the old failure mode: the bond holder could not exit. Now they can.
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, NET);
    assert_eq!(total(&e), NET);
    e.assert_claim_invariant();
}

#[test]
fn a_bond_exit_just_below_at_and_just_above_the_ceiling() {
    // total before the exit = fill; the exit adds NET
    for (fill, ok, label) in [
        (CEILING - NET - 1, true, "one base unit below the ceiling after the exit"),
        (CEILING - NET, true, "exactly at the ceiling after the exit"),
        (CEILING - NET + 1, false, "one base unit above the ceiling after the exit"),
    ] {
        let (mut e, s) = rig();
        let k = bonded(&mut e);
        queue(&mut e, &s, fill);
        assert_eq!(total(&e), fill, "{label}");
        let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
        let before = bond_digest(&e, &k);
        let r = e.send_as(ix, &k);
        if ok {
            assert_ok(r);
            assert_eq!(total(&e), fill + NET, "{label}");
            assert!(total(&e) <= CEILING);
            assert!(e.bond(&k.pubkey(), 0).is_none(), "the position was closed");
            e.assert_claim_invariant();
        } else {
            assert_vault_err(&r, VaultError::ClaimsCeilingExceeded);
            assert_eq!(bond_digest(&e, &k), before, "{label}: a refused exit changes nothing");
            assert!(e.bond(&k.pubkey(), 0).is_some(), "the principal stays in its position");
            assert_eq!(e.tracker_of(&k.pubkey()).unwrap().open_principal_total, 100 * M);
            assert!(e.bond_claim_opt(&k.pubkey(), 0).is_none());
            e.assert_bond_invariant();
        }
    }
}

#[test]
fn a_bond_exit_blocked_by_trader_claims_is_accepted_again_after_a_heartbeat_pays_the_total_down() {
    let (mut e, s) = rig();
    let k = bonded(&mut e);
    let w = queue(&mut e, &s, CEILING - 10); // trader claims fill the headroom
    e.make_atas(&w);
    let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
    assert_vault_err(&e.send_as(ix, &k), VaultError::ClaimsCeilingExceeded);
    // repeated attempts stay refused and harmless
    let ix = request_bond_payout_ix(&e, &k.pubkey(), 0);
    assert_vault_err(&e.send_as(ix, &k), VaultError::ClaimsCeilingExceeded);
    assert!(e.bond(&k.pubkey(), 0).is_some());

    // the pool pays the trader claim in full (ratio 1): the total drops to zero
    e.set_pool(Coin::Usdc, CEILING);
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    assert_eq!(total(&e), 0);
    e.finalize();
    // now the same exit goes through
    e.bond_request(&k, 0);
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, NET);
    assert_eq!(total(&e), NET);
    e.assert_claim_invariant();
    e.assert_bond_invariant();
}

#[test]
fn a_partial_payment_that_leaves_the_total_under_the_ceiling_unblocks_the_exit() {
    let (mut e, s) = rig();
    let k = bonded(&mut e);
    let w = queue(&mut e, &s, CEILING - 10);
    e.make_atas(&w);
    e.set_pool(Coin::Usdc, 1_000 * M); // ratio far below 1: pays 1,000 coins
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    let t = total(&e);
    assert!(t < CEILING - 10, "the heartbeat paid something");
    assert!(CEILING - t >= NET, "and that opened enough headroom for the exit");
    e.finalize();
    e.bond_request(&k, 0);
    assert!(total(&e) <= CEILING);
}

#[test]
fn a_request_below_the_ceiling_still_works_for_another_product() {
    let (mut e, s) = rig();
    let s2 = Sector::new();
    e.register(&s2, &Cfg::default());
    let (w1, w2) = (wallet(), wallet());
    e.deposit(&s, &w1, 1);
    e.deposit(&s2, &w2, 1);
    e.payout(&s, &w1, 1, CEILING - 10, 1);
    e.payout(&s2, &w2, 1, 10, 1); // exactly fills it
    assert_eq!(total(&e), CEILING);
}

#[test]
fn the_ceiling_check_comes_after_the_other_checks() {
    // an over-ceiling request on a trader that fails another check reports THAT error
    let (mut e, s) = rig();
    let w = wallet();
    e.deposit(&s, &w, 1);
    let ix = payout_ix(&e, &s, &w, 1, CEILING + 1, 7); // wrong request id
    assert_vault_err(&e.send(ix), VaultError::RequestIdMismatch);
    let ix = payout_ix(&e, &s, &w, 1, 0, 1); // zero amount
    assert_vault_err(&e.send(ix), VaultError::ZeroAmount);
}
