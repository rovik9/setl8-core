//! finalize_heartbeat: ends a complete cycle and recomputes the 25% reserve floors.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::errors::VaultError;
use solana_signer::Signer;

fn world() -> (Env, Sector) {
    Env::registered(&Cfg { max_payout: 9, ..Cfg::default() })
}

#[test]
fn an_incomplete_cycle_cannot_be_finalized() {
    let (mut e, s) = world();
    let (w1, _) = e.queue_claim(&s, 100);
    let (w2, _) = e.queue_claim(&s, 100);
    e.make_atas(&w1);
    e.make_atas(&w2);
    e.set_pool(Coin::Usdc, 1_000);
    e.begin();
    assert_vault_err(&e.finalize_result(), VaultError::CycleIncomplete);
    e.settle(&[triple(&e, &s, &w1, 1, 1)]);
    assert_vault_err(&e.finalize_result(), VaultError::CycleIncomplete);
    let before = e.vault_state();
    assert!(before.cycle_active, "a rejected finalize leaves the cycle open");
    e.settle(&[triple(&e, &s, &w2, 1, 1)]);
    e.finalize();
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn it_needs_an_active_cycle() {
    let (mut e, _s) = world();
    assert_vault_err(&e.finalize_result(), VaultError::NoCycleInProgress);
    e.begin();
    e.finalize();
    assert_vault_err(&e.finalize_result(), VaultError::NoCycleInProgress);
}

#[test]
fn floors_are_a_quarter_of_each_pool_rounded_down() {
    let (mut e, _s) = world();
    e.set_pool(Coin::Usdc, 12_345_679);
    e.set_pool(Coin::Usdt, 7_654_321);
    e.begin();
    e.advance(77);
    e.finalize();
    let vs = e.vault_state();
    assert_eq!(vs.usdc_floor, 3_086_419, "floor(12,345,679 * 2500 / 10,000)");
    assert_eq!(vs.usdt_floor, 1_913_580, "floor(7,654,321 * 2500 / 10,000)");
    assert_eq!(vs.floor_updated_at, T0 + 77);
    assert!(!vs.cycle_active);
    assert_eq!(vs.cycle_started_at, T0, "the gap is measured from the cycle START: finalize leaves it alone");
    assert_eq!(e.pools(), (12_345_679, 7_654_321), "floors never move tokens");
}

#[test]
fn floors_use_the_largest_balances_without_overflow() {
    let (mut e, _s) = world();
    e.set_pool(Coin::Usdc, u64::MAX);
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    e.finalize();
    assert_eq!(e.vault_state().usdc_floor, 4_611_686_018_427_387_903);
}

#[test]
fn floors_are_taken_after_settlement_not_before() {
    let (mut e, s) = world();
    let (w, _) = e.queue_claim(&s, 2_000);
    e.make_atas(&w);
    e.set_pool(Coin::Usdc, 4_000);
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    assert_eq!(e.pools(), (2_000, 0));
    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (500, 0), "25% of the 2,000 left, not of the 4,000 before");
}

#[test]
fn floors_are_recomputed_every_cycle() {
    let (mut e, _s) = world();
    e.set_pool(Coin::Usdc, 1_000);
    e.begin();
    e.finalize();
    assert_eq!(e.vault_state().usdc_floor, 250);
    e.advance(GAP);
    e.set_pool(Coin::Usdc, 8_000);
    e.set_pool(Coin::Usdt, 400);
    e.begin();
    e.advance(5);
    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor, vs.floor_updated_at), (2_000, 100, T0 + GAP + 5));
}

#[test]
fn a_cycle_with_no_claims_finalizes_immediately() {
    let (mut e, _s) = world();
    e.begin();
    e.finalize();
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn a_skipped_claim_counts_as_processed_so_the_cycle_can_finish() {
    let (mut e, s) = world();
    let (w, _) = e.queue_claim(&s, 100); // no destination accounts at all
    e.set_pool(Coin::Usdc, 1_000);
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    e.finalize();
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 100, "skipped, still owed");
    assert_eq!(e.vault_state().usdc_floor, 250);
}

#[test]
fn floors_never_limit_settlement() {
    let (mut e, s) = world();
    let (w, _) = e.queue_claim(&s, 900);
    e.make_atas(&w);
    e.set_pool(Coin::Usdc, 1_000);
    e.set_vault_state(|v| {
        v.usdc_floor = 1_000; // a floor equal to the whole pool
        v.usdt_floor = 5;
    });
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    assert_eq!(e.pools(), (100, 0), "the claim was paid although that dips below the floor");
    e.finalize();
    assert_eq!(e.vault_state().usdc_floor, 25, "the floor is simply recomputed");
}

#[test]
fn anyone_may_finalize_but_must_sign() {
    let (mut e, _s) = world();
    e.begin();
    let mut ix = finalize_ix(&Pubkey::new_unique(), &e);
    ix.accounts[0].is_signer = false;
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
    assert!(e.vault_state().cycle_active);

    let stranger = e.new_caller();
    let ix = finalize_ix(&stranger.pubkey(), &e);
    let fee_payer = dup(&e.payer);
    assert_ok(e.send_with(&[ix], &fee_payer, &[&stranger]));
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn the_pool_and_vault_accounts_must_be_the_vaults_own() {
    let (mut e, _s) = world();
    e.begin();
    let payer = e.payer.pubkey();
    for (slot, bad) in [(2, e.usdt_pool), (3, e.usdc_pool), (2, e.sl8_usdc)] {
        let mut ix = finalize_ix(&payer, &e);
        ix.accounts[slot].pubkey = bad;
        assert_vault_err(&e.send(ix), VaultError::InvalidTokenAccount);
    }
    let mut ix = finalize_ix(&payer, &e);
    ix.accounts[1].pubkey = Pubkey::new_unique();
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotInitialized);
    assert!(e.vault_state().cycle_active);
}
