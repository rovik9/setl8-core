//! Scheduling, pools and alerts through full keeper passes, against the real program in LiteSVM.
#![cfg(feature = "localnet")]
mod common;
use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountSerialize;
use common::*;
use core_vault::constants::{OPEN_CLAIMS_CEILING, PAUSE_PLANNED_UPGRADE, PAUSE_RECONCILIATION_DEFICIT};
use core_vault::state::{PayoutClaim, CLAIM_KIND_BOND};
use serde_json::Value;
use setl8_admin::admin_ix::AdminIx;
use setl8_admin::constants::{DEVNET_GENESIS, MAINNET_GENESIS};
use setl8_keeper::alerts::{CYCLE_OPEN_MAX_SECS, DEFAULT_MIN_PAYER_BALANCE, NO_CYCLE_MAX_SECS};
use setl8_keeper::config::Config;
use setl8_keeper::runner::PassReport;
use solana_keypair::Keypair;
use solana_signer::Signer;

// ------------------------------------------------------------------ helpers

fn kinds(r: &PassReport) -> Vec<&'static str> {
    r.alerts.iter().map(|a| a.kind).collect()
}

fn labels(r: &PassReport) -> Vec<String> {
    r.sent.iter().map(|s| s.label.clone()).collect()
}

/// Log lines of level "alert" for one kind.
fn alert_lines(e: &Env, kind: &str) -> Vec<Value> {
    e.log_lines().into_iter().filter(|l| l["level"] == "alert" && l["alert"] == kind).collect()
}

fn ata_pair(e: &Env, t: usize) -> (u64, u64) {
    let w = e.wallet(t);
    (e.balance(&ata(&w, &e.usdc)), e.balance(&ata(&w, &e.usdt)))
}

/// Rewrites a claim account in place (the claim keeps its address, owner and size).
fn edit_claim(e: &Env, addr: &Pubkey, f: impl FnOnce(&mut PayoutClaim)) {
    let mut c = e.claim(addr).unwrap();
    f(&mut c);
    let mut data = vec![];
    c.try_serialize(&mut data).unwrap();
    assert_eq!(data.len(), PayoutClaim::SPACE);
    e.set_raw(addr, data, e.keys.program_id);
}

/// Pools are set after the claims are queued (buying a challenge pays fees into them).
fn set_pools(e: &Env, usdc: u64, usdt: u64) {
    e.fill_pool(&e.usdc, usdc);
    e.fill_pool(&e.usdt, usdt);
}

fn keeper_with_balance(e: &Env, lamports: u64, cfg: Config) -> setl8_keeper::runner::Keeper<LiteChain> {
    let kp = Keypair::new();
    e.chain.svm.borrow_mut().airdrop(&kp.pubkey(), lamports).unwrap();
    e.keeper_for(kp, cfg)
}

/// Arms the chain so the issuer freezes the vault's pool for `mint` right before the next transaction lands
/// (the same state change as `Env::freeze`, made from inside the chain hook).
fn freeze_pool_on_next_send(e: &Env, mint: &Pubkey) {
    use anchor_spl::token::spl_token::{
        solana_program::program_pack::Pack,
        state::{Account, AccountState},
    };
    let pool = e.pool(mint);
    let chain = e.chain.clone();
    *e.chain.before_send.borrow_mut() = Some(Box::new(move || {
        let acc = chain.svm.borrow().get_account(&pool).unwrap();
        let mut t = Account::unpack(&acc.data).unwrap();
        t.state = AccountState::Frozen;
        let mut data = vec![0u8; Account::LEN];
        Account::pack(t, &mut data).unwrap();
        chain.svm.borrow_mut().set_account(pool, solana_account::Account { data, ..acc }).unwrap();
    }));
}

/// The first full cycle (one claim of 10 USDC, pools 1,000/1,000), run by its own keeper.
/// Returns the cycle's start time.
fn first_cycle(e: &mut Env) -> i64 {
    let usdc = e.usdc;
    e.queue_claim(&usdc, 10 * M);
    set_pools(e, 1_000 * M, 1_000 * M);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    let vs = e.vault_state();
    assert!(!vs.cycle_active);
    assert_eq!(vs.cycle_id, 1);
    vs.cycle_started_at
}

// ------------------------------------------------------------------ 1. empty queue

#[test]
fn an_empty_queue_sends_nothing_and_leaves_the_cycle_counter_alone() {
    let e = Env::new();
    e.advance(GAP + 1); // the gap is long over: only the empty queue holds the keeper back
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(e.chain.sends.get(), 0);
    assert_eq!(e.chain.simulations.get(), 0);
    assert!(r.sent.is_empty() && !r.progress && r.would_send.is_empty());
    assert!(r.idle.as_deref().unwrap().starts_with("no open claims"), "{:?}", r.idle);
    assert_eq!(r.exit_code(), 0);
    assert!(r.alerts.is_empty() && r.hard_failure.is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_started_at), (0, false, 0));
    let idle = e.logged("idle");
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0]["reason"], "no_open_claims");
    assert!(e.alert_kinds().is_empty());
}

#[test]
fn an_empty_queue_after_a_finished_cycle_stays_idle_even_long_after_the_gap() {
    let mut e = Env::new();
    let s = first_cycle(&mut e);
    let sends = e.chain.sends.get();
    e.set_time(s + 10 * GAP);
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(e.chain.sends.get(), sends, "no transaction for an empty queue");
    assert!(r.idle.is_some() && r.sent.is_empty());
    assert_eq!(r.exit_code(), 0);
    assert_eq!(e.vault_state().cycle_id, 1, "no empty cycle is burned");
}

// ------------------------------------------------------------------ 2. the 432,000 s boundary

