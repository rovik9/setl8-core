//! begin_heartbeat: opens a cycle, snapshots what is owed and available.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::errors::VaultError;
use solana_signer::Signer;

fn world() -> (Env, Sector) {
    Env::registered(&Cfg::default())
}

#[test]
fn the_gap_constant_is_five_days() {
    assert_eq!(GAP, 432_000);
    assert_eq!(GAP, 5 * DAY);
}

#[test]
fn the_first_cycle_starts_immediately_with_exact_snapshots() {
    let (mut e, s) = world();
    e.queue_claim(&s, 100);
    e.queue_claim(&s, 250);
    e.set_pool(Coin::Usdc, 700);
    e.set_pool(Coin::Usdt, 300);
    let before = e.vault_state();
    assert_eq!((before.cycle_id, before.cycle_started_at, before.cycle_active), (0, 0, false));

    e.begin(); // clock is still T0: no waiting for the first cycle
    let vs = e.vault_state();
    assert_eq!(vs.cycle_id, 1);
    assert_eq!(vs.cycle_started_at, T0);
    assert!(vs.cycle_active);
    assert_eq!(vs.cycle_owed_snapshot, 350);
    assert_eq!(vs.cycle_available_snapshot, 1_000);
    assert_eq!(vs.cycle_eligible_count, 2);
    assert_eq!(vs.cycle_processed_count, 0);
    // nothing else moved
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (2, 350));
    assert_eq!(e.pools(), (700, 300));
    e.assert_claim_invariant();
}

#[test]
fn it_works_with_zero_claims() {
    let (mut e, _s) = world();
    e.set_pool(Coin::Usdc, 5);
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active), (1, true));
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (0, 5));
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (0, 0));
}

#[test]
fn a_second_cycle_needs_the_full_gap_from_the_previous_START() {
    let (mut e, _s) = world();
    e.begin();
    e.advance(1_000);
    e.finalize(); // finishing late must not push the next start back
    e.advance(GAP - 1_000 - 1); // now = start + GAP - 1
    assert_vault_err(&e.begin_result(), VaultError::HeartbeatTooEarly);
    assert_eq!(e.vault_state().cycle_id, 1, "a rejected begin changes nothing");
    assert!(!e.vault_state().cycle_active);

    e.advance(1); // exactly start + GAP
    e.begin();
    let vs = e.vault_state();
    assert_eq!(vs.cycle_id, 2);
    assert_eq!(vs.cycle_started_at, T0 + GAP);
}

#[test]
fn it_is_rejected_while_a_cycle_is_active_even_after_the_gap() {
    let (mut e, _s) = world();
    e.begin();
    assert_vault_err(&e.begin_result(), VaultError::CycleInProgress);
    e.advance(GAP + 1);
    assert_vault_err(&e.begin_result(), VaultError::CycleInProgress);
    assert_eq!(e.vault_state().cycle_id, 1);
}

#[test]
fn each_cycle_takes_fresh_snapshots() {
    let (mut e, s) = world();
    e.queue_claim(&s, 100);
    e.set_pool(Coin::Usdc, 40);
    e.set_pool(Coin::Usdt, 0);
    e.begin();
    let first = e.vault_state();
    assert_eq!((first.cycle_owed_snapshot, first.cycle_available_snapshot, first.cycle_eligible_count), (100, 40, 1));

    // nothing is settled; skip the claim by giving no ATAs
    let w = e.open_claims()[0].1.trader_wallet;
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    e.finalize();
    e.queue_claim(&s, 900);
    e.set_pool(Coin::Usdt, 11);
    e.advance(GAP);
    e.begin();
    let second = e.vault_state();
    assert_eq!(second.cycle_id, 2);
    assert_eq!(second.cycle_started_at, T0 + GAP);
    assert_eq!(second.cycle_owed_snapshot, 1_000, "100 carried + 900 new");
    // 40 USDC + the 65 a new deposit adds to the pool (65% of 100) + 11 USDT
    assert_eq!(second.cycle_available_snapshot, 116);
    assert_eq!(second.cycle_eligible_count, 2);
    assert_eq!(second.cycle_processed_count, 0, "reset for the new cycle");
}

#[test]
fn claims_created_after_begin_do_not_join_the_cycle() {
    let (mut e, s) = world();
    e.queue_claim(&s, 100);
    e.begin();
    e.queue_claim(&s, 5_000);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_eligible_count), (100, 1));
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (2, 5_100));
}

#[test]
fn anyone_can_call_it() {
    let (mut e, _s) = world();
    let stranger = e.new_caller();
    let ix = begin_ix(&stranger.pubkey(), &e);
    let fee_payer = dup(&e.payer);
    assert_ok(e.send_with(&[ix], &fee_payer, &[&stranger]));
    assert!(e.vault_state().cycle_active);
}

#[test]
fn the_caller_must_sign() {
    let (mut e, _s) = world();
    let mut ix = begin_ix(&Pubkey::new_unique(), &e); // distinct from the fee payer
    ix.accounts[0].is_signer = false;
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn the_pool_and_vault_accounts_must_be_the_vaults_own() {
    let (mut e, _s) = world();
    let payer = e.payer.pubkey();
    // accounts: 0 caller, 1 vault_state, 2 usdc_pool, 3 usdt_pool
    for (slot, bad, want) in [
        (2, e.usdt_pool, VaultError::InvalidTokenAccount), // swapped
        (3, e.usdc_pool, VaultError::InvalidTokenAccount),
        (2, e.sl8_usdc, VaultError::InvalidTokenAccount), // a real token account that is not the pool
    ] {
        let mut ix = begin_ix(&payer, &e);
        ix.accounts[slot].pubkey = bad;
        assert_vault_err(&e.send(ix), want);
    }
    // a pool address that holds no account at all fails to load, before the address check
    let mut ix = begin_ix(&payer, &e);
    ix.accounts[2].pubkey = Pubkey::new_unique();
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotInitialized);
    let mut ix = begin_ix(&payer, &e);
    ix.accounts[1].pubkey = Pubkey::new_unique();
    assert_anchor_err(&e.send(ix), ErrorCode::AccountNotInitialized);
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn the_first_cycle_has_no_waiting_period_even_on_a_young_clock() {
    // `cycle_started_at == 0` means "never started": the min-gap rule must not apply,
    // even when the clock reads less than the gap itself.
    let (mut e, _s) = world();
    e.set_time(1_000);
    assert!(1_000 < GAP);
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_started_at), (1, 1_000));
    // and from then on the gap is enforced from that start
    e.finalize();
    e.set_time(1_000 + GAP - 1);
    assert_vault_err(&e.begin_result(), VaultError::HeartbeatTooEarly);
}
