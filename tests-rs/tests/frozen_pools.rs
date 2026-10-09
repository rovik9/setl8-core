//! SR-03: a payout pool the issuer has FROZEN is treated as empty by the heartbeat instead
//! of making `settle_claims` revert and wedging the cycle.
//!  * `begin_heartbeat`: the `available` snapshot leaves a frozen pool out.
//!  * `settle_claims`: a frozen pool's live balance is 0, so legs come from the other pool
//!    only and no transfer is attempted from a frozen account. Both frozen -> zero payment,
//!    the claim carries over and the cycle can still finalize.
//!  * `finalize_heartbeat`: floors come from the real balances (unchanged).
//! Admin withdrawals from, and deposits into, a frozen pool fail inside the token program
//! (documented, tested below): traders and bond holders can use the other mint.
mod common;
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use solana_signer::Signer;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::errors::VaultError;
use core_vault::state::BondTerm;

const M: u64 = 1_000_000;
const FROZEN: u32 = 17; // spl-token TokenError::AccountFrozen

fn world() -> (Env, Sector) {
    Env::registered(&Cfg::default())
}

/// A wallet with both associated accounts and one claim of `owed`.
fn claimant(e: &mut Env, s: &Sector, owed: u64) -> (anchor_lang::solana_program::pubkey::Pubkey, Triple) {
    let (w, _) = e.queue_claim(s, owed);
    e.make_atas(&w);
    (w, triple(e, s, &w, 1, 1))
}

fn freeze(e: &mut Env, c: Coin) {
    let pool = e.coin(c).1;
    e.edit_token_account(&pool, |t| t.state = AccountState::Frozen);
}
fn thaw(e: &mut Env, c: Coin) {
    let pool = e.coin(c).1;
    e.edit_token_account(&pool, |t| t.state = AccountState::Initialized);
}
fn got(e: &Env, w: &anchor_lang::solana_program::pubkey::Pubkey) -> (u64, u64) {
    (e.token_balance(&ata(w, &e.usdc)), e.token_balance(&ata(w, &e.usdt)))
}

#[test]
fn usdc_frozen_before_begin_the_snapshot_excludes_it_and_claims_are_paid_from_usdt_only() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    freeze(&mut e, Coin::Usdc);

    e.begin();
    let vs = e.vault_state();
    assert_eq!(vs.cycle_available_snapshot, 300 * M, "the frozen 1,000 is not available");
    assert_eq!(vs.cycle_owed_snapshot, 600 * M);
    e.settle(&[ta, tb]); // does not revert
    // ratio 300/600 = 1/2: A 200, B 100, all from USDT
    assert_eq!(got(&e, &a), (0, 200 * M));
    assert_eq!(got(&e, &b), (0, 100 * M));
    assert_eq!(e.pools(), (1_000 * M, 0), "the frozen pool was not touched");
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 200 * M);
    assert_eq!(e.claim(&s, &b, 1, 1).owed, 100 * M);
    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (250 * M, 0), "floors come from the REAL balances");
    e.assert_claim_invariant();

    // thaw: the next cycle's snapshot includes the pool and the carried-over claims are paid in full
    thaw(&mut e, Coin::Usdc);
    e.advance(GAP);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 1_000 * M);
    e.settle(&[ta, tb]);
    assert_eq!(got(&e, &a), (200 * M, 200 * M));
    assert_eq!(got(&e, &b), (100 * M, 100 * M));
    assert_eq!(e.pools(), (700 * M, 0));
    assert_eq!(e.vault_state().open_claims_count, 0);
    e.finalize();
}

#[test]
fn usdt_frozen_before_begin_is_the_mirror_image() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 300 * M);
    e.set_pool(Coin::Usdt, 1_000 * M);
    freeze(&mut e, Coin::Usdt);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 300 * M);
    e.settle(&[ta, tb]);
    assert_eq!(got(&e, &a), (200 * M, 0));
    assert_eq!(got(&e, &b), (100 * M, 0));
    assert_eq!(e.pools(), (0, 1_000 * M));
    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (0, 250 * M));
    thaw(&mut e, Coin::Usdt);
    e.advance(GAP);
    e.begin();
    e.settle(&[ta, tb]);
    assert_eq!(got(&e, &a), (200 * M, 200 * M));
    assert_eq!(got(&e, &b), (100 * M, 100 * M));
    assert_eq!(e.pools(), (0, 700 * M));
}

