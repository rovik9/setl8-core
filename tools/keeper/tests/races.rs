//! Cycle mechanics under races and failures: a second keeper acting at the worst moment, a keeper that
//! crashes mid-cycle, transactions that land but are not confirmed (or never land), a rate-limited node,
//! one poisoned claim, a claim the program skips, reconcile ordering, priority fees and the send caps.
//!
//! The keeper keeps no state between passes, so every scenario is "do something to the chain at a chosen
//! moment, then check from the chain and the log that the cycle still ended once and every claim was paid
//! exactly once". The chosen moment is set with `Wire`, a `Chain` that wraps `LiteChain`, looks at each
//! transaction the keeper hands to `send` (it decodes the instruction names) and can run an action right
//! before it executes, or let it "succeed" without executing it.
#![cfg(feature = "localnet")]
mod common;
use common::*;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use anchor_lang::Discriminator;
use setl8_admin::admin_ix::{AdminIx, Product, Tier};
use setl8_admin::cluster::Cluster;
use setl8_keeper::chain::{CResult, Chain, ChainError, RawAccount, Sim, TxErr, TxStatus};
use setl8_keeper::config::Config;
use setl8_keeper::ixs;
use setl8_keeper::log::Logger;
use setl8_keeper::plan::{batches, eligible_unprocessed};
use setl8_keeper::runner::{Keeper, NoSleep, PassReport, Sent, Sleeper};
use solana_account::Account as RawAcct;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;

// ------------------------------------------------------------------ the wire

struct Hook {
    name: &'static str,
    skip: u32,
    act: Box<dyn FnMut()>,
}

#[derive(Clone, Debug)]
struct SentIx {
    name: String,
    data: Vec<u8>,
    /// The accounts of the instruction, in order.
    accounts: Vec<Pubkey>,
}

/// `LiteChain` plus: one-shot actions before a matching `send`, swallowed sends, and a record of every
/// transaction the keeper handed over. Clones share everything.
#[derive(Clone)]
struct Wire {
    lite: LiteChain,
    hooks: Rc<RefCell<Vec<Hook>>>,
    swallow: Rc<RefCell<Vec<&'static str>>>,
    txs: Rc<RefCell<Vec<Vec<SentIx>>>>,
    last_sim_units: Rc<Cell<Option<u64>>>,
    sim_units_at_send: Rc<RefCell<Vec<Option<u64>>>>,
    /// The owning keeper's key, so a transaction held back by a hook can be re-signed on a fresh blockhash.
    signer: Rc<RefCell<Option<Keypair>>>,
    /// `blockhash()` reports a last valid block height of 100 instead of "never expires".
    short_blockhash: Rc<Cell<bool>>,
    /// `block_height()` reports 101: every blockhash handed out while `short_blockhash` was on has expired.
    height_past_expiry: Rc<Cell<bool>>,
    /// A transaction the program rejects is ACCEPTED by `send` (as a real node does after its preflight passed) and
    /// its failure only shows up in `status`.
    late_errors: Rc<Cell<bool>>,
    failed_sigs: Rc<RefCell<std::collections::HashMap<String, TxErr>>>,
    /// `claim_addresses()` answers in descending address order (a node's listing order is unspecified).
    reverse_claims: Rc<Cell<bool>>,
    /// The next N `account()` reads fail with an HTTP 429.
    read_errors: Rc<Cell<u32>>,
    /// Simulations answer without a compute unit count (some nodes do).
    hide_units: Rc<Cell<bool>>,
    /// The next N `status()` calls fail with a network error.
    status_errors: Rc<Cell<u32>>,
}

fn ix_name(program: &Pubkey, data: &[u8]) -> String {
    if *program == setl8_admin::constants::COMPUTE_BUDGET_ID {
        return "ComputeBudget".into();
    }
    if *program == core_vault::ID && data.len() >= 8 {
        let d = &data[..8];
        if d == core_vault::instruction::ReconcileProduct::DISCRIMINATOR {
            return "reconcile_product".into();
        }
        if d == core_vault::instruction::BeginHeartbeat::DISCRIMINATOR {
            return "begin_heartbeat".into();
        }
        if d == core_vault::instruction::SettleClaims::DISCRIMINATOR {
            return "settle_claims".into();
        }
        if d == core_vault::instruction::FinalizeHeartbeat::DISCRIMINATOR {
            return "finalize_heartbeat".into();
        }
    }
    format!("unknown program {program}")
}

fn decode(tx: &[u8]) -> (Transaction, Vec<SentIx>) {
    let t: Transaction = bincode::deserialize(tx).expect("the keeper sends a valid transaction");
    let ixs = t
        .message
        .instructions
        .iter()
        .map(|i| {
            let program = t.message.account_keys[i.program_id_index as usize];
            SentIx {
                name: ix_name(&program, &i.data),
                data: i.data.clone(),
                accounts: i.accounts.iter().map(|&a| t.message.account_keys[a as usize]).collect(),
            }
        })
        .collect();
    (t, ixs)
}

impl Wire {
    fn new(lite: &LiteChain) -> Wire {
        Wire {
            lite: lite.clone(),
            hooks: Rc::new(RefCell::new(vec![])),
            swallow: Rc::new(RefCell::new(vec![])),
            txs: Rc::new(RefCell::new(vec![])),
            last_sim_units: Rc::new(Cell::new(None)),
            sim_units_at_send: Rc::new(RefCell::new(vec![])),
            signer: Rc::new(RefCell::new(None)),
            short_blockhash: Rc::new(Cell::new(false)),
            height_past_expiry: Rc::new(Cell::new(false)),
            late_errors: Rc::new(Cell::new(false)),
            failed_sigs: Rc::new(RefCell::new(Default::default())),
            reverse_claims: Rc::new(Cell::new(false)),
            read_errors: Rc::new(Cell::new(0)),
            hide_units: Rc::new(Cell::new(false)),
            status_errors: Rc::new(Cell::new(0)),
        }
    }

    /// Runs `act` once, right before the (`skip`+1)-th send whose vault instruction is `name` executes.
    fn on_send(&self, name: &'static str, skip: u32, act: impl FnMut() + 'static) {
        self.hooks.borrow_mut().push(Hook { name, skip, act: Box::new(act) });
    }

    /// The next send of `name` is acknowledged with its signature but never executed (a dropped transaction).
    fn swallow_next(&self, name: &'static str) {
        self.swallow.borrow_mut().push(name);
    }

    /// The vault instruction names of every transaction handed to `send`, in order.
    fn sent_names(&self) -> Vec<String> {
        self.txs.borrow().iter().map(|t| t.last().unwrap().name.clone()).collect()
    }

    fn count(&self, name: &str) -> usize {
        self.sent_names().iter().filter(|n| n.as_str() == name).count()
    }
}

impl Chain for Wire {
    fn genesis_hash(&self) -> CResult<String> {
        self.lite.genesis_hash()
    }
    fn now(&self) -> CResult<i64> {
        self.lite.now()
    }
    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>> {
        if self.read_errors.get() > 0 {
            self.read_errors.set(self.read_errors.get() - 1);
            return Err(ChainError::RateLimited("HTTP 429".into()));
        }
        self.lite.account(key)
    }
    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>> {
        self.lite.accounts(keys)
    }
    fn claim_addresses(&self) -> CResult<Vec<Pubkey>> {
        let mut v = self.lite.claim_addresses()?;
        if self.reverse_claims.get() {
            v.reverse();
        }
        Ok(v)
    }
    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>> {
        self.lite.registries()
    }
    fn balance(&self, key: &Pubkey) -> CResult<u64> {
        self.lite.balance(key)
    }
    fn blockhash(&self) -> CResult<(Hash, u64)> {
        let (h, last_valid) = self.lite.blockhash()?;
        Ok((h, if self.short_blockhash.get() { 100 } else { last_valid }))
    }
    fn block_height(&self) -> CResult<u64> {
        if self.height_past_expiry.get() {
            return Ok(101);
        }
        self.lite.block_height()
    }
    fn simulate(&self, tx: &[u8]) -> CResult<Sim> {
        let mut s = self.lite.simulate(tx)?;
        if self.hide_units.get() {
            s.units = None;
        }
        self.last_sim_units.set(s.units);
        Ok(s)
    }
    fn send(&self, tx: &[u8]) -> CResult<String> {
        let (t, ixs) = decode(tx);
        let name = ixs.last().unwrap().name.clone();
        self.txs.borrow_mut().push(ixs);
        self.sim_units_at_send.borrow_mut().push(self.last_sim_units.get());
        let hook = {
            let mut hs = self.hooks.borrow_mut();
            let mut found = None;
            for (i, h) in hs.iter_mut().enumerate() {
                if h.name == name {
                    if h.skip > 0 {
                        h.skip -= 1;
                    } else {
                        found = Some(i);
                        break;
                    }
                }
            }
            found.map(|i| hs.remove(i))
        };
        let mut tx = tx.to_vec();
        if let Some(mut h) = hook {
            (h.act)();
            // LiteChain expires the blockhash after every send, so the keeper's blockhash would now be unknown to
            // the SVM. A real node keeps it valid for ~150 blocks: re-sign the SAME message on the fresh blockhash.
            let (fresh, _) = self.lite.blockhash().unwrap();
            let mut t2 = t.clone();
            t2.message.recent_blockhash = fresh;
            t2.signatures = vec![Default::default()];
            let signer = self.signer.borrow();
            let signer = signer.as_ref().expect("keeper_on registered the signer");
            t2.sign(&[signer], fresh);
            tx = bincode::serialize(&t2).unwrap();
        }
        let swallowed = {
            let mut s = self.swallow.borrow_mut();
            let pos = s.iter().position(|n| *n == name);
            pos.map(|i| s.remove(i)).is_some()
        };
        if swallowed {
            self.lite.sends.set(self.lite.sends.get() + 1);
            return Ok(t.signatures[0].to_string());
        }
        match self.lite.send(&tx) {
            Err(ChainError::Rejected(err)) if self.late_errors.get() => {
                let sig = decode(&tx).0.signatures[0].to_string();
                self.failed_sigs.borrow_mut().insert(sig.clone(), err);
                Ok(sig)
            }
            other => other,
        }
    }
    fn status(&self, signature: &str) -> CResult<Option<TxStatus>> {
        if self.status_errors.get() > 0 {
            self.status_errors.set(self.status_errors.get() - 1);
            return Err(ChainError::Network("connection reset".into()));
        }
        if let Some(err) = self.failed_sigs.borrow().get(signature) {
            return Ok(Some(TxStatus::Failed(err.clone())));
        }
        self.lite.status(signature)
    }
}