#[test]
fn the_keeper_begins_at_exactly_start_plus_432000_seconds_and_not_one_second_before() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let s = first_cycle(&mut e);
    assert_eq!(s, T0);
    let (tb, claim_b) = e.queue_claim(&usdc, 20 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let before = ata_pair(&e, tb);
    let mut k = e.keeper();

    e.set_time(s + GAP - 1);
    let sends = e.chain.sends.get();
    let r = k.pass();
    assert_eq!(e.chain.sends.get(), sends, "nothing is sent one second early");
    assert!(r.sent.is_empty() && !r.progress);
    assert_eq!(r.idle.as_deref(), Some("next cycle may begin in 1 s"));
    assert_eq!(r.exit_code(), 0);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active), (1, false));
    assert_eq!(e.claim(&claim_b).unwrap().owed, 20 * M);
    let idle = e.logged("idle");
    assert_eq!(idle.last().unwrap()["reason"], "gap_not_over");
    assert_eq!(idle.last().unwrap()["seconds_left"], 1);

    e.set_time(s + GAP);
    let r = k.pass(); // the same keeper object: it has no memory of the earlier pass
    let l = labels(&r);
    assert!(l[0].starts_with("reconcile_product"), "{l:?}");
    assert_eq!(l[1..], ["begin_heartbeat", "settle_claims x1", "finalize_heartbeat"]);
    assert_eq!(r.exit_code(), 0);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_started_at), (2, false, s + GAP));
    assert!(e.claim(&claim_b).is_none(), "paid in full and closed");
    let after = ata_pair(&e, tb);
    assert_eq!((after.0 - before.0) + (after.1 - before.1), 20 * M);
}

#[test]
fn the_gap_is_measured_from_the_start_of_the_last_cycle_not_from_its_end() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let s = first_cycle(&mut e);
    // the first cycle was finalized at S (same second); a later finalize time would not matter either:
    // queue a claim, wait 1 day, check the keeper still counts from S
    let (_t, c) = e.queue_claim(&usdc, 5 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    e.set_time(s + DAY);
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(r.idle.as_deref(), Some(format!("next cycle may begin in {} s", GAP - DAY).as_str()));
    assert_eq!(e.claim(&c).unwrap().owed, 5 * M);
}

#[test]
fn the_gap_runs_from_the_start_of_a_cycle_that_finished_hours_later_not_from_its_finalize() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, first) = e.queue_claim(&usdc, 10 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    begun_but_not_settled(&e); // cycle 1 begins at T0
    assert_eq!(e.vault_state().cycle_started_at, T0);
    // the cycle is only finished 20 hours later (inside the 24 h limit, so no alert)
    e.set_time(T0 + 20 * 3600);
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(labels(&r), vec!["settle_claims x1", "finalize_heartbeat"]);
    assert_eq!(r.exit_code(), 0);
    assert!(e.claim(&first).is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_started_at), (1, false, T0));

    let (tb, second) = e.queue_claim(&usdc, 20 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let before = ata_pair(&e, tb);
    // 432,000 s after the START (T0), which is 432,000 - 72,000 s after the finalize
    e.set_time(T0 + GAP - 1);
    let r = k.pass();
    assert!(r.sent.is_empty());
    assert_eq!(r.idle.as_deref(), Some("next cycle may begin in 1 s"));
    e.set_time(T0 + GAP);
    let r = k.pass();
    assert_eq!(labels(&r)[1..], ["begin_heartbeat", "settle_claims x1", "finalize_heartbeat"]);
    assert!(e.claim(&second).is_none());
    let after = ata_pair(&e, tb);
    assert_eq!((after.0 - before.0) + (after.1 - before.1), 20 * M);
    assert_eq!(e.vault_state().cycle_started_at, T0 + GAP);
}

// ------------------------------------------------------------------ 3. frozen USDC before begin

#[test]
fn a_frozen_usdc_pool_before_begin_is_never_touched_and_usdt_pays_pro_rata() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let (ta, ca) = e.queue_claim(&usdc, 60 * M);
    let (tb, cb) = e.queue_claim(&usdt, 30 * M);
    set_pools(&e, 1_000 * M, 40 * M);
    e.freeze(&usdc, true);
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);

    // begin snapshots a frozen pool as empty: available 40 of 90 owed
    let vs = e.vault_state();
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (90 * M, 40 * M));
    assert_eq!((vs.cycle_id, vs.cycle_active), (1, false));
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (2, 2));

    // 60 * 40/90 = 26,666,666.67 and 30 * 40/90 = 13,333,333.33, rounded down, all from USDT
    let after = (ata_pair(&e, ta), ata_pair(&e, tb));
    assert_eq!(after.0 .0 - before.0 .0, 0, "nothing from the frozen USDC pool");
    assert_eq!(after.1 .0 - before.1 .0, 0);
    assert_eq!(after.0 .1 - before.0 .1, 26_666_666);
    assert_eq!(after.1 .1 - before.1 .1, 13_333_333);
    assert_eq!((e.balance(&e.pool(&usdc)), e.balance(&e.pool(&usdt))), (1_000 * M, 1));
    // the planner agrees (cross-check against the program's own function)
    assert_eq!(expected_payment(&e, 60 * M), (0, 1), "live pool balance is only 1 now; the cycle ratio is what paid");
    // carry-over: nothing is closed, the unpaid remainder stays owed
    assert_eq!(e.claim(&ca).unwrap().owed, 60 * M - 26_666_666);
    assert_eq!(e.claim(&cb).unwrap().owed, 30 * M - 13_333_333);
    assert_eq!(e.claim(&ca).unwrap().last_settled_cycle, 1);
    assert_eq!(vs.open_claims_count, 2);
    assert_eq!(vs.open_claims_total, 90 * M - 26_666_666 - 13_333_333);

    // alerts: in the pass report AND in the log, once each
    assert_eq!(kinds(&r), vec!["pool_frozen", "coverage_below_one"]);
    assert_eq!(r.alerts[0].key, "usdc");
    assert_eq!(r.exit_code(), 10);
    let frozen = alert_lines(&e, "pool_frozen");
    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen[0]["key"], "usdc");
    assert_eq!(frozen[0]["pool"], "usdc");
    assert_eq!(frozen[0]["balance"], 1_000 * M);
    assert_eq!(frozen[0]["cluster"], "localnet");
    assert_eq!(e.alert_kinds(), vec!["pool_frozen", "coverage_below_one"]);
}