#[test]
fn usdc_frozen_between_begin_and_settle_the_live_balance_caps_the_payment_without_reverting() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 1_300 * M, "ratio 1 at the snapshot");
    freeze(&mut e, Coin::Usdc);
    e.settle(&[ta, tb]); // no revert
    // live = 300 (USDT only): A target 400 -> pays 300; B target 200 -> live 0 -> pays 0
    assert_eq!(got(&e, &a), (0, 300 * M));
    assert_eq!(got(&e, &b), (0, 0));
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 100 * M);
    assert_eq!(e.claim(&s, &b, 1, 1).owed, 200 * M);
    assert_eq!(e.claim(&s, &b, 1, 1).last_settled_cycle, 1, "B was processed (zero pay), not skipped by an error");
    e.finalize(); // the cycle completes
    thaw(&mut e, Coin::Usdc);
    e.advance(GAP);
    e.begin();
    e.settle(&[ta, tb]);
    assert_eq!(e.vault_state().open_claims_count, 0, "carried-over claims paid in full after the thaw");
    assert_eq!(e.pools(), (1_000 * M - 300 * M, 0));
}

#[test]
fn usdc_frozen_between_two_batches_of_one_cycle() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    e.begin();
    e.settle(&[ta]); // ratio 1, larger pool (USDC) pays
    assert_eq!(got(&e, &a), (400 * M, 0));
    freeze(&mut e, Coin::Usdc);
    e.settle(&[tb]); // USDC is now empty for settlement; the 300 USDT pays B in full
    assert_eq!(got(&e, &b), (0, 200 * M));
    assert_eq!(e.vault_state().open_claims_count, 0);
    assert_eq!(e.pools(), (600 * M, 100 * M));
    e.finalize();
}

#[test]
fn usdt_frozen_between_begin_and_settle_and_between_batches() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 300 * M);
    e.set_pool(Coin::Usdt, 1_000 * M);
    e.begin();
    e.settle(&[ta]); // USDT is larger: pays 400 in full
    assert_eq!(got(&e, &a), (0, 400 * M));
    freeze(&mut e, Coin::Usdt);
    e.settle(&[tb]);
    assert_eq!(got(&e, &b), (200 * M, 0));
    assert_eq!(e.pools(), (100 * M, 600 * M));
    e.finalize();
}

#[test]
fn both_pools_frozen_before_begin_everything_is_processed_with_zero_pay_and_the_cycle_finalizes() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    freeze(&mut e, Coin::Usdc);
    freeze(&mut e, Coin::Usdt);
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_available_snapshot, vs.cycle_owed_snapshot), (0, 600 * M));
    e.settle(&[ta, tb]);
    assert_eq!((got(&e, &a), got(&e, &b)), ((0, 0), (0, 0)));
    assert_eq!(e.pools(), (1_000 * M, 300 * M));
    assert_eq!(e.vault_state().cycle_processed_count, 2);
    e.finalize();
    assert_eq!(e.vault_state().open_claims_total, 600 * M, "nothing paid, everything carries over");
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (250 * M, 75 * M));

    thaw(&mut e, Coin::Usdc);
    thaw(&mut e, Coin::Usdt);
    e.advance(GAP);
    e.begin();
    e.settle(&[ta, tb]);
    assert_eq!(e.vault_state().open_claims_count, 0);
    assert_eq!(got(&e, &a), (400 * M, 0));
    assert_eq!(got(&e, &b), (200 * M, 0));
}

#[test]
fn both_pools_frozen_after_begin_is_also_a_clean_zero_pay() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    e.begin();
    freeze(&mut e, Coin::Usdc);
    freeze(&mut e, Coin::Usdt);
    e.settle(&[ta]);
    assert_eq!(got(&e, &a), (0, 0));
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 400 * M);
    e.finalize();
}