struct Rec(Rc<RefCell<Vec<Duration>>>);
impl Sleeper for Rec {
    fn sleep(&self, d: Duration) {
        self.0.borrow_mut().push(d);
    }
}

// ------------------------------------------------------------------ helpers

fn keeper_on(e: &Env, wire: Wire, cfg: Config) -> Keeper<Wire> {
    let kp = Keypair::new();
    e.chain.svm.borrow_mut().airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    let payer = kp.pubkey();
    let mut log = Logger::quiet();
    log.capture = Some(e.logs.clone());
    *wire.signer.borrow_mut() = Some(dup(&kp));
    let mut k = Keeper::new(wire, Some(kp), payer, e.keys, Cluster::Localnet, cfg, log);
    k.sleeper = Box::new(NoSleep);
    k
}

struct Q {
    trader: usize,
    claim: Pubkey,
    owed: u64,
}

/// `n` claims owed (10+i) million base units each, buyers alternating USDC / USDT.
fn queue_n(e: &mut Env, n: u64) -> Vec<Q> {
    let (usdc, usdt) = (e.usdc, e.usdt);
    (0..n)
        .map(|i| {
            let mint = if i % 2 == 0 { usdc } else { usdt };
            let owed = (10 + i) * M;
            let (trader, claim) = e.queue_claim(&mint, owed);
            Q { trader, claim, owed }
        })
        .collect()
}

/// Both pools hold far more than every claim: the cycle ratio is 1 and a claim is paid in full.
fn fund(e: &Env) {
    e.fill_pool(&e.usdc, 2_000 * M);
    e.fill_pool(&e.usdt, 2_000 * M);
}

fn pool_total(e: &Env) -> u64 {
    e.balance(&e.pool(&e.usdc)) + e.balance(&e.pool(&e.usdt))
}

/// Per trader: USDC + USDT held.
fn wallet_totals(e: &Env) -> Vec<u64> {
    (0..e.traders.len())
        .map(|t| e.balance(&ata(&e.wallet(t), &e.usdc)) + e.balance(&ata(&e.wallet(t), &e.usdt)))
        .collect()
}

/// Every claim closed, each trader received exactly what was owed (no more, no less), the pools lost
/// exactly the sum, nothing is open and no cycle is running.
fn assert_all_paid_exactly_once(e: &Env, wallets_before: &[u64], pools_before: u64, qs: &[Q]) {
    let after = wallet_totals(e);
    let mut total = 0;
    for q in qs {
        assert!(e.claim(&q.claim).is_none(), "claim of trader {} must be closed", q.trader);
        assert_eq!(after[q.trader] - wallets_before[q.trader], q.owed, "trader {}", q.trader);
        total += q.owed;
    }
    assert_eq!(pools_before - pool_total(e), total, "the pools lost exactly what was owed");
    let vs = e.vault_state();
    assert_eq!(vs.open_claims_count, 0);
    assert_eq!(vs.open_claims_total, 0);
    assert!(!vs.cycle_active);
    assert_eq!(vs.cycle_processed_count, vs.cycle_eligible_count);
}

fn labels(r: &PassReport) -> Vec<String> {
    r.sent.iter().map(|s| s.label.clone()).collect()
}

fn errors_logged(e: &Env) -> Vec<serde_json::Value> {
    e.log_lines().into_iter().filter(|l| l["level"] == "error").collect()
}

fn reconcile_label(p: &Pubkey) -> String {
    format!("reconcile_product {p}")
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Keeper B's single `settle_claims` over batch number `n` (0-based) of the claims A will also try (sorted by address,
/// as the keeper does).
fn settle_batch_n(b: &mut Keeper<LiteChain>, n: usize) -> Sent {
    let w = b.read_world().unwrap();
    let claims = b.load_claims().unwrap();
    let todo = eligible_unprocessed(&w.vault, &claims);
    let batch = batches(&todo).remove(n);
    let ix = ixs::settle_claims_for(&b.keys, &b.payer, &w.vault, &batch);
    b.send("settle_claims (keeper B)", vec![ix])
}

/// Keeper B's single `settle_claims` over the first batch A will also try.
fn settle_first_batch(b: &mut Keeper<LiteChain>) -> Sent {
    settle_batch_n(b, 0)
}

// ------------------------------------------------------------------ 1. a second keeper wins a settle race

/// Pools too small for the claims (ratio < 1): a claim paid in a cycle stays open, partly owed, marked as
/// processed in that cycle. That is the world in which a lost race shows up as `ClaimAlreadySettled`.
fn underfund(e: &Env, usdc: u64, usdt: u64) -> u64 {
    e.fill_pool(&e.usdc, usdc);
    e.fill_pool(&e.usdt, usdt);
    usdc + usdt
}

/// What the program pays a claim in a cycle whose pools held `available` against `total_owed`: floor(owed * min(avail, owed_total) / owed_total).
fn pay_of(owed: u64, available: u64, total_owed: u64) -> u64 {
    (owed as u128 * available.min(total_owed) as u128 / total_owed as u128) as u64
}

/// Every claim got exactly its share once: the trader received it, the claim is still open with the rest,
/// marked processed in `cycle`; the pools lost exactly the sum.
fn assert_partially_paid_once(e: &Env, wallets: &[u64], pools: u64, qs: &[Q], available: u64, cycle: u64) {
    let total_owed: u64 = qs.iter().map(|q| q.owed).sum();
    let after = wallet_totals(e);
    let mut paid = 0;
    for q in qs {
        let pay = pay_of(q.owed, available, total_owed);
        assert!(pay > 0 && pay < q.owed);
        assert_eq!(after[q.trader] - wallets[q.trader], pay, "trader {} paid exactly once", q.trader);
        let c = e.claim(&q.claim).expect("a part-paid claim stays open");
        assert_eq!((c.owed, c.last_settled_cycle), (q.owed - pay, cycle));
        paid += pay;
    }
    assert_eq!(pools - pool_total(e), paid);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (qs.len() as u64, total_owed - paid));
    assert!(!vs.cycle_active);
    assert_eq!(vs.cycle_processed_count, vs.cycle_eligible_count);
}

#[test]
fn losing_a_settle_race_is_logged_as_claim_already_settled_and_the_cycle_still_finishes_with_exact_payments() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    let available = underfund(&e, 40 * M, 30 * M);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    let b_result: Rc<RefCell<Option<Sent>>> = Rc::new(RefCell::new(None));
    let slot = b_result.clone();
    // right before A's first settle executes, B settles the very same six claims
    wire.on_send("settle_claims", 0, move || {
        *slot.borrow_mut() = Some(settle_first_batch(&mut b));
    });

    let ra = a.pass();

    assert!(matches!(b_result.borrow().as_ref(), Some(Sent::Done { .. })), "B's settle landed first");
    assert_eq!(ra.hard_failure, None);
    // exit 10: the pools no longer cover what is still owed (an alert condition, not a failure)
    assert_eq!(ra.exit_code(), 10);
    assert_eq!(e.alert_kinds(), vec!["coverage_below_one".to_string()]);
    // A's own sends: the lost batch of 6 is not in its list; it settled the remaining 2 and finalized
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x2".into(),
            "finalize_heartbeat".into()
        ]
    );
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], "settle_claims x6");
    assert_eq!(lost[0]["error"], "ClaimAlreadySettled (Custom 6030)");
    assert_eq!(lost[0]["level"], "info");
    assert!(errors_logged(&e).is_empty(), "a lost race is not an error: {:?}", errors_logged(&e));
    assert!(e.logged("batch_split").is_empty(), "a race is not a poisoned batch");
    assert_partially_paid_once(&e, &wallets, pools, &qs, available, 1);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_eligible_count, vs.cycle_processed_count), (1, 8, 8));
    // counters: A sent 5 (one failed on chain), B 1; 5 landed; A's own count excludes the failed one
    assert_eq!(e.chain.sends.get(), 6);
    assert_eq!(e.chain.landed.borrow().len(), 5);
    assert_eq!(a.stats.sends, 4);
    assert_eq!(
        wire.sent_names(),
        s(&["reconcile_product", "begin_heartbeat", "settle_claims", "settle_claims", "finalize_heartbeat"])
    );
}

#[test]
fn losing_a_settle_race_on_a_single_claim_batch_is_benign_too() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 1);
    let available = underfund(&e, 4 * M, 3 * M);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_first_batch(&mut b), Sent::Done { .. }));
    });
    let ra = a.pass();
    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 10);
    assert_eq!(e.alert_kinds(), vec!["coverage_below_one".to_string()]);
    assert_eq!(labels(&ra), vec![reconcile_label(&e.sector), "begin_heartbeat".into(), "finalize_heartbeat".into()]);
    assert_eq!(e.logged("lost_race").len(), 1);
    assert_eq!(e.logged("lost_race")[0]["error"], "ClaimAlreadySettled (Custom 6030)");
    assert!(errors_logged(&e).is_empty());
    assert_partially_paid_once(&e, &wallets, pools, &qs, available, 1);
    let c = e.claim(&qs[0].claim).unwrap();
    assert_eq!(c.owed, 3 * M, "10M owed, 7M available: 7M paid once, 3M still owed");
    assert_eq!(e.chain.landed.borrow().len(), 4, "reconcile, begin, B's settle, finalize");
}

#[test]
fn a_settle_race_lost_to_a_keeper_that_paid_the_claims_in_full_still_ends_the_cycle_with_exact_payments() {
    // The winner paid the six claims in full, so the program CLOSED their accounts; A's stale batch then finds
    // accounts it no longer owns and fails with InvalidClaim (see the ignored test below for what is logged).
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_first_batch(&mut b), Sent::Done { .. }));
    });
    let ra = a.pass();
    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 0);
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x2".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_eligible_count, vs.cycle_processed_count), (1, 8, 8));
    assert_eq!(e.chain.landed.borrow().len(), 5);
    assert_eq!(a.stats.sends, 4);
}