#[test]
fn a_frozen_usdt_pool_is_reported_under_its_own_key_and_usdc_pays() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (t, c) = e.queue_claim(&usdc, 25 * M);
    set_pools(&e, 100 * M, 1_000 * M);
    e.freeze(&e.usdt.clone(), true);
    let before = ata_pair(&e, t);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none());
    let after = ata_pair(&e, t);
    assert_eq!((after.0 - before.0, after.1 - before.1), (25 * M, 0), "USDC pays although USDT is larger");
    assert!(e.claim(&c).is_none());
    assert_eq!(e.balance(&e.pool(&e.usdt)), 1_000 * M);
    assert_eq!(kinds(&r), vec!["pool_frozen"]);
    assert_eq!(r.alerts[0].key, "usdt");
    assert_eq!(r.exit_code(), 10);
}

// ------------------------------------------------------------------ 4. frozen between begin and settle

/// The keeper begins the cycle and is stopped by its send cap right before the first settle.
fn begun_but_not_settled(e: &Env) {
    let mut k = e.keeper_with(Config { max_sends: 2, ..Config::default() });
    let r = k.pass();
    let l = labels(&r);
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(l[0].starts_with("reconcile_product") && l[1] == "begin_heartbeat");
    assert_eq!(r.hard_failure.as_deref(), Some("send cap reached (max-sends-per-run)"));
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_processed_count), (1, true, 0));
}

#[test]
fn a_pool_frozen_after_begin_but_before_the_first_settle_still_lets_the_cycle_complete_from_the_other_pool() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let (ta, ca) = e.queue_claim(&usdc, 30 * M);
    let (tb, cb) = e.queue_claim(&usdt, 20 * M);
    set_pools(&e, 1_000 * M, 100 * M);
    begun_but_not_settled(&e);
    assert_eq!(e.vault_state().cycle_available_snapshot, 1_100 * M, "the snapshot was taken with both pools usable");
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));

    // the issuer freezes USDC exactly when the keeper sends its first settle
    freeze_pool_on_next_send(&e, &usdc);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    assert_eq!(labels(&r), vec!["settle_claims x2", "finalize_heartbeat"]);

    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (false, 2, 2));
    let after = (ata_pair(&e, ta), ata_pair(&e, tb));
    // ratio 1: both paid in full, from USDT because USDC is frozen now (USDC is the larger pool, so it
    // would have paid had it been usable)
    assert_eq!((after.0 .0 - before.0 .0, after.0 .1 - before.0 .1), (0, 30 * M));
    assert_eq!((after.1 .0 - before.1 .0, after.1 .1 - before.1 .1), (0, 20 * M));
    assert_eq!((e.balance(&e.pool(&usdc)), e.balance(&e.pool(&usdt))), (1_000 * M, 50 * M));
    assert!(e.claim(&ca).is_none() && e.claim(&cb).is_none());
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (0, 0));

    // the alert comes from the end-of-pass re-evaluation: the pass started with both pools usable
    assert_eq!(kinds(&r), vec!["pool_frozen"]);
    assert_eq!(r.alerts[0].key, "usdc");
    assert_eq!(r.exit_code(), 10);
    assert_eq!(alert_lines(&e, "pool_frozen").len(), 1);
    let log = e.log_lines();
    let alert_at = log.iter().position(|l| l["alert"] == "pool_frozen").unwrap();
    let last_sent = log.iter().rposition(|l| l["event"] == "sent").unwrap();
    assert!(alert_at > last_sent, "raised after the sends, not before them");
}

#[test]
fn a_pool_frozen_mid_cycle_caps_payments_at_the_remaining_usdt_in_the_order_the_keeper_settles() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let (ta, ca) = e.queue_claim(&usdc, 60 * M);
    let (tb, cb) = e.queue_claim(&usdt, 40 * M);
    set_pools(&e, 1_000 * M, 50 * M);
    begun_but_not_settled(&e);
    freeze_pool_on_next_send(&e, &usdc);
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    // the keeper settles in claim-address order; each payment is min(owed, what USDT still holds)
    let mut order = vec![(ca, 60 * M, ta), (cb, 40 * M, tb)];
    order.sort_by_key(|(c, _, _)| c.to_bytes());
    let mut left = 50 * M;
    for (claim, owed, t) in order {
        let pay = owed.min(left);
        left -= pay;
        let b = if t == ta { before.0 } else { before.1 };
        let a = ata_pair(&e, t);
        assert_eq!((a.0 - b.0, a.1 - b.1), (0, pay));
        match e.claim(&claim) {
            Some(c) => assert_eq!(c.owed, owed - pay),
            None => assert_eq!(pay, owed),
        }
    }
    assert_eq!(left, 0, "model: all 50 USDT paid out");
    assert_eq!(e.balance(&e.pool(&usdt)), 0);
    assert_eq!(e.balance(&e.pool(&usdc)), 1_000 * M);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count), (false, 2));
    assert_eq!(vs.open_claims_total, 50 * M, "100 owed, 50 paid");
    assert_eq!(kinds(&r), vec!["pool_frozen", "coverage_below_one"]);
    assert_eq!(r.exit_code(), 10);
}

// ------------------------------------------------------------------ 5. both pools frozen