#[test]
fn a_pool_that_thaws_mid_cycle_does_not_change_the_cycles_ratio() {
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 400 * M);
    let (b, tb) = claimant(&mut e, &s, 200 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 300 * M);
    freeze(&mut e, Coin::Usdc);
    e.begin(); // snapshot: available 300, owed 600 -> ratio 1/2
    thaw(&mut e, Coin::Usdc);
    e.settle(&[ta, tb]);
    // the ratio is the snapshot's (1/2) even though more is spendable now: A 200, B 100,
    // from the larger (now thawed) USDC pool
    assert_eq!(got(&e, &a), (200 * M, 0));
    assert_eq!(got(&e, &b), (100 * M, 0));
    assert_eq!(e.pools(), (700 * M, 300 * M));
    e.finalize();
}

#[test]
fn a_claim_is_not_under_paid_forever_by_a_pool_that_stays_frozen() {
    // 10,000 USDC frozen, 50 USDT spendable per cycle, 100 owed. The snapshot ignores the
    // frozen money, so the ratio is 50/100 and the claim gets exactly what can be sent; had
    // the frozen pool been counted the ratio would be 1 and the live cap would do the same
    // job but only by accident of ordering. Then the pool thaws and the rest is paid.
    let (mut e, s) = world();
    let (a, ta) = claimant(&mut e, &s, 100 * M);
    e.set_pool(Coin::Usdc, 10_000 * M);
    freeze(&mut e, Coin::Usdc);
    e.set_pool(Coin::Usdt, 50 * M);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 50 * M);
    e.settle(&[ta]);
    assert_eq!(got(&e, &a), (0, 50 * M));
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 50 * M);
    e.finalize();
    // second cycle, still frozen, 30 more USDT arrive: 80 available vs 50 owed -> ratio 1
    e.advance(GAP);
    e.set_pool(Coin::Usdt, 30 * M);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 30 * M);
    e.settle(&[ta]);
    assert_eq!(got(&e, &a), (0, 80 * M), "30 of the remaining 50 (ratio 30/50 applied to 50)");
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 20 * M);
    e.finalize();
    thaw(&mut e, Coin::Usdc);
    e.advance(GAP);
    e.begin();
    e.settle(&[ta]);
    assert!(e.claim_opt(&s, &a, 1, 1).is_none(), "paid in full once the pool is back");
    assert_eq!(got(&e, &a), (20 * M, 80 * M));
    assert_eq!(e.pools(), (10_000 * M - 20 * M, 0));
}

#[test]
fn admin_withdrawal_from_a_frozen_pool_fails_in_the_token_program_and_changes_nothing() {
    let mut e = Env::new();
    e.set_pool(Coin::Usdc, 1_000 * M);
    freeze(&mut e, Coin::Usdc);
    let before = (e.token_snapshot(), e.svm.get_account(&e.vault).unwrap().data);
    let r = e.withdraw_result(Coin::Usdc, 1);
    assert_custom_code(&r, FROZEN, "token program: AccountFrozen");
    assert_eq!((e.token_snapshot(), e.svm.get_account(&e.vault).unwrap().data), before);
    // the other pool is unaffected
    e.set_pool(Coin::Usdt, 1_000 * M);
    e.withdraw(Coin::Usdt, 1);
}

#[test]
fn deposits_into_a_frozen_pool_fail_cleanly_and_the_other_mint_still_works() {
    let (mut e, s) = world();
    let w = wallet();
    e.fund_wallet(&w);
    freeze(&mut e, Coin::Usdc);
    // trader fee in the frozen coin: the pool leg fails
    let ix = deposit_fee_ix_coin(&e, &s, &w, 1, 100, 10_000, Coin::Usdc);
    let before = e.digest();
    assert_custom_code(&e.send(ix), FROZEN, "deposit_fee into a frozen pool");
    assert_eq!(e.digest(), before);
    assert!(e.trader_opt(&s, &w, 1).is_none(), "no trader state was created");
    // ... and in the other coin it works
    e.deposit_coin(&s, &w, 1, (10_000, 100), Coin::Usdt);
    // bond deposit: same
    let k = e.new_depositor();
    let r = e.bond_deposit_result(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    assert_custom_code(&r, FROZEN, "deposit_bond into a frozen pool");
    assert!(e.bond(&k.pubkey(), 0).is_none());
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdt);
}

#[test]
fn a_frozen_pool_alone_never_blocks_begin_or_finalize() {
    let (mut e, _s) = world();
    freeze(&mut e, Coin::Usdc);
    freeze(&mut e, Coin::Usdt);
    e.begin();
    assert_eq!(e.vault_state().cycle_available_snapshot, 0);
    e.finalize();
}