#[test]
fn a_settle_race_lost_to_a_keeper_that_paid_the_claims_in_full_is_logged_as_a_benign_race_not_as_errors() {
    let mut e = Env::new();
    let _qs = queue_n(&mut e, 8);
    fund(&e);
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_first_batch(&mut b), Sent::Done { .. }));
    });
    let ra = a.pass();
    assert_eq!(ra.hard_failure, None);
    assert!(errors_logged(&e).is_empty(), "a benign race is not an error: {:?}", errors_logged(&e));
    assert!(e.logged("batch_split").is_empty());
    assert_eq!(e.logged("lost_race").len(), 1);
}

// ------------------------------------------------------------------ 2. two keepers interleaved

#[test]
fn keepers_taking_turns_pass_by_pass_end_the_cycle_once_and_the_second_only_idles() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let (wa, wb) = (Wire::new(&e.chain), Wire::new(&e.chain));
    let mut a = keeper_on(&e, wa.clone(), Config::default());
    let mut b = keeper_on(&e, wb.clone(), Config::default());
    let mut reports = vec![];
    for _ in 0..3 {
        reports.push(("A", a.pass()));
        reports.push(("B", b.pass()));
    }
    for (who, r) in &reports {
        assert_eq!(r.hard_failure, None, "{who}");
        assert_eq!(r.exit_code(), 0, "{who}");
        assert!(r.alerts.is_empty(), "{who}");
    }
    // the first pass does the whole cycle; every other pass finds nothing owed and sends nothing
    assert_eq!(
        labels(&reports[0].1),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x6".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    for (who, r) in &reports[1..] {
        assert!(r.sent.is_empty(), "{who}");
        assert!(!r.progress, "{who}");
        assert_eq!(
            r.idle.as_deref(),
            Some("no open claims: no cycle is begun (an empty cycle would only burn the 5-day slot)"),
            "{who}"
        );
    }
    assert_eq!(wa.count("finalize_heartbeat") + wb.count("finalize_heartbeat"), 1);
    assert_eq!(wb.sent_names().len(), 0);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(e.vault_state().cycle_id, 1);
}

#[test]
fn keeper_b_finishing_the_cycle_inside_a_pass_of_a_makes_a_stale_settle_a_benign_race_and_the_cycle_ends_once() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let (wa, wb) = (Wire::new(&e.chain), Wire::new(&e.chain));
    let mut a = keeper_on(&e, wa.clone(), Config::default());
    let b = keeper_on(&e, wb.clone(), Config::default());
    let b_report: Rc<RefCell<Option<PassReport>>> = Rc::new(RefCell::new(None));
    let (slot, mut b) = (b_report.clone(), b);
    // A lands its first batch, then right before its second batch executes B runs a whole pass:
    // B settles claims 7..15 (the same six A is about to send, then three) and finalizes
    wa.on_send("settle_claims", 1, move || {
        *slot.borrow_mut() = Some(b.pass());
    });

    let ra = a.pass();
    let rb = b_report.borrow_mut().take().expect("B ran");

    assert_eq!(ra.hard_failure, None);
    assert_eq!(rb.hard_failure, None);
    assert_eq!((ra.exit_code(), rb.exit_code()), (0, 0));
    assert_eq!(labels(&ra), vec![reconcile_label(&e.sector), "begin_heartbeat".into(), "settle_claims x6".into()]);
    assert_eq!(labels(&rb), s(&["settle_claims x6", "settle_claims x3", "finalize_heartbeat"]));
    // A's stale batch hit a cycle that B had already closed; its third batch was found done on re-read
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], "settle_claims x6");
    assert_eq!(lost[0]["error"], "NoCycleInProgress (Custom 6026)");
    assert_eq!(e.logged("batch_already_done").len(), 1);
    assert_eq!(e.logged("batch_already_done")[0]["claims"], 3);
    assert!(errors_logged(&e).is_empty());
    assert_eq!((wa.count("finalize_heartbeat"), wb.count("finalize_heartbeat")), (0, 1), "only B finalized");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(e.vault_state().cycle_id, 1);
    // A, B, A, B again afterwards: nothing is left to do for either
    let mut b2 = e.keeper();
    for _ in 0..2 {
        for r in [a.pass(), b2.pass()] {
            assert!(r.sent.is_empty());
            assert_eq!(r.exit_code(), 0);
        }
    }
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_eq!(e.chain.landed.borrow().len(), 2 + 1 + 2 + 1, "reconcile, begin, A's x6, B's x6 + x3, finalize");
}

#[test]
fn both_keepers_reaching_finalize_close_the_cycle_once_and_the_loser_does_not_fail() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wa = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wa.clone(), Config::default());
    let mut b = e.keeper();
    let b_report: Rc<RefCell<Option<PassReport>>> = Rc::new(RefCell::new(None));
    let slot = b_report.clone();
    // everything is settled; right before A's finalize executes B's pass finalizes
    wa.on_send("finalize_heartbeat", 0, move || {
        *slot.borrow_mut() = Some(b.pass());
    });
    let ra = a.pass();
    let rb = b_report.borrow_mut().take().expect("B ran");
    assert_eq!((ra.hard_failure.clone(), rb.hard_failure.clone()), (None, None));
    assert_eq!((ra.exit_code(), rb.exit_code()), (0, 0));
    assert_eq!(labels(&rb), s(&["finalize_heartbeat"]));
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x6".into(),
            "settle_claims x3".into()
        ],
        "A's finalize lost the race and is not reported as sent"
    );
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], "finalize_heartbeat");
    assert_eq!(lost[0]["error"], "NoCycleInProgress (Custom 6026)");
    assert_eq!(wa.count("finalize_heartbeat"), 1);
    assert_eq!(e.chain.landed.borrow().len(), 2 + 3 + 1, "reconcile, begin, three settles, ONE finalize");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(e.vault_state().cycle_id, 1);
}

// ------------------------------------------------------------------ 3. both begin at once

#[test]
fn when_the_other_keeper_begins_first_begin_fails_with_cycle_in_progress_and_the_keeper_continues_that_cycle() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    let b_result: Rc<RefCell<Option<Sent>>> = Rc::new(RefCell::new(None));
    let slot = b_result.clone();
    wire.on_send("begin_heartbeat", 0, move || {
        let w = b.read_world().unwrap();
        *slot.borrow_mut() = Some(b.send("begin_heartbeat (keeper B)", vec![ixs::begin(&b.keys, &b.payer, &w.vault)]));
    });

    let ra = a.pass();

    assert!(matches!(b_result.borrow().as_ref(), Some(Sent::Done { .. })));
    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 0);
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "settle_claims x6".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ],
        "no begin of its own, but it carried the cycle B opened to its end"
    );
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], "begin_heartbeat");
    assert_eq!(lost[0]["error"], "CycleInProgress (Custom 6025)");
    assert!(errors_logged(&e).is_empty());
    let vs = e.vault_state();
    assert_eq!(vs.cycle_id, 1, "exactly one cycle");
    assert_eq!(vs.cycle_started_at, T0);
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (7, 7));
    assert_eq!(wire.count("begin_heartbeat"), 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn when_the_other_keeper_runs_the_whole_cycle_before_begin_lands_begin_fails_with_heartbeat_too_early_and_no_second_cycle_starts(
) {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    let b_report: Rc<RefCell<Option<PassReport>>> = Rc::new(RefCell::new(None));
    let slot = b_report.clone();
    wire.on_send("begin_heartbeat", 0, move || {
        *slot.borrow_mut() = Some(b.pass());
    });

    let ra = a.pass();
    let rb = b_report.borrow_mut().take().unwrap();

    assert_eq!(rb.hard_failure, None);
    assert_eq!(
        labels(&rb),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 0);
    assert_eq!(labels(&ra), vec![reconcile_label(&e.sector)], "A's begin lost; there was no cycle left to continue");
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["error"], "HeartbeatTooEarly (Custom 6027)");
    assert_eq!(e.vault_state().cycle_id, 1, "no second cycle");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(wire.count("begin_heartbeat"), 1);
}

// ------------------------------------------------------------------ 4. crash and restart mid-cycle

/// 15 claims; keeper 1 has `max_sends: cap`; it is dropped and a brand-new keeper (own key, own state) resumes.
fn crash_and_resume(cap: u32, first_hard: &str, first: &[&str], second: &[&str], processed_at_crash: u64) {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let mut k1 = e.keeper_with(Config { max_sends: cap, ..Config::default() });
    let r1 = k1.pass();
    let hard = r1.hard_failure.clone().expect("the cap is a hard failure");
    assert_eq!(hard, first_hard);
    assert!(hard.contains("send cap reached"));
    assert_eq!(r1.exit_code(), 20);
    let first: Vec<String> =
        first.iter().map(|l| if *l == "R" { reconcile_label(&e.sector) } else { l.to_string() }).collect();
    assert_eq!(labels(&r1), first);
    assert_eq!(k1.stats.sends, cap);
    let caps = e.logged("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-sends-per-run");
    assert_eq!(caps[0]["sends"], cap);
    let vs = e.vault_state();
    assert!(vs.cycle_active, "the crashed keeper left the cycle open");
    assert_eq!(vs.cycle_id, 1);
    assert_eq!(vs.cycle_processed_count, processed_at_crash);
    let paid_so_far: u64 = qs.iter().filter(|q| e.claim(&q.claim).is_none()).map(|q| q.owed).sum();
    assert_eq!(vs.open_claims_total, qs.iter().map(|q| q.owed).sum::<u64>() - paid_so_far);
    drop(k1);

    let mut k2 = e.keeper();
    assert_eq!(k2.stats.sends, 0, "a fresh keeper shares nothing with the dead one");
    let r2 = k2.pass();
    assert_eq!(r2.hard_failure, None);
    assert_eq!(r2.exit_code(), 0);
    assert_eq!(labels(&r2), s(second));
    assert_eq!(k2.stats.sends as usize, second.len());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(e.vault_state().cycle_id, 1, "the same cycle, not a new one");
    assert_eq!(e.chain.landed.borrow().len(), 2 + 3 + 1, "reconcile, begin, 3 settles, 1 finalize: nothing sent twice");
    assert_eq!(e.chain.sends.get(), 6);
    assert!(errors_logged(&e).iter().all(|l| l["event"] == "send_cap_reached"));
}