#[test]
fn with_both_pools_frozen_claims_are_processed_with_zero_pay_and_the_cycle_still_finalizes() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let (ta, ca) = e.queue_claim(&usdc, 25 * M);
    let (tb, cb) = e.queue_claim(&usdt, 35 * M);
    set_pools(&e, 500 * M, 500 * M);
    e.freeze(&usdc, true);
    e.freeze(&usdt, true);
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    let l = labels(&r);
    assert_eq!(l[1..], ["begin_heartbeat", "settle_claims x2", "finalize_heartbeat"]);

    let vs = e.vault_state();
    assert!(!vs.cycle_active);
    assert_eq!((vs.cycle_processed_count, vs.cycle_eligible_count), (2, 2));
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (60 * M, 0));
    // carry-overs remain owed in full
    for (c, owed) in [(ca, 25 * M), (cb, 35 * M)] {
        let claim = e.claim(&c).unwrap();
        assert_eq!((claim.owed, claim.last_settled_cycle), (owed, 1));
    }
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (2, 60 * M));
    assert_eq!((ata_pair(&e, ta), ata_pair(&e, tb)), before, "not a base unit moved");
    assert_eq!((e.balance(&e.pool(&usdc)), e.balance(&e.pool(&usdt))), (500 * M, 500 * M));

    assert_eq!(kinds(&r), vec!["pool_frozen", "pool_frozen", "coverage_below_one"]);
    assert_eq!((r.alerts[0].key.as_str(), r.alerts[1].key.as_str()), ("usdc", "usdt"));
    assert_eq!(r.exit_code(), 10);
    let cov = alert_lines(&e, "coverage_below_one");
    assert_eq!(cov.len(), 1);
    assert_eq!(cov[0]["spendable_pools"], "0");
    assert_eq!(cov[0]["open_claims_total"], 60 * M);
    assert_eq!(cov[0]["ratio"], "0.000000");
    assert_eq!(alert_lines(&e, "pool_frozen").len(), 2);

    // the next pass: nothing to begin (gap), the alerts keep coming
    let r2 = k.pass();
    assert!(r2.sent.is_empty());
    assert!(r2.idle.as_deref().unwrap().starts_with("next cycle may begin in"));
    assert_eq!(kinds(&r2), vec!["pool_frozen", "pool_frozen", "coverage_below_one"]);
    assert_eq!(r2.exit_code(), 10);
}

// ------------------------------------------------------------------ 6. a wrong tally

#[test]
fn a_wrong_tally_pauses_the_product_with_reason_2_and_the_queued_claims_still_settle_in_the_same_pass() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (ta, ca) = e.queue_claim(&usdc, 12 * M);
    let (tb, cb) = e.queue_claim(&usdc, 8 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    e.set_tally(2, 20 * M + 1); // the vault booked 2 requests for 20,000,000; the sector says one base unit more
    assert!(e.registry(&e.sector).active);
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);

    let reg = e.registry(&e.sector);
    assert!(!reg.active);
    assert_eq!(reg.pause_reason, PAUSE_RECONCILIATION_DEFICIT);
    assert_eq!(reg.pause_reason, 2);

    let l = labels(&r);
    assert_eq!(l[0], format!("reconcile_product {}", e.sector));
    assert_eq!(l[1..], ["begin_heartbeat", "settle_claims x2", "finalize_heartbeat"]);
    let a = alert_lines(&e, "product_auto_paused");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["reason_code"], 2);
    assert_eq!(a[0]["key"], e.sector.to_string());
    assert_eq!(a[0]["product"], e.sector.to_string());
    assert_eq!(kinds(&r), vec!["product_auto_paused"]);
    assert_eq!(r.exit_code(), 10);

    // the claims were paid in full in the same pass
    assert!(e.claim(&ca).is_none() && e.claim(&cb).is_none());
    let after = (ata_pair(&e, ta), ata_pair(&e, tb));
    assert_eq!(after.0 .0 - before.0 .0 + after.0 .1 - before.0 .1, 12 * M);
    assert_eq!(after.1 .0 - before.1 .0 + after.1 .1 - before.1 .1, 8 * M);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.open_claims_count, vs.open_claims_total), (1, false, 0, 0));
}

#[test]
fn an_auto_paused_product_keeps_raising_the_alert_on_every_later_pass_and_is_not_reconciled_again() {
    let mut e = Env::new();
    let usdc = e.usdc;
    e.queue_claim(&usdc, 12 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    e.set_tally(1, 1);
    let mut k = e.keeper();
    k.pass();
    assert_eq!(e.registry(&e.sector).pause_reason, 2);

    let sends = e.chain.sends.get();
    let r = k.pass(); // idle: no claims
    assert_eq!(e.chain.sends.get(), sends);
    assert_eq!(kinds(&r), vec!["product_auto_paused"]);
    assert_eq!(r.exit_code(), 10);

    // a new claim cannot be queued (paused product), so only the idle path exists; with a fresh keeper
    // the alert is the same: nothing is remembered between passes
    let mut k2 = e.keeper();
    let r2 = k2.pass();
    assert_eq!(kinds(&r2), vec!["product_auto_paused"]);
    assert_eq!(alert_lines(&e, "product_auto_paused").len(), 3);
}

#[test]
fn a_matching_tally_is_not_paused_and_raises_nothing() {
    let mut e = Env::new();
    let usdc = e.usdc;
    e.queue_claim(&usdc, 12 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let mut k = e.keeper();
    let r = k.pass();
    let reg = e.registry(&e.sector);
    assert!(reg.active);
    assert_eq!(reg.pause_reason, 0);
    assert!(r.alerts.is_empty() && e.alert_kinds().is_empty());
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r)[0], format!("reconcile_product {}", e.sector));
}

// ------------------------------------------------------------------ 7. a product paused by the admins

#[test]
fn a_product_paused_by_the_admins_is_not_reconciled_not_alerted_as_auto_paused_and_claims_still_settle() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (t, c) = e.queue_claim(&usdc, 15 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let sector = e.sector;
    e.send_admin(AdminIx::PauseProduct { product: sector }.build(&e.keys));
    let reg = e.registry(&sector);
    assert!(!reg.active);
    assert_eq!(reg.pause_reason, PAUSE_PLANNED_UPGRADE);
    assert_eq!(reg.pause_reason, 1);
    // a deliberately wrong tally: a (wrong) reconcile would change the reason to 2
    e.set_tally(99, 99);
    let before = ata_pair(&e, t);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);

    assert_eq!(labels(&r), vec!["begin_heartbeat", "settle_claims x1", "finalize_heartbeat"]);
    assert_eq!(e.registry(&sector).pause_reason, 1, "the admins' reason is left alone");
    let skipped = e.logged("reconcile_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["product"], sector.to_string());
    assert_eq!(skipped[0]["pause_reason_code"], 1);
    assert!(alert_lines(&e, "product_auto_paused").is_empty());
    assert!(e.logged("lost_race").is_empty());
    assert_eq!(e.chain.simulations.get(), e.chain.sends.get(), "no reconcile was even simulated");
    assert!(r.alerts.is_empty());
    assert_eq!(r.exit_code(), 0);
    assert!(e.claim(&c).is_none());
    let after = ata_pair(&e, t);
    assert_eq!((after.0 - before.0) + (after.1 - before.1), 15 * M);
}