#[test]
fn a_keeper_that_dies_right_after_begin_is_replaced_by_one_that_settles_everything_and_finalizes() {
    crash_and_resume(
        2,
        "send cap reached (max-sends-per-run)",
        &["R", "begin_heartbeat"],
        &["settle_claims x6", "settle_claims x6", "settle_claims x3", "finalize_heartbeat"],
        0,
    );
}

#[test]
fn a_keeper_that_dies_after_one_settle_batch_is_replaced_by_one_that_finishes_without_paying_that_batch_again() {
    crash_and_resume(
        3,
        "send cap reached (max-sends-per-run)",
        &["R", "begin_heartbeat", "settle_claims x6"],
        &["settle_claims x6", "settle_claims x3", "finalize_heartbeat"],
        6,
    );
}

#[test]
fn a_keeper_that_dies_just_before_finalize_is_replaced_by_one_that_only_finalizes() {
    crash_and_resume(
        5,
        "send cap reached before the cycle could be finalized",
        &["R", "begin_heartbeat", "settle_claims x6", "settle_claims x6", "settle_claims x3"],
        &["finalize_heartbeat"],
        15,
    );
}

// ------------------------------------------------------------------ 5. landed but not confirmed

fn cfg_unconfirmed() -> Config {
    // 2 status polls per send
    Config { confirm_timeout_secs: 1, ..Config::default() }
}

#[test]
fn a_settle_that_landed_but_is_reported_unconfirmed_is_not_sent_again() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), cfg_unconfirmed());
    let lite = e.chain.clone();
    // larger than the 2 polls: the first settle's status is unknown for both polls (and for one poll of the next send)
    wire.on_send("settle_claims", 0, move || lite.status_unknown.set(3));

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x6".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    let un = e.logged("unconfirmed");
    assert_eq!(un.len(), 1);
    assert_eq!(un[0]["label"], "settle_claims x6");
    assert_eq!(un[0]["level"], "warn");
    let landed = e.chain.landed.borrow().clone();
    assert_eq!(un[0]["signature"], landed[2].as_str(), "the unconfirmed one is the third landed transaction");
    assert_eq!(r.sent[2].signature, landed[2]);
    assert_eq!(r.sent[2].units, None, "no confirmation, no units");
    assert!(r.sent.iter().enumerate().filter(|(i, _)| *i != 2).all(|(_, t)| t.units.is_some()));
    // three batches, three settle transactions: none repeated
    assert_eq!(wire.count("settle_claims"), 3);
    assert_eq!(landed.len(), 6);
    assert_eq!(e.chain.sends.get(), 6);
    assert!(e.logged("lost_race").is_empty());
    assert!(errors_logged(&e).is_empty(), "{:?}", errors_logged(&e));
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_begin_that_landed_but_is_reported_unconfirmed_is_not_repeated_and_the_cycle_goes_on() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 4);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), cfg_unconfirmed());
    let lite = e.chain.clone();
    wire.on_send("begin_heartbeat", 0, move || lite.status_unknown.set(2));
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x4".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(e.logged("unconfirmed").len(), 1);
    assert_eq!(e.logged("unconfirmed")[0]["label"], "begin_heartbeat");
    assert_eq!(wire.count("begin_heartbeat"), 1);
    assert!(e.logged("lost_race").is_empty());
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_finalize_that_landed_but_is_reported_unconfirmed_is_not_repeated_by_the_next_pass() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 4);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), cfg_unconfirmed());
    let lite = e.chain.clone();
    wire.on_send("finalize_heartbeat", 0, move || lite.status_unknown.set(2));
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(e.logged("unconfirmed").len(), 1);
    assert_eq!(e.logged("unconfirmed")[0]["label"], "finalize_heartbeat");
    let r2 = a.pass();
    assert!(r2.sent.is_empty());
    assert_eq!(r2.hard_failure, None);
    assert_eq!(wire.count("finalize_heartbeat"), 1);
    assert!(e.logged("lost_race").is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_settle_that_was_acknowledged_but_never_landed_is_sent_again_after_the_re_read() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), cfg_unconfirmed());
    let lite = e.chain.clone();
    wire.on_send("settle_claims", 0, move || lite.status_unknown.set(2));
    wire.swallow_next("settle_claims");

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(), // the dropped one (unconfirmed)
            "settle_claims x6".into(),
            "settle_claims x3".into(),
            "settle_claims x6".into(), // the same six again, after the re-read showed 9 of 15 processed
            "finalize_heartbeat".into()
        ]
    );
    let un = e.logged("unconfirmed");
    assert_eq!(un.len(), 1);
    assert_eq!(wire.count("settle_claims"), 4);
    assert_eq!(e.chain.sends.get(), 7);
    assert_eq!(e.chain.landed.borrow().len(), 6, "the dropped transaction never landed");
    assert!(!e.chain.landed.borrow().contains(&r.sent[2].signature));
    assert!(e.logged("lost_race").is_empty());
    assert!(errors_logged(&e).is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

// ------------------------------------------------------------------ 6. rate limiting

#[test]
fn two_rate_limited_sends_are_retried_with_backoff_and_the_cycle_completes() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let mut a = e.keeper();
    let slept = Rc::new(RefCell::new(vec![]));
    a.sleeper = Box::new(Rec(slept.clone()));
    e.chain.rate_limit_sends.set(2);

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    // 5 real transactions plus the 2 refused with HTTP 429 before reaching the chain
    assert_eq!(e.chain.sends.get(), 7);
    assert_eq!(e.chain.landed.borrow().len(), 5);
    assert_eq!(a.stats.sends, 5, "only accepted sends count against the cap");
    assert_eq!(e.chain.rate_limit_sends.get(), 0);
    assert_eq!(
        *slept.borrow(),
        vec![Duration::from_millis(1000), Duration::from_millis(2000)],
        "backoff 500ms << attempt"
    );
    assert!(e.logged("transient_failure").is_empty());
    assert!(errors_logged(&e).is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_node_that_rate_limits_longer_than_the_retries_ends_the_pass_with_a_hard_failure_and_pays_nothing() {
    // Exposes a keeper bug: `begin()` ignores Sent::Transient / Sent::Failed, so a pass whose begin_heartbeat could
    // not be sent at all returns hard_failure = None and exit code 0, as if all were well.
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, _) = (wallet_totals(&e), pool_total(&e));
    let mut a = e.keeper_with(Config { max_retries: 4, ..Config::default() });
    e.chain.rate_limit_sends.set(100);

    let r = a.pass();

    assert!(r.sent.is_empty());
    assert_eq!(e.chain.landed.borrow().len(), 0);
    assert!(r.hard_failure.is_some(), "a pass that could send nothing must not look healthy");
    assert_eq!(r.exit_code(), 20);
    assert_eq!(wallet_totals(&e), wallets, "nothing paid");
    let vs = e.vault_state();
    assert!(!vs.cycle_active);
    assert_eq!(vs.cycle_id, 0);
    assert_eq!(vs.open_claims_count, 7);
    assert_eq!(qs.len(), 7);
}

#[test]
fn a_begin_that_cannot_be_sent_after_a_good_reconcile_ends_the_pass_with_a_hard_failure_naming_begin() {
    // The reconcile goes through; only begin_heartbeat is refused (HTTP 429) for longer than the retries allow.
    // A pass that cannot start the cycle must not look healthy.
    let mut e = Env::new();
    let qs = queue_n(&mut e, 4);
    fund(&e);
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config { max_retries: 2, ..Config::default() });
    let lite = e.chain.clone();
    wire.on_send("begin_heartbeat", 0, move || lite.rate_limit_sends.set(100));

    let r = a.pass();

    assert_eq!(labels(&r), vec![reconcile_label(&e.sector)], "only the reconcile landed");
    let f = r.hard_failure.clone().expect("a pass that could not begin the cycle must fail");
    assert!(f.starts_with("cannot send begin_heartbeat"), "{f}");
    assert_eq!(r.exit_code(), 20);
    assert_eq!(e.vault_state().cycle_id, 0);
    assert!(!e.vault_state().cycle_active);
    assert_eq!(qs.len(), 4);
}

#[test]
fn a_node_that_rate_limits_longer_than_the_retries_while_settling_fails_clearly_and_a_later_pass_finishes_without_double_payment(
) {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config { max_retries: 4, ..Config::default() });
    let lite = e.chain.clone();
    // from the first settle on, 10 sends are refused with HTTP 429: more than the 5 attempts (1 + max_retries 4)
    wire.on_send("settle_claims", 0, move || lite.rate_limit_sends.set(10));

    let r1 = a.pass();

    assert_eq!(labels(&r1), vec![reconcile_label(&e.sector), "begin_heartbeat".into()]);
    assert_eq!(r1.hard_failure.as_deref(), Some("cannot send settle_claims: HTTP 429"));
    assert_eq!(r1.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), 2 + 5, "reconcile, begin, then five attempts at the first settle");
    assert_eq!(e.chain.rate_limit_sends.get(), 5);
    assert_eq!(e.logged("transient_failure").len(), 1);
    assert_eq!(e.logged("transient_failure")[0]["label"], "settle_claims x6");
    assert_eq!(e.logged("transient_failure")[0]["error"], "HTTP 429");
    assert_eq!(wallet_totals(&e), wallets, "nothing was paid by the failed pass");
    let vs = e.vault_state();
    assert!(vs.cycle_active);
    assert_eq!(vs.cycle_processed_count, 0);

    // the node recovers: a new pass resumes the open cycle (no second begin) and pays everything once
    e.chain.rate_limit_sends.set(0);
    let r2 = a.pass();
    assert_eq!(r2.hard_failure, None);
    assert_eq!(labels(&r2), s(&["settle_claims x6", "settle_claims x1", "finalize_heartbeat"]));
    assert_eq!(wire.count("begin_heartbeat"), 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
    assert_eq!(e.vault_state().cycle_id, 1);
}

// ------------------------------------------------------------------ 7. one bad claim