// ------------------------------------------------------------------ 8. ceiling alerts

const EIGHTY: u64 = OPEN_CLAIMS_CEILING / 5 * 4;

/// A vault with no claims whose books say `total` is owed, pools big enough that coverage is not the alert.
fn ceiling_env(total: u64) -> Env {
    let e = Env::new();
    e.set_vault_state(|v| v.open_claims_total = total);
    set_pools(&e, 2_000_000_000_000, 2_000_000_000_000);
    e
}

#[test]
fn the_ceiling_constants_are_what_the_boundaries_below_assume() {
    assert_eq!(OPEN_CLAIMS_CEILING, 2_500_000_000_000);
    assert_eq!(EIGHTY, 2_000_000_000_000);
}

#[test]
fn exactly_80_percent_of_the_ceiling_raises_no_ceiling_alert() {
    let e = ceiling_env(EIGHTY);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.alerts.is_empty(), "{:?}", kinds(&r));
    assert_eq!(r.exit_code(), 0);
    assert!(alert_lines(&e, "claims_near_ceiling").is_empty());
}

#[test]
fn one_base_unit_above_80_percent_raises_the_ceiling_alert() {
    let e = ceiling_env(EIGHTY + 1);
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(kinds(&r), vec!["claims_near_ceiling"]);
    assert_eq!(r.exit_code(), 10);
    let a = alert_lines(&e, "claims_near_ceiling");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["key"], "ceiling");
    assert_eq!(a[0]["open_claims_total"], EIGHTY + 1);
    assert_eq!(a[0]["ceiling"], OPEN_CLAIMS_CEILING);
    assert_eq!(a[0]["percent"], 80);
    assert_eq!(a[0]["at_ceiling"], false);
}

#[test]
fn one_base_unit_below_80_percent_raises_no_ceiling_alert() {
    let e = ceiling_env(EIGHTY - 1);
    let mut k = e.keeper();
    assert!(k.pass().alerts.is_empty());
}

#[test]
fn at_100_percent_of_the_ceiling_the_alert_says_at_ceiling_true() {
    let e = ceiling_env(OPEN_CLAIMS_CEILING);
    let mut k = e.keeper();
    let r = k.pass();
    // pools of 2,000 + 2,000 million dollars cover the 2,500 million owed: only the ceiling alert fires
    assert_eq!(kinds(&r), vec!["claims_near_ceiling"]);
    let a = alert_lines(&e, "claims_near_ceiling");
    assert_eq!(a[0]["percent"], 100);
    assert_eq!(a[0]["at_ceiling"], true);
    assert_eq!(a[0]["open_claims_total"], OPEN_CLAIMS_CEILING);
}

#[test]
fn the_read_only_status_report_covers_the_same_three_ceiling_boundaries_and_sends_nothing() {
    let e = ceiling_env(EIGHTY);
    let mut k = e.keeper();
    let got = |k: &mut setl8_keeper::runner::Keeper<LiteChain>| -> Vec<String> {
        k.status_report().unwrap()["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(got(&mut k).is_empty());
    e.set_vault_state(|v| v.open_claims_total = EIGHTY + 1);
    assert_eq!(got(&mut k), vec!["claims_near_ceiling"]);
    e.set_vault_state(|v| v.open_claims_total = OPEN_CLAIMS_CEILING);
    assert_eq!(got(&mut k), vec!["claims_near_ceiling"]);
    assert_eq!(alert_lines(&e, "claims_near_ceiling").last().unwrap()["at_ceiling"], true);
    assert_eq!((e.chain.sends.get(), e.chain.simulations.get()), (0, 0));
}

// ------------------------------------------------------------------ 9. coverage

fn two_claims_100() -> Env {
    let mut e = Env::new();
    let usdc = e.usdc;
    e.queue_claim(&usdc, 60 * M);
    e.queue_claim(&usdc, 40 * M);
    e
}

fn status_alert_kinds(k: &mut setl8_keeper::runner::Keeper<LiteChain>) -> Vec<String> {
    k.status_report().unwrap()["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn pools_smaller_than_the_open_claims_raise_coverage_below_one_with_the_ratio() {
    let e = two_claims_100();
    set_pools(&e, 30 * M, 40 * M);
    let mut k = e.keeper();
    assert_eq!(status_alert_kinds(&mut k), vec!["coverage_below_one"]);
    let a = alert_lines(&e, "coverage_below_one");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["key"], "coverage");
    assert_eq!(a[0]["ratio"], "0.700000");
    assert_eq!(a[0]["spendable_pools"], "70000000");
    assert_eq!(a[0]["open_claims_total"], 100 * M);
    let s = k.status_report().unwrap();
    assert_eq!(s["coverage_ratio"], "0.700000");
}

#[test]
fn pools_one_base_unit_short_of_the_open_claims_raise_the_alert() {
    let e = two_claims_100();
    set_pools(&e, 60 * M, 40 * M - 1);
    let mut k = e.keeper();
    assert_eq!(status_alert_kinds(&mut k), vec!["coverage_below_one"]);
    let a = alert_lines(&e, "coverage_below_one");
    assert_eq!(a[0]["spendable_pools"], "99999999");
}

#[test]
fn pools_exactly_equal_to_the_open_claims_raise_no_coverage_alert() {
    let e = two_claims_100();
    set_pools(&e, 60 * M, 40 * M);
    let mut k = e.keeper();
    assert!(status_alert_kinds(&mut k).is_empty());
    assert!(alert_lines(&e, "coverage_below_one").is_empty());
    assert_eq!(k.status_report().unwrap()["coverage_ratio"], "1.000000");
}

#[test]
fn pools_larger_than_the_open_claims_raise_no_coverage_alert() {
    let e = two_claims_100();
    set_pools(&e, 100 * M, 100 * M);
    let mut k = e.keeper();
    assert!(status_alert_kinds(&mut k).is_empty());
}

#[test]
fn a_frozen_pool_does_not_count_towards_coverage() {
    let e = two_claims_100();
    set_pools(&e, 100 * M, 99 * M);
    e.freeze(&e.usdc.clone(), true);
    let mut k = e.keeper();
    assert_eq!(status_alert_kinds(&mut k), vec!["pool_frozen", "coverage_below_one"]);
    assert_eq!(alert_lines(&e, "coverage_below_one")[0]["spendable_pools"], "99000000");
}

#[test]
fn a_full_pass_with_pools_short_of_the_claims_pays_pro_rata_and_alerts_on_the_remaining_shortfall() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (ta, _) = e.queue_claim(&usdc, 60 * M);
    let (tb, _) = e.queue_claim(&usdc, 40 * M);
    set_pools(&e, 30 * M, 40 * M);
    let before = (ata_pair(&e, ta), ata_pair(&e, tb));
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (100 * M, 70 * M));
    let after = (ata_pair(&e, ta), ata_pair(&e, tb));
    let got = |b: (u64, u64), a: (u64, u64)| (a.0 - b.0) + (a.1 - b.1);
    assert_eq!(got(before.0, after.0), 42 * M, "60 * 70/100");
    assert_eq!(got(before.1, after.1), 28 * M, "40 * 70/100");
    assert_eq!((e.balance(&e.pool(&usdc)), e.balance(&e.pool(&e.usdt))), (0, 0));
    assert_eq!(vs.open_claims_total, 30 * M);
    assert_eq!(kinds(&r), vec!["coverage_below_one"]);
    assert_eq!(alert_lines(&e, "coverage_below_one")[0]["ratio"], "0.700000");
}