#[test]
fn one_poisoned_claim_is_isolated_by_splitting_the_batch_and_every_good_claim_is_still_paid_exactly() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    // the keeper's batches follow the claims sorted by address: the first six form batch 1
    let mut order: Vec<usize> = (0..qs.len()).collect();
    order.sort_by_key(|&i| qs[i].claim.to_bytes());
    let bad = order[2];
    let bad_addr = qs[bad].claim;
    // kind offset = 8 (discriminator) + 3 * 32 (pubkeys) + 4 * 8 (request_id, owed, created, last_settled) = 136
    let acct = e.chain.svm.borrow().get_account(&bad_addr).unwrap();
    assert_eq!(acct.data[136], e.claim(&bad_addr).unwrap().kind);
    assert_eq!(acct.data[136], 0);
    let mut data = acct.data.clone();
    data[136] = 7;
    e.chain.svm.borrow_mut().set_account(bad_addr, RawAcct { data, ..acct }).unwrap();
    assert_eq!(e.claim(&bad_addr).unwrap().kind, 7, "the offset is the kind field");
    assert_eq!(e.claim(&bad_addr).unwrap().owed, qs[bad].owed);

    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let r = a.pass();

    // batch 1 is refused (InvalidClaim): the five good claims are settled one by one, the bad one is quarantined
    let mut expect = vec![reconcile_label(&e.sector), "begin_heartbeat".to_string()];
    for &i in order.iter().take(6) {
        if i != bad {
            expect.push(format!("settle_claims {}", qs[i].claim));
        }
    }
    expect.push("settle_claims x2".into());
    assert_eq!(labels(&r), expect);
    assert_eq!(e.logged("batch_split").len(), 1);
    assert_eq!(e.logged("batch_split")[0]["claims"], 6);
    assert_eq!(e.logged("batch_split")[0]["level"], "warn");
    let loud: Vec<_> = e.logged("settle_refused");
    assert_eq!(loud.len(), 2, "the whole batch, then the bad claim alone");
    assert!(loud.iter().all(|l| l["error"] == "InvalidClaim (Custom 6031)" && l["level"] == "warn"));
    assert_eq!(loud[0]["label"], "settle_claims x6");
    assert_eq!(loud[1]["label"], format!("settle_claims {bad_addr}"));
    // a refusal is only an ERROR once no rival could have caused it: exactly the bad claim is reported as one
    let quarantined = e.logged("claim_quarantined");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["claim"], bad_addr.to_string());
    assert_eq!(quarantined[0]["level"], "error");
    let stuck = e.logged("cycle_stuck");
    assert_eq!(stuck.len(), 1);
    assert_eq!(r.exit_code(), 20);
    assert_eq!(
        r.hard_failure.as_deref(),
        Some("the cycle expects 8 claims processed, 7 are, and none is left that this keeper can settle (quarantined: 1)")
    );

    // every good claim paid exactly; the bad one is untouched
    let after = wallet_totals(&e);
    let mut good_total = 0;
    for (i, q) in qs.iter().enumerate() {
        if i == bad {
            assert_eq!(after[q.trader], wallets[q.trader], "the poisoned claim's wallet received nothing");
            assert_eq!(e.claim(&q.claim).unwrap().owed, q.owed);
            assert_eq!(e.claim(&q.claim).unwrap().last_settled_cycle, 0);
        } else {
            assert_eq!(after[q.trader] - wallets[q.trader], q.owed, "good claim {i}");
            assert!(e.claim(&q.claim).is_none());
            good_total += q.owed;
        }
    }
    assert_eq!(pools - pool_total(&e), good_total);
    let vs = e.vault_state();
    assert!(vs.cycle_active, "the cycle cannot finalize");
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (8, 7));
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, qs[bad].owed));
    assert_eq!(e.chain.landed.borrow().len(), 2 + 5 + 1);
    // the refusals were found by SIMULATING (the whole batch of six, then the bad claim alone): neither was ever sent
    assert_eq!(wire.txs.borrow().len(), 8, "reconcile, begin, five single settles, the batch of two");
    assert_eq!(e.chain.sends.get(), 8);
    assert_eq!(e.chain.simulations.get(), 10, "the 8 sent transactions plus the 2 refused ones");

    // another pass: still refuses, sends nothing, pays nothing more
    let sends_before = e.chain.sends.get();
    let r2 = a.pass();
    assert!(r2.sent.is_empty());
    assert_eq!(r2.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), sends_before);
    assert_eq!(wallet_totals(&e), after);
    assert_eq!(wire.count("finalize_heartbeat"), 0);
}

// ------------------------------------------------------------------ 8. a skipped claim

fn three_claims_one_without_usdc_account() -> (Env, Vec<Q>, Vec<u64>, u64) {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    e.delete_account(&ata(&e.wallet(qs[1].trader), &e.usdc));
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    (e, qs, wallets, pools)
}

#[test]
fn a_claim_skipped_by_the_program_counts_as_processed_and_is_not_settled_again_by_the_next_pass() {
    let (e, qs, wallets, pools) = three_claims_one_without_usdc_account();
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(wire.count("settle_claims"), 1, "one settle transaction for the whole cycle");
    let skipped = e.claim(&qs[1].claim).unwrap();
    assert_eq!(skipped.owed, qs[1].owed, "still owed");
    assert_eq!(skipped.last_settled_cycle, 1, "processed in cycle 1");
    let vs = e.vault_state();
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (3, 3));
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, qs[1].owed));
    assert!(!vs.cycle_active);
    let after = wallet_totals(&e);
    assert_eq!(after[qs[0].trader] - wallets[qs[0].trader], qs[0].owed);
    assert_eq!(after[qs[2].trader] - wallets[qs[2].trader], qs[2].owed);
    assert_eq!(after[qs[1].trader], wallets[qs[1].trader]);
    assert_eq!(pools - pool_total(&e), qs[0].owed + qs[2].owed);

    // at once, and again: no settle, no send at all; the gap since the cycle's start is the only thing it waits for
    let sends = e.chain.sends.get();
    for _ in 0..3 {
        let r2 = a.pass();
        assert!(r2.sent.is_empty());
        assert!(!r2.progress);
        assert_eq!(r2.hard_failure, None);
        assert_eq!(r2.exit_code(), 0);
        assert_eq!(r2.idle.as_deref(), Some("next cycle may begin in 432000 s"));
    }
    assert_eq!(e.chain.sends.get(), sends);
    assert_eq!(wire.count("settle_claims"), 1);
    assert_eq!(e.claim(&qs[1].claim).unwrap().last_settled_cycle, 1);
}

#[test]
fn a_skipped_claim_is_paid_in_the_next_cycle_once_its_account_exists_again_and_not_before() {
    let (e, qs, wallets, _) = three_claims_one_without_usdc_account();
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    assert_eq!(a.pass().hard_failure, None);
    // the wallet gets its account back: nothing changes until the next cycle may begin
    let w = e.wallet(qs[1].trader);
    e.set_token(&ata(&w, &e.usdc), &e.usdc, &w, 1_000 * M);
    let r = a.pass();
    assert!(r.sent.is_empty());
    assert_eq!(r.idle.as_deref(), Some("next cycle may begin in 432000 s"));
    assert_eq!(e.claim(&qs[1].claim).unwrap().owed, qs[1].owed);
    e.advance(GAP - 1);
    let r = a.pass();
    assert!(r.sent.is_empty());
    assert_eq!(r.idle.as_deref(), Some("next cycle may begin in 1 s"));
    e.advance(1);

    let before = e.balance(&ata(&w, &e.usdc)) + e.balance(&ata(&w, &e.usdt));
    assert_eq!(
        before,
        1_000 * M + 950 * M,
        "the restored USDC account holds 1,000; the USDT one 950 after the $50 challenge"
    );
    assert_eq!(wallets[qs[1].trader], 950 * M, "before the restore only the USDT account existed");
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    let after = e.balance(&ata(&w, &e.usdc)) + e.balance(&ata(&w, &e.usdt));
    assert_eq!(after - before, qs[1].owed);
    assert!(e.claim(&qs[1].claim).is_none());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.open_claims_count, vs.open_claims_total), (2, 0, 0));
    assert_eq!(wire.count("settle_claims"), 2);
}

// ------------------------------------------------------------------ 9. reconcile ordering

fn register_second_product(e: &Env) -> Pubkey {
    let p2 = Pubkey::new_unique();
    let product = Product {
        product_program_id: p2,
        fee_split_bps: 6500,
        challenge_sizes: vec![Tier { size: 5_000_000_000, cost: 50_000_000 }],
        max_payout_count: 1000,
        reset_price_bps: vec![100],
    };
    e.send_admin(AdminIx::RegisterProduct(product).build(&e.keys));
    p2
}