// ------------------------------------------------------------------ 10. fee payer balance

#[test]
fn a_payer_below_the_default_threshold_raises_payer_balance_low() {
    let e = Env::new();
    assert_eq!(Config::default().min_payer_balance, 50_000_000);
    assert_eq!(DEFAULT_MIN_PAYER_BALANCE, 50_000_000);
    let mut k = keeper_with_balance(&e, 49_999_999, Config::default());
    let r = k.pass();
    assert_eq!(kinds(&r), vec!["payer_balance_low"]);
    assert_eq!(r.alerts[0].key, "payer");
    assert_eq!(r.exit_code(), 10);
    let a = alert_lines(&e, "payer_balance_low");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["key"], "payer");
    assert_eq!(a[0]["balance_lamports"], 49_999_999);
    assert_eq!(a[0]["threshold_lamports"], 50_000_000);
}

#[test]
fn a_payer_holding_exactly_the_threshold_raises_no_alert() {
    let e = Env::new();
    let mut k = keeper_with_balance(&e, 50_000_000, Config::default());
    let r = k.pass();
    assert!(r.alerts.is_empty());
    assert_eq!(r.exit_code(), 0);
    assert!(alert_lines(&e, "payer_balance_low").is_empty());
}

#[test]
fn a_custom_threshold_from_the_config_is_honoured_to_the_lamport() {
    let e = Env::new();
    let cfg = Config { min_payer_balance: 1_234_567, ..Config::default() };
    let mut low = keeper_with_balance(&e, 1_234_566, cfg.clone());
    assert_eq!(kinds(&low.pass()), vec!["payer_balance_low"]);
    let mut at = keeper_with_balance(&e, 1_234_567, cfg.clone());
    assert!(at.pass().alerts.is_empty());
    let mut above = keeper_with_balance(&e, 1_234_568, cfg);
    assert!(above.pass().alerts.is_empty());
    let a = alert_lines(&e, "payer_balance_low");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["threshold_lamports"], 1_234_567);
}

#[test]
fn a_keeper_with_a_low_balance_still_completes_the_cycle_and_exits_10() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, c) = e.queue_claim(&usdc, 9 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let mut k = keeper_with_balance(&e, 20_000_000, Config::default());
    let r = k.pass();
    assert!(r.hard_failure.is_none());
    assert!(e.claim(&c).is_none());
    assert_eq!(kinds(&r), vec!["payer_balance_low"]);
    assert_eq!(r.exit_code(), 10);
    assert_eq!(alert_lines(&e, "payer_balance_low")[0]["balance_lamports"], 20_000_000, "judged on the start state");
}

// ------------------------------------------------------------------ 11. cycle open too long / no cycle for too long

#[test]
fn a_cycle_open_for_exactly_24_hours_is_not_an_alert_and_the_keeper_finishes_it() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, c) = e.queue_claim(&usdc, 10 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    begun_but_not_settled(&e);
    let s = e.vault_state().cycle_started_at;
    assert_eq!(CYCLE_OPEN_MAX_SECS, 86_400);
    e.set_time(s + 86_400);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.alerts.is_empty(), "{:?}", kinds(&r));
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r), vec!["settle_claims x1", "finalize_heartbeat"]);
    assert!(e.claim(&c).is_none());
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn a_cycle_open_for_24_hours_and_one_second_raises_cycle_open_too_long_while_the_keeper_continues_it() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, c) = e.queue_claim(&usdc, 10 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    begun_but_not_settled(&e);
    let s = e.vault_state().cycle_started_at;
    e.set_time(s + 86_401);
    let mut k = e.keeper();
    let r = k.pass();
    assert_eq!(kinds(&r), vec!["cycle_open_too_long"]);
    assert_eq!(r.alerts[0].key, "1");
    assert_eq!(r.exit_code(), 10);
    // it did not stop at the alert: no second begin, the same cycle is settled and finalized
    assert_eq!(labels(&r), vec!["settle_claims x1", "finalize_heartbeat"]);
    assert!(e.claim(&c).is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active), (1, false));
    let a = alert_lines(&e, "cycle_open_too_long");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["cycle_id"], 1);
    assert_eq!(a[0]["open_secs"], 86_401);
    assert_eq!(a[0]["processed"], 0);
    assert_eq!(a[0]["eligible"], 1);
    // raised from the start state, before the first transaction of this pass
    let log = e.log_lines();
    let alert_at = log.iter().position(|l| l["alert"] == "cycle_open_too_long").unwrap();
    let sent_at = log.iter().rposition(|l| l["event"] == "sent" && l["label"] == "settle_claims x1").unwrap();
    assert!(alert_at < sent_at);
}

/// First cycle at S, a second claim queued, then a pass at S + `delta`.
fn second_cycle_pass_at(delta: i64) -> (Env, PassReport, i64, Pubkey) {
    let mut e = Env::new();
    let usdc = e.usdc;
    let s = first_cycle(&mut e);
    let (_, c) = e.queue_claim(&usdc, 7 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    e.set_time(s + delta);
    let mut k = e.keeper();
    let r = k.pass();
    (e, r, s, c)
}

#[test]
fn six_days_without_a_cycle_is_not_yet_an_alert_and_the_keeper_begins() {
    assert_eq!(NO_CYCLE_MAX_SECS, 518_400);
    let (e, r, s, c) = second_cycle_pass_at(518_400);
    assert!(r.alerts.is_empty(), "{:?}", kinds(&r));
    assert_eq!(r.exit_code(), 0);
    assert!(r.hard_failure.is_none());
    assert!(e.claim(&c).is_none());
    assert_eq!(e.vault_state().cycle_started_at, s + 518_400);
    assert_eq!(e.vault_state().cycle_id, 2);
}

#[test]
fn six_days_and_one_second_without_a_cycle_raises_no_cycle_for_too_long_judged_on_the_pass_start_state() {
    let (e, r, s, c) = second_cycle_pass_at(518_401);
    assert_eq!(kinds(&r), vec!["no_cycle_for_too_long"]);
    assert_eq!(r.alerts[0].key, "idle");
    assert_eq!(r.exit_code(), 10);
    // the keeper fixed the situation in the same pass
    assert!(e.claim(&c).is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_started_at), (2, false, s + 518_401));
    let a = alert_lines(&e, "no_cycle_for_too_long");
    assert_eq!(a.len(), 1, "not repeated by the end-of-pass re-evaluation");
    assert_eq!(a[0]["secs_since_last_cycle_started"], 518_401);
    assert_eq!(a[0]["open_claims"], 1);
    // raised before the first transaction of the pass
    let log = e.log_lines();
    let alert_at = log.iter().position(|l| l["alert"] == "no_cycle_for_too_long").unwrap();
    let first_sent = log.iter().position(|l| l["event"] == "sent" && l["label"] != "begin_heartbeat").unwrap();
    // the first pass's sends come earlier in the log: find this pass's by looking after the first cycle's
    let this_pass_first_sent = log.iter().enumerate().skip(alert_at).find(|(_, l)| l["event"] == "sent").unwrap().0;
    assert!(alert_at < this_pass_first_sent);
    assert!(first_sent < alert_at, "the first cycle's own sends precede it");
}

#[test]
fn the_no_cycle_alert_needs_open_claims_an_idle_empty_vault_never_raises_it() {
    let mut e = Env::new();
    let s = first_cycle(&mut e);
    e.set_time(s + 30 * DAY);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.alerts.is_empty());
    assert_eq!(r.exit_code(), 0);
}

// ------------------------------------------------------------------ 12. a claim skipped through 3 cycles

#[test]
fn status_report_flags_a_claim_skipped_three_cycles_with_a_missing_ata_and_not_one_with_usable_atas() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let (t_bad, bad) = e.queue_claim(&usdc, 10 * M);
    let (_t_ok, ok) = e.queue_claim(&usdc, 11 * M);
    let (t_two, two) = e.queue_claim(&usdc, 12 * M);
    let (t_frozen, frozen) = e.queue_claim(&usdc, 13 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    // all four: created in cycle 0; last processed in cycle 3 (= created + 3)
    for c in [&bad, &ok, &frozen] {
        edit_claim(&e, c, |c| c.last_settled_cycle = 3);
    }
    edit_claim(&e, &two, |c| c.last_settled_cycle = 2); // only two cycles: below the threshold
    e.delete_account(&ata(&e.wallet(t_bad), &usdc));
    e.delete_account(&ata(&e.wallet(t_two), &usdt));
    e.freeze_ata(&e.wallet(t_frozen), &usdt);

    let mut k = e.keeper();
    let rep = k.status_report().unwrap();
    let alerts = rep["alerts"].as_array().unwrap();
    let mut flagged: Vec<String> = alerts
        .iter()
        .filter(|a| a["kind"] == "claim_skipped_repeatedly")
        .map(|a| a["key"].as_str().unwrap().to_string())
        .collect();
    flagged.sort();
    let mut want = vec![bad.to_string(), frozen.to_string()];
    want.sort();
    assert_eq!(flagged, want, "missing and frozen ATAs after 3 cycles; not the usable one, not the 2-cycle one");
    let lines = alert_lines(&e, "claim_skipped_repeatedly");
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|l| l["cycles"] == 3 && l["key"] == l["claim"]));
}