#[test]
fn every_active_product_is_reconciled_in_key_order_before_begin_heartbeat() {
    let mut e = Env::new();
    let p2 = register_second_product(&e);
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    assert_eq!(e.chain.registries().unwrap().len(), 2);
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    let mut products = [e.sector, p2];
    products.sort_by_key(|p| p.to_bytes());
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&products[0]),
            reconcile_label(&products[1]),
            "begin_heartbeat".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(
        wire.sent_names(),
        s(&["reconcile_product", "reconcile_product", "begin_heartbeat", "settle_claims", "finalize_heartbeat"])
    );
    // both stayed active: the missing tally of the second product reads 0 / 0 and matches its books
    assert!(e.registry(&e.sector).active);
    assert!(e.registry(&p2).active);
    assert!(e.logged("reconcile_skipped").is_empty());
    assert!(e.alert_kinds().is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_paused_product_is_not_reconciled_and_the_skip_is_logged() {
    let mut e = Env::new();
    let p2 = register_second_product(&e);
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    e.send_admin(AdminIx::PauseProduct { product: p2 }.build(&e.keys));
    let reg = e.registry(&p2);
    assert!(!reg.active);
    let reason = reg.pause_reason;
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ],
        "only the active product is reconciled"
    );
    assert_eq!(wire.count("reconcile_product"), 1);
    let skipped = e.logged("reconcile_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["product"], p2.to_string());
    assert_eq!(skipped[0]["reason"], "already paused");
    assert_eq!(skipped[0]["pause_reason_code"], reason);
    assert!(errors_logged(&e).is_empty(), "no ProductAlreadyPaused noise");
    assert!(e.logged("lost_race").is_empty(), "the paused product was not even tried");
    assert_eq!(e.chain.simulations.get(), e.chain.sends.get(), "no transaction was simulated and then refused");
    let reg = e.registry(&p2);
    assert!(!reg.active);
    assert_eq!(reg.pause_reason, reason, "the pause reason was not rewritten");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_mismatching_tally_pauses_its_product_with_reason_two_before_begin_and_the_cycle_still_runs() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    e.set_tally(99, 1);
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(wire.sent_names()[..2], s(&["reconcile_product", "begin_heartbeat"]));
    let reg = e.registry(&e.sector);
    assert!(!reg.active);
    assert_eq!(reg.pause_reason, core_vault::constants::PAUSE_RECONCILIATION_DEFICIT);
    assert_eq!(reg.pause_reason, 2);
    assert_eq!(e.alert_kinds(), vec!["product_auto_paused".to_string()]);
    assert_eq!(r.alerts.len(), 1);
    assert_eq!(r.exit_code(), 10);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

// ------------------------------------------------------------------ 10. priority fee

#[test]
fn without_a_priority_fee_every_transaction_is_exactly_one_vault_instruction() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config { priority_fee_micro: 0, ..Config::default() });
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    let txs = wire.txs.borrow().clone();
    assert_eq!(txs.len(), 4);
    for t in &txs {
        assert_eq!(t.len(), 1, "{t:?}");
        assert_ne!(t[0].name, "ComputeBudget");
    }
    assert_eq!(wire.sent_names(), s(&["reconcile_product", "begin_heartbeat", "settle_claims", "finalize_heartbeat"]));
    assert_eq!(a.stats.fee_lamports, 4 * 5_000);
    assert_eq!(a.stats.sends, 4);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn with_a_priority_fee_every_transaction_carries_a_compute_limit_and_price_and_units_are_recorded() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config { priority_fee_micro: 7_000, ..Config::default() });
    let r = a.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(r.sent.len(), 4);
    assert!(r.sent.iter().all(|t| t.units.is_some()));
    let txs = wire.txs.borrow().clone();
    let sim_units = wire.sim_units_at_send.borrow().clone();
    assert_eq!(txs.len(), 4);
    let mut fee = 0;
    for (i, t) in txs.iter().enumerate() {
        assert_eq!(t.len(), 3, "{t:?}");
        assert_eq!((t[0].name.as_str(), t[1].name.as_str()), ("ComputeBudget", "ComputeBudget"));
        assert_ne!(t[2].name, "ComputeBudget");
        // SetComputeUnitLimit = simulated units * 1.3 + 1,000; SetComputeUnitPrice = the configured price
        let used = sim_units[i].expect("the simulation reports units");
        let limit = (used * 13 / 10 + 1_000).min(1_400_000) as u32;
        assert_eq!(t[0].data, [vec![2u8], limit.to_le_bytes().to_vec()].concat(), "tx {i}");
        assert_eq!(t[1].data, [vec![3u8], 7_000u64.to_le_bytes().to_vec()].concat(), "tx {i}");
        assert_eq!(r.sent[i].units, Some(used));
        fee += 5_000 + (limit as u64 * 7_000).div_ceil(1_000_000);
    }
    assert_eq!(a.stats.fee_lamports, fee, "the fee estimate counts the priority part");
    assert!(fee > 4 * 5_000);
    let units = e.chain.units.borrow().clone();
    assert_eq!(units.len(), 4);
    assert!(units.iter().all(|u| *u > 0));
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

// ------------------------------------------------------------------ 11. fee and send caps

#[test]
fn a_fee_cap_of_one_transaction_allows_exactly_one_send_then_stops_with_a_hard_failure() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, _) = (wallet_totals(&e), pool_total(&e));
    let mut a = e.keeper_with(Config { max_fee_lamports: 5_000, ..Config::default() });

    let r = a.pass();

    assert_eq!(labels(&r), vec![reconcile_label(&e.sector)]);
    assert_eq!(r.hard_failure.as_deref(), Some("send cap reached before the cycle could begin"));
    assert_eq!(r.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), 1);
    assert_eq!(e.chain.simulations.get(), 2, "begin was simulated, then refused by the cap");
    assert_eq!(a.stats.sends, 1);
    assert_eq!(a.stats.fee_lamports, 5_000);
    let caps = e.logged("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-fee-lamports-per-run");
    assert_eq!(caps[0]["estimated_fees"], 5_000);
    assert_eq!(caps[0]["level"], "error");
    assert!(!e.vault_state().cycle_active);
    assert_eq!(wallet_totals(&e), wallets);
    assert_eq!(qs.len(), 3);
}

#[test]
fn a_fee_cap_of_three_transactions_stops_the_cycle_after_the_first_settle_batch() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let mut a = e.keeper_with(Config { max_fee_lamports: 15_000, ..Config::default() });

    let r = a.pass();

    assert_eq!(labels(&r), vec![reconcile_label(&e.sector), "begin_heartbeat".into(), "settle_claims x6".into()]);
    assert_eq!(r.hard_failure.as_deref(), Some("send cap reached (max-fee-lamports-per-run)"));
    assert_eq!(r.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), 3);
    assert_eq!(a.stats.fee_lamports, 15_000);
    let caps = e.logged("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-fee-lamports-per-run");
    assert_eq!(caps[0]["estimated_fees"], 15_000);
    let vs = e.vault_state();
    assert!(vs.cycle_active);
    assert_eq!((vs.cycle_eligible_count, vs.cycle_processed_count), (15, 6));
    let paid: u64 = qs.iter().filter(|q| e.claim(&q.claim).is_none()).map(|q| q.owed).sum();
    assert_eq!(vs.open_claims_total, qs.iter().map(|q| q.owed).sum::<u64>() - paid);
    assert_eq!(qs.iter().filter(|q| e.claim(&q.claim).is_none()).count(), 6);
}

#[test]
fn a_priority_fee_counts_against_the_fee_cap_so_a_huge_price_stops_the_very_first_send() {
    let mut e = Env::new();
    let _qs = queue_n(&mut e, 2);
    fund(&e);
    let mut a = e.keeper_with(Config { max_fee_lamports: 5_000, priority_fee_micro: 1_000_000, ..Config::default() });
    let r = a.pass();
    assert!(r.sent.is_empty());
    assert_eq!(r.hard_failure.as_deref(), Some("send cap reached during the reconcile pass"));
    assert_eq!(r.exit_code(), 20);
    assert_eq!(e.chain.sends.get(), 0, "nothing reached the node");
    assert_eq!(a.stats.fee_lamports, 0);
    let caps = e.logged("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-fee-lamports-per-run");
    assert_eq!(caps[0]["estimated_fees"], 0);
}

#[test]
fn the_send_cap_is_counted_per_keeper_and_a_failed_on_chain_send_does_not_use_it_up() {
    let mut e = Env::new();
    let _qs = queue_n(&mut e, 8);
    fund(&e);
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config { max_sends: 4, ..Config::default() });
    let mut b = e.keeper();
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_first_batch(&mut b), Sent::Done { .. }));
    });
    let r = a.pass();
    // reconcile, begin, settle x2 (the x6 lost the race and does not count), finalize = 4 accepted sends
    assert_eq!(r.hard_failure, None);
    assert_eq!(a.stats.sends, 4);
    assert_eq!(e.chain.sends.get(), 6);
    assert!(e.logged("send_cap_reached").is_empty());
    assert_eq!(e.vault_state().cycle_id, 1);
}

// ------------------------------------------------------------------ 12. review additions

/// A one-shot action for `Wire::on_send`: the admins pause `product` (a test admin pair signs; the env payer pays).
fn admin_pause_action(e: &Env, product: Pubkey) -> impl FnMut() {
    let chain = e.chain.clone();
    let (sl8, rov, payer) = (dup(&e.sl8), dup(&e.rov), dup(&e.payer));
    let ix = AdminIx::PauseProduct { product }.build(&e.keys);
    move || {
        let mut svm = chain.svm.borrow_mut();
        let bh = svm.latest_blockhash();
        let msg = Message::new_with_blockhash(std::slice::from_ref(&ix), Some(&payer.pubkey()), &bh);
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[&payer, &sl8, &rov], bh);
        svm.send_transaction(tx).expect("the admins' pause lands");
        svm.expire_blockhash();
    }
}

#[test]
fn a_product_paused_by_the_admins_just_before_the_keepers_reconcile_lands_is_a_benign_race_and_the_cycle_runs() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let ran = Rc::new(Cell::new(false));
    let (mut pause, flag) = (admin_pause_action(&e, e.sector), ran.clone());
    wire.on_send("reconcile_product", 0, move || {
        pause();
        flag.set(true);
    });

    let r = a.pass();

    assert!(ran.get(), "the admins paused the product right before the reconcile executed");
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r), s(&["begin_heartbeat", "settle_claims x3", "finalize_heartbeat"]), "the reconcile lost");
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], reconcile_label(&e.sector));
    assert_eq!(lost[0]["error"], "ProductAlreadyPaused (Custom 6014)");
    assert!(errors_logged(&e).is_empty(), "{:?}", errors_logged(&e));
    assert!(e.logged("reconcile_failed").is_empty());
    assert_eq!(wire.sent_names(), s(&["reconcile_product", "begin_heartbeat", "settle_claims", "finalize_heartbeat"]));
    let reg = e.registry(&e.sector);
    assert!(!reg.active);
    assert_eq!(reg.pause_reason, core_vault::constants::PAUSE_PLANNED_UPGRADE, "the admins' reason stands");
    assert!(r.alerts.is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn when_the_other_keeper_begins_between_the_reconcile_and_the_begin_the_keeper_does_not_send_a_begin_at_all() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    let b_result: Rc<RefCell<Option<Sent>>> = Rc::new(RefCell::new(None));
    let slot = b_result.clone();
    // B's begin lands right before A's reconcile executes; A then re-reads the vault before it would begin
    wire.on_send("reconcile_product", 0, move || {
        let w = b.read_world().unwrap();
        *slot.borrow_mut() = Some(b.send("begin_heartbeat (keeper B)", vec![ixs::begin(&b.keys, &b.payer, &w.vault)]));
    });

    let ra = a.pass();

    assert!(matches!(b_result.borrow().as_ref(), Some(Sent::Done { .. })), "B's begin landed first");
    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 0);
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "settle_claims x6".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(wire.count("begin_heartbeat"), 0, "A re-read the vault, saw the open cycle and never sent a begin");
    let not_needed = e.logged("begin_not_needed");
    assert_eq!(not_needed.len(), 1);
    assert_eq!(not_needed[0]["now_state"], "CycleActive");
    assert!(e.logged("lost_race").is_empty(), "nothing was sent that could lose");
    assert!(errors_logged(&e).is_empty());
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_eligible_count, vs.cycle_processed_count), (1, 7, 7));
    assert_eq!(e.chain.sends.get(), 1 + 4, "B's begin, then A's reconcile, two settles and the finalize");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn when_the_other_keeper_runs_the_whole_cycle_between_the_reconcile_and_the_begin_the_keeper_stops_after_the_reconcile()
{
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    let b_report: Rc<RefCell<Option<PassReport>>> = Rc::new(RefCell::new(None));
    let slot = b_report.clone();
    wire.on_send("reconcile_product", 0, move || {
        *slot.borrow_mut() = Some(b.pass());
    });

    let ra = a.pass();
    let rb = b_report.borrow_mut().take().expect("B ran");

    assert_eq!(labels(&rb).len(), 5, "B did reconcile, begin, two settles and the finalize");
    assert_eq!((ra.hard_failure.clone(), ra.exit_code()), (None, 0));
    assert_eq!(labels(&ra), vec![reconcile_label(&e.sector)]);
    assert_eq!(wire.count("begin_heartbeat"), 0);
    let not_needed = e.logged("begin_not_needed");
    assert_eq!(not_needed.len(), 1);
    assert_eq!(not_needed[0]["now_state"], "NoClaims");
    assert!(e.logged("lost_race").is_empty());
    assert_eq!(e.chain.sends.get(), 5 + 1);
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_keeper_that_dies_right_after_the_reconcile_is_replaced_by_one_that_reconciles_again_and_runs_the_whole_cycle() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let mut k1 = e.keeper_with(Config { max_sends: 1, ..Config::default() });
    let r1 = k1.pass();
    assert_eq!(labels(&r1), vec![reconcile_label(&e.sector)]);
    assert_eq!(r1.hard_failure.as_deref(), Some("send cap reached before the cycle could begin"));
    assert_eq!(r1.exit_code(), 20);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active), (0, false), "no cycle was begun");
    drop(k1);

    let mut k2 = e.keeper();
    let r2 = k2.pass();
    assert_eq!(r2.hard_failure, None);
    assert_eq!(r2.exit_code(), 0);
    assert_eq!(
        labels(&r2),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x6".into(),
            "settle_claims x3".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert!(e.registry(&e.sector).active, "a second reconcile of a matching tally changes nothing");
    assert_eq!(e.chain.landed.borrow().len(), 1 + 6, "the reconcile twice, everything else once");
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn keepers_with_different_keys_each_send_two_transactions_and_hand_over_and_the_cycle_ends_once() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 15);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let (kp_a, kp_b) = (Keypair::new(), Keypair::new());
    let start = 10_000_000_000u64;
    for k in [&kp_a, &kp_b] {
        e.chain.svm.borrow_mut().airdrop(&k.pubkey(), start).unwrap();
    }
    let cfg = Config { max_sends: 2, ..Config::default() };
    // A (a process that may send 2), B (2), then A restarted (2): every pass ends at its cap, the last one at the end
    let ra = e.keeper_for(dup(&kp_a), cfg.clone()).pass();
    let rb = e.keeper_for(dup(&kp_b), cfg.clone()).pass();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_processed_count), (1, true, 12));
    let ra2 = e.keeper_for(dup(&kp_a), cfg).pass();

    assert_eq!(labels(&ra), vec![reconcile_label(&e.sector), "begin_heartbeat".into()]);
    assert_eq!(ra.hard_failure.as_deref(), Some("send cap reached (max-sends-per-run)"));
    assert_eq!(labels(&rb), s(&["settle_claims x6", "settle_claims x6"]));
    assert_eq!(rb.hard_failure.as_deref(), Some("send cap reached (max-sends-per-run)"));
    assert_eq!(labels(&ra2), s(&["settle_claims x3", "finalize_heartbeat"]));
    assert_eq!((ra2.hard_failure.clone(), ra2.exit_code()), (None, 0));
    assert_eq!(e.chain.landed.borrow().len(), 2 + 3 + 1);
    assert_eq!(e.chain.sends.get(), 6);
    assert_eq!(e.vault_state().cycle_id, 1);
    // each key paid its own fees (5,000 lamports a transaction) and got the rent of the claims ITS settles closed
    let rent = e.chain.svm.borrow().minimum_balance_for_rent_exemption(core_vault::state::PayoutClaim::SPACE);
    let (bal_a, bal_b) = (e.chain.balance(&kp_a.pubkey()).unwrap(), e.chain.balance(&kp_b.pubkey()).unwrap());
    assert_eq!(bal_a, start - 4 * 5_000 + 3 * rent, "A: reconcile, begin, x3, finalize; closed 3 claims");
    assert_eq!(bal_b, start - 2 * 5_000 + 12 * rent, "B: two batches of six; closed 12 claims");
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_claim_queued_while_the_cycle_is_open_is_left_alone_does_not_hold_the_cycle_back_and_is_paid_by_the_next_one() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 2);
    fund(&e);
    let mut k1 = e.keeper_with(Config { max_sends: 2, ..Config::default() });
    assert_eq!(labels(&k1.pass()).len(), 2, "reconcile and begin");
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_eligible_count), (1, true, 2));

    // a third claim is requested while cycle 1 is open
    let usdc = e.usdc;
    let (late_trader, late_claim) = e.queue_claim(&usdc, 33 * M);
    let late = e.claim(&late_claim).unwrap();
    assert_eq!((late.created_in_cycle, late.last_settled_cycle, late.owed), (1, 0, 33 * M));
    let late_before = wallet_totals(&e)[late_trader];
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));

    let mut k2 = e.keeper();
    let r = k2.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r), s(&["settle_claims x2", "finalize_heartbeat"]), "the late claim is not in the batch");
    assert!(e.logged("lost_race").is_empty());
    // the only error line is the first keeper's own send cap
    assert!(errors_logged(&e).iter().all(|l| l["event"] == "send_cap_reached"), "{:?}", errors_logged(&e));
    let after = wallet_totals(&e);
    for q in &qs {
        assert!(e.claim(&q.claim).is_none());
        assert_eq!(after[q.trader] - wallets[q.trader], q.owed);
    }
    assert_eq!(after[late_trader], late_before, "the late claim was not touched");
    let late = e.claim(&late_claim).unwrap();
    assert_eq!((late.owed, late.last_settled_cycle), (33 * M, 0));
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_eligible_count, vs.cycle_processed_count), (1, false, 2, 2));
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, 33 * M));
    assert_eq!(pools - pool_total(&e), qs.iter().map(|q| q.owed).sum::<u64>());

    // the next cycle pays it, to the base unit
    e.set_time(T0 + GAP);
    let r = k2.pass();
    assert_eq!(r.hard_failure, None);
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert!(e.claim(&late_claim).is_none());
    assert_eq!(wallet_totals(&e)[late_trader] - late_before, 33 * M);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.open_claims_count, vs.open_claims_total), (2, 0, 0));
}

#[test]
fn a_cycle_that_makes_no_progress_for_three_rounds_stops_the_pass_with_a_clear_failure_and_a_later_pass_finishes_it() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    // the node acknowledges the settle three times and never executes it
    for _ in 0..3 {
        wire.swallow_next("settle_claims");
    }

    let r = a.pass();

    assert_eq!(r.hard_failure.as_deref(), Some("the cycle is not progressing: 0 of 3 claims processed after 3 rounds"));
    assert_eq!(r.exit_code(), 20);
    let stalled = e.logged("cycle_stalled");
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0]["level"], "error");
    assert_eq!(wire.count("settle_claims"), 3, "rounds 1, 2 and 3 each sent the batch; round 4 gave up before sending");
    assert_eq!(wire.count("finalize_heartbeat"), 0);
    assert_eq!(e.chain.landed.borrow().len(), 2, "only reconcile and begin ever landed");
    assert_eq!(wallet_totals(&e), wallets, "nothing was paid");
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (true, 0, 3));

    // the node works again: a later pass settles once and finalizes (no second begin, no double payment)
    let r2 = a.pass();
    assert_eq!(r2.hard_failure, None);
    assert_eq!(labels(&r2), s(&["settle_claims x3", "finalize_heartbeat"]));
    assert_eq!(wire.count("begin_heartbeat"), 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_poisoned_claim_that_is_alone_in_its_batch_is_quarantined_at_once_and_ends_the_pass_with_a_stuck_cycle() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 1);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let bad = qs[0].claim;
    let acct = e.chain.svm.borrow().get_account(&bad).unwrap();
    let mut data = acct.data.clone();
    data[136] = 7; // the kind byte: an unknown kind is InvalidClaim
    e.chain.svm.borrow_mut().set_account(bad, RawAcct { data, ..acct }).unwrap();
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r = a.pass();

    assert_eq!(labels(&r), vec![reconcile_label(&e.sector), "begin_heartbeat".into()]);
    assert_eq!(
        r.hard_failure.as_deref(),
        Some("the cycle expects 1 claims processed, 0 are, and none is left that this keeper can settle (quarantined: 1)")
    );
    assert_eq!(r.exit_code(), 20);
    assert!(e.logged("batch_split").is_empty(), "a batch of one is not split");
    let loud = e.logged("settle_refused");
    assert_eq!(loud.len(), 1, "refused once, never retried in this pass: {loud:?}");
    assert_eq!(loud[0]["label"], "settle_claims x1");
    assert_eq!(loud[0]["error"], "InvalidClaim (Custom 6031)");
    assert_eq!(e.logged("claim_quarantined").len(), 1);
    assert_eq!(e.logged("claim_quarantined")[0]["level"], "error");
    assert_eq!(e.logged("cycle_stuck").len(), 1);
    assert!(e.logged("cycle_stalled").is_empty(), "the quarantine, not the stall guard, ended the pass");
    assert_eq!(wire.txs.borrow().len(), 2, "the refused settle was found by simulation and never sent");
    assert_eq!(e.chain.simulations.get(), 3);
    assert_eq!(wallet_totals(&e), wallets);
    assert_eq!(pool_total(&e), pools);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (true, 0, 1));
}