#[test]
fn a_pass_during_an_active_cycle_flags_the_repeatedly_skipped_claim_and_still_completes_the_cycle() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (t_bad, bad) = e.queue_claim(&usdc, 10 * M);
    let (t_ok, ok) = e.queue_claim(&usdc, 11 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    for c in [&bad, &ok] {
        edit_claim(&e, c, |c| c.last_settled_cycle = 3);
    }
    e.delete_account(&ata(&e.wallet(t_bad), &usdc));
    begun_but_not_settled(&e);
    let before_ok = ata_pair(&e, t_ok);

    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    assert_eq!(kinds(&r), vec!["claim_skipped_repeatedly"]);
    assert_eq!(r.alerts[0].key, bad.to_string());
    assert_eq!(r.exit_code(), 10);
    assert_eq!(labels(&r), vec!["settle_claims x2", "finalize_heartbeat"]);

    // the usable claim was paid and closed; the skipped one stays owed in full, counted as processed
    assert!(e.claim(&ok).is_none());
    let a = ata_pair(&e, t_ok);
    assert_eq!((a.0 - before_ok.0) + (a.1 - before_ok.1), 11 * M);
    let c = e.claim(&bad).unwrap();
    assert_eq!((c.owed, c.last_settled_cycle), (10 * M, 1));
    let vs = e.vault_state();
    assert_eq!(
        (vs.cycle_active, vs.cycle_processed_count, vs.open_claims_count, vs.open_claims_total),
        (false, 2, 1, 10 * M)
    );
    // logged once by the capped first pass (it read the claims before its send cap hit) and once by this pass
    assert_eq!(alert_lines(&e, "claim_skipped_repeatedly").len(), 2);
}

#[test]
fn a_claim_with_usable_atas_and_the_same_cycle_numbers_does_not_alert_during_a_pass() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, ok) = e.queue_claim(&usdc, 11 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    edit_claim(&e, &ok, |c| c.last_settled_cycle = 3);
    begun_but_not_settled(&e);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.alerts.is_empty(), "{:?}", kinds(&r));
    assert_eq!(r.exit_code(), 0);
    assert!(e.claim(&ok).is_none());
}

// ------------------------------------------------------------------ 13. genesis mismatch mid-run

#[test]
fn a_genesis_hash_that_changes_between_passes_stops_the_next_pass_before_anything_is_sent() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let s = first_cycle(&mut e);
    let (_, c) = e.queue_claim(&usdc, 21 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    e.set_time(s + GAP);
    let mut k = e.keeper();
    let (sends, sims) = (e.chain.sends.get(), e.chain.simulations.get());

    *e.chain.genesis.borrow_mut() = DEVNET_GENESIS.to_string();
    let r = k.pass();
    let f = r.hard_failure.clone().unwrap();
    assert_eq!(
        f,
        format!("refused: --cluster localnet but the node is on a public cluster (genesis {DEVNET_GENESIS})")
    );
    assert_eq!(r.exit_code(), 20);
    assert!(r.sent.is_empty() && !r.progress && r.would_send.is_empty());
    assert_eq!((e.chain.sends.get(), e.chain.simulations.get()), (sends, sims), "nothing sent or simulated");
    assert_eq!(e.claim(&c).unwrap().owed, 21 * M);
    assert_eq!(e.vault_state().cycle_id, 1);
    let a = alert_lines(&e, "genesis_mismatch");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["key"], "genesis");
    assert_eq!(a[0]["genesis"], DEVNET_GENESIS);
    assert_eq!(a[0]["cluster"], "localnet");

    // mainnet's hash is refused the same way
    *e.chain.genesis.borrow_mut() = MAINNET_GENESIS.to_string();
    let r = k.pass();
    assert_eq!(r.exit_code(), 20);
    assert!(r.hard_failure.unwrap().contains(MAINNET_GENESIS));
    assert_eq!(e.chain.sends.get(), sends);
    assert_eq!(alert_lines(&e, "genesis_mismatch").len(), 2);

    // and the keeper, which remembers nothing, carries on once the node is itself again
    *e.chain.genesis.borrow_mut() = LOCAL_GENESIS.to_string();
    let r = k.pass();
    assert!(r.hard_failure.is_none());
    assert!(e.claim(&c).is_none());
    assert_eq!(e.vault_state().cycle_id, 2);
}

#[test]
fn a_devnet_keeper_on_a_localnet_genesis_is_refused_too() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (_, c) = e.queue_claim(&usdc, 21 * M);
    set_pools(&e, 1_000 * M, 1_000 * M);
    let mut k = e.keeper();
    k.cluster = setl8_admin::cluster::Cluster::Devnet;
    let r = k.pass();
    assert_eq!(
        r.hard_failure.as_deref(),
        Some(format!("refused: --cluster devnet but the node's genesis hash is {LOCAL_GENESIS}").as_str())
    );
    assert_eq!(r.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), 0);
    assert_eq!(e.claim(&c).unwrap().owed, 21 * M);
    assert_eq!(alert_lines(&e, "genesis_mismatch")[0]["cluster"], "devnet");
}

// ------------------------------------------------------------------ 14. bond claims

#[test]
fn a_vault_whose_only_open_claim_is_a_bond_claim_still_starts_a_cycle_and_pays_it() {
    let mut e = Env::new();
    let usdc = e.usdc;
    let (t, claim) = e.queue_bond_claim(&usdc);
    let c = e.claim(&claim).unwrap();
    assert_eq!(c.kind, CLAIM_KIND_BOND);
    let owed = c.owed;
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total, vs.cycle_id, vs.cycle_started_at), (1, owed, 0, 0));
    assert_eq!(e.open_claims().len(), 1, "the bond claim is the only claim");
    set_pools(&e, 500 * M, 0);
    let before = ata_pair(&e, t);
    let mut k = e.keeper();
    let r = k.pass();
    assert!(r.hard_failure.is_none(), "{:?}", r.hard_failure);
    let l = labels(&r);
    assert!(l[0].starts_with("reconcile_product"), "{l:?}");
    assert_eq!(l[1..], ["begin_heartbeat", "settle_claims x1", "finalize_heartbeat"]);

    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_eligible_count, vs.cycle_processed_count), (1, false, 1, 1));
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (0, 0));
    assert!(e.claim(&claim).is_none(), "paid and closed");
    let after = ata_pair(&e, t);
    assert_eq!((after.0 - before.0, after.1 - before.1), (owed, 0), "paid its net amount from the USDC pool");
    assert_eq!(e.balance(&e.pool(&usdc)), 500 * M - owed);
    assert_eq!(r.exit_code(), 0);
}