#[test]
fn a_settle_that_the_node_accepted_but_that_failed_on_chain_with_claim_already_settled_is_the_same_benign_race() {
    // the real-world shape of a lost race: the node's preflight passed, the transaction was accepted and only the
    // signature status says it failed
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    let available = underfund(&e, 40 * M, 30 * M);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    wire.late_errors.set(true);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_first_batch(&mut b), Sent::Done { .. }));
    });

    let ra = a.pass();

    assert_eq!(ra.hard_failure, None);
    assert_eq!(ra.exit_code(), 10, "the pools no longer cover what is owed");
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x2".into(),
            "finalize_heartbeat".into()
        ],
        "the failed batch is not reported as sent"
    );
    let lost = e.logged("lost_race");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["label"], "settle_claims x6");
    assert_eq!(lost[0]["error"], "ClaimAlreadySettled (Custom 6030)");
    assert_eq!(wire.failed_sigs.borrow().len(), 1, "exactly one transaction failed on chain");
    assert!(errors_logged(&e).is_empty(), "{:?}", errors_logged(&e));
    assert_eq!(a.stats.sends, 5, "the node accepted the failed transaction, so it counts as a send");
    assert_eq!(e.chain.landed.borrow().len(), 5);
    assert_partially_paid_once(&e, &wallets, pools, &qs, available, 1);
}

#[test]
fn a_transaction_whose_blockhash_expired_before_it_was_seen_is_reported_unconfirmed_without_waiting_out_the_timeout() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 7);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    wire.short_blockhash.set(true);
    // the default confirm timeout is 90 s = 180 polls: the status stays "unknown" for 50 calls, but the keeper must
    // give up at the FIRST poll of each send, because the chain is already past the blockhash's last valid height
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let (lite, late) = (e.chain.clone(), wire.height_past_expiry.clone());
    wire.on_send("settle_claims", 0, move || {
        lite.status_unknown.set(50);
        late.set(true);
    });

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    // the chain stays "past the blockhash" and the status stays unknown, so the settle and the next two sends all
    // give up at their first poll: 3 unconfirmed transactions, 3 polls spent (not 3 x 180)
    let un = e.logged("unconfirmed");
    assert_eq!(
        un.iter().map(|l| l["label"].as_str().unwrap()).collect::<Vec<_>>(),
        ["settle_claims x6", "settle_claims x1", "finalize_heartbeat"]
    );
    assert_eq!(e.chain.status_unknown.get(), 50 - 3, "one status poll each");
    // it did land, so the re-read shows it and nothing is sent twice
    assert_eq!(
        labels(&r),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x1".into(),
            "finalize_heartbeat".into()
        ]
    );
    assert_eq!(wire.count("settle_claims"), 2);
    assert_eq!(e.chain.landed.borrow().len(), 5);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn claims_part_paid_by_the_other_keeper_before_the_keeper_re_reads_them_are_skipped_without_sending_or_losing_a_race() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    let available = underfund(&e, 40 * M, 30 * M);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let mut b = e.keeper();
    // B settles A's SECOND batch (the last two claims) while A's first batch executes: those two claims stay open,
    // part paid, marked as processed in this cycle, so A's re-read right before the second batch must drop them
    wire.on_send("settle_claims", 0, move || {
        assert!(matches!(settle_batch_n(&mut b, 1), Sent::Done { .. }));
    });

    let ra = a.pass();

    assert_eq!(ra.hard_failure, None);
    assert_eq!(
        labels(&ra),
        vec![
            reconcile_label(&e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "finalize_heartbeat".into()
        ]
    );
    let done = e.logged("batch_already_done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["claims"], 2);
    assert!(e.logged("lost_race").is_empty(), "A never sent the second batch");
    assert!(errors_logged(&e).is_empty(), "{:?}", errors_logged(&e));
    assert_eq!(wire.count("settle_claims"), 1);
    assert_eq!(e.chain.landed.borrow().len(), 5, "reconcile, begin, B's x2, A's x6, finalize");
    assert_partially_paid_once(&e, &wallets, pools, &qs, available, 1);
}

#[test]
fn batches_follow_ascending_claim_address_order_whatever_order_the_node_lists_the_claims_in() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 8);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    wire.reverse_claims.set(true);
    assert!(wire.claim_addresses().unwrap().windows(2).all(|w| w[0].to_bytes() > w[1].to_bytes()), "listed descending");
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    let mut sorted: Vec<Pubkey> = qs.iter().map(|q| q.claim).collect();
    sorted.sort_by_key(|k| k.to_bytes());
    let settles: Vec<Vec<Pubkey>> = wire
        .txs
        .borrow()
        .iter()
        .flatten()
        .filter(|ix| ix.name == "settle_claims")
        .map(|ix| ix.accounts[7..].chunks(3).map(|t| t[0]).collect())
        .collect();
    assert_eq!(settles, vec![sorted[..6].to_vec(), sorted[6..].to_vec()]);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn reads_that_are_rate_limited_are_retried_with_backoff_and_the_pass_goes_on() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let slept = Rc::new(RefCell::new(vec![]));
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    a.sleeper = Box::new(Rec(slept.clone()));
    wire.read_errors.set(2);

    let r = a.pass();

    assert_eq!(wire.read_errors.get(), 0, "both failures were consumed");
    assert_eq!(*slept.borrow(), vec![Duration::from_millis(1000), Duration::from_millis(2000)]);
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r).len(), 4);
    assert!(errors_logged(&e).is_empty());
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn reads_that_stay_rate_limited_past_the_retries_fail_the_pass_with_the_node_error_and_send_nothing() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, _) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let slept = Rc::new(RefCell::new(vec![]));
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    a.sleeper = Box::new(Rec(slept.clone()));
    wire.read_errors.set(1_000);

    let r = a.pass();

    assert_eq!(r.hard_failure.as_deref(), Some("HTTP 429"));
    assert_eq!(r.exit_code(), 20);
    assert_eq!(
        *slept.borrow(),
        [1000u64, 2000, 4000, 8000].map(Duration::from_millis).to_vec(),
        "max_retries 4: four pauses, doubling"
    );
    assert_eq!(1_000 - wire.read_errors.get(), 5, "one try and four retries");
    let c = e.logged("cannot_read_chain");
    assert_eq!(c.len(), 1);
    assert_eq!(c[0]["error"], "HTTP 429");
    assert!(r.sent.is_empty() && wire.txs.borrow().is_empty());
    assert_eq!(wallet_totals(&e), wallets);
    assert_eq!(e.vault_state().cycle_id, 0);
    assert_eq!(qs.len(), 3);
}

#[test]
fn a_failing_status_poll_is_not_the_end_of_the_send_the_keeper_polls_again() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());
    let w = wire.status_errors.clone();
    wire.on_send("settle_claims", 0, move || w.set(3));

    let r = a.pass();

    assert_eq!(wire.status_errors.get(), 0, "three status polls failed");
    assert_eq!(r.hard_failure, None);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(labels(&r).len(), 4);
    assert!(r.sent.iter().all(|t| t.units.is_some()), "every send ended confirmed");
    assert!(e.logged("unconfirmed").is_empty());
    assert_eq!(wire.count("settle_claims"), 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_simulation_without_a_unit_count_gets_a_200000_unit_base_for_the_compute_limit() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 3);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let wire = Wire::new(&e.chain);
    wire.hide_units.set(true);
    let mut a = keeper_on(&e, wire.clone(), Config { priority_fee_micro: 7_000, ..Config::default() });

    let r = a.pass();

    assert_eq!(r.hard_failure, None);
    assert_eq!(r.sent.len(), 4);
    assert!(r.sent.iter().all(|t| t.units.is_none()), "no unit count was reported");
    let limit = 200_000u32 * 13 / 10 + 1_000;
    assert_eq!(limit, 261_000);
    for t in wire.txs.borrow().iter() {
        assert_eq!(t[0].data, [vec![2u8], limit.to_le_bytes().to_vec()].concat());
        assert_eq!(t[1].data, [vec![3u8], 7_000u64.to_le_bytes().to_vec()].concat());
    }
    let per_tx = 5_000 + (261_000u64 * 7_000).div_ceil(1_000_000);
    assert_eq!(per_tx, 6_827);
    assert_eq!(a.stats.fee_lamports, 4 * per_tx);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}

#[test]
fn a_claim_refused_in_one_pass_is_tried_again_by_the_next_pass_because_nothing_is_remembered() {
    let mut e = Env::new();
    let qs = queue_n(&mut e, 2);
    fund(&e);
    let (wallets, pools) = (wallet_totals(&e), pool_total(&e));
    let bad = qs[1].claim;
    let good_acct = e.chain.svm.borrow().get_account(&bad).unwrap();
    let mut data = good_acct.data.clone();
    data[136] = 7;
    e.chain.svm.borrow_mut().set_account(bad, RawAcct { data, ..good_acct.clone() }).unwrap();
    let wire = Wire::new(&e.chain);
    let mut a = keeper_on(&e, wire.clone(), Config::default());

    let r1 = a.pass();

    assert_eq!(
        r1.hard_failure.as_deref(),
        Some("the cycle expects 2 claims processed, 1 are, and none is left that this keeper can settle (quarantined: 1)")
    );
    assert_eq!(e.vault_state().cycle_processed_count, 1);

    // whatever was wrong with the claim is repaired; the very same keeper object settles it on its next pass
    e.chain.svm.borrow_mut().set_account(bad, good_acct).unwrap();
    let r2 = a.pass();
    assert_eq!(r2.hard_failure, None);
    assert_eq!(r2.exit_code(), 0);
    assert_eq!(labels(&r2), s(&["settle_claims x1", "finalize_heartbeat"]));
    assert_eq!(wire.count("begin_heartbeat"), 1);
    assert_all_paid_exactly_once(&e, &wallets, pools, &qs);
}
