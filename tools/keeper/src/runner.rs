//! One pass of the keeper: read the chain, decide, send, re-read. The keeper keeps no state of its own
//! between passes (only logs and the per-run send counters), so a crash or restart at any point is safe
//! and a second keeper running at the same time is harmless.

use std::collections::HashSet;
use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountDeserialize;
use anchor_spl::token::spl_token::{solana_program::program_pack::Pack, state::Account as TokAcc, state::AccountState};
use serde_json::{json, Value};
use setl8_admin::admin_ix::Keys;
use setl8_admin::cluster::Cluster;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;

use crate::alerts::{evaluate, Alert, Thresholds, SKIPPED_CYCLES};
use crate::chain::{decode_claim, Chain, ChainError, TxErr, TxStatus};
use crate::config::Config;
use crate::errors::{classify, describe_code, is_per_claim, Class};
use crate::ixs;
use crate::log::Logger;
use crate::model::{ClaimView, PoolView, ProductView, VaultView, World};
use crate::plan::{batches, eligible_unprocessed, products_to_reconcile, schedule, Schedule};
use crate::safety::check_genesis;

/// Why the genesis check failed: the node answers with another cluster's genesis (a refusal), or cannot be asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenesisError {
    Mismatch(String),
    Unreadable(String),
}

impl GenesisError {
    pub fn message(&self) -> &str {
        match self {
            GenesisError::Mismatch(m) | GenesisError::Unreadable(m) => m,
        }
    }
}

/// A cluster name that is safe to log: a custom cluster is shown by host only (a URL can carry an API key).
pub fn cluster_label(c: &Cluster) -> String {
    crate::safety::cluster_label(c)
}

pub trait Sleeper {
    fn sleep(&self, d: Duration);
}

pub struct RealSleeper;
impl Sleeper for RealSleeper {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// For tests: never sleeps.
pub struct NoSleep;
impl Sleeper for NoSleep {
    fn sleep(&self, _d: Duration) {}
}

/// What one send attempt came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    Done {
        signature: String,
        units: Option<u64>,
    },
    /// Dry-run: logged, not sent.
    DryRun,
    /// Another keeper (or the state) got there first: a benign race.
    Race(String),
    /// The program or the node refused it; `code` is the vault's custom code when there is one.
    Failed {
        code: Option<u32>,
        text: String,
        class: Class,
    },
    /// Sent but not seen confirmed before the blockhash expired or the timeout: re-read before anything else.
    Unconfirmed(String),
    /// A send cap was reached.
    CapReached(&'static str),
    /// Transient trouble outlasted the retries.
    Transient(String),
}

#[derive(Clone, Debug)]
pub struct SentTx {
    pub label: String,
    pub signature: String,
    pub units: Option<u64>,
}

#[derive(Debug, Default)]
pub struct PassReport {
    pub sent: Vec<SentTx>,
    /// Something changed on chain because of this pass.
    pub progress: bool,
    pub alerts: Vec<Alert>,
    pub hard_failure: Option<String>,
    /// Why nothing was done, when nothing was.
    pub idle: Option<String>,
    pub would_send: Vec<String>,
}

impl PassReport {
    /// 0 nothing to do or progress made, 10 an alert condition is present, 20 a hard failure.
    pub fn exit_code(&self) -> i32 {
        if self.hard_failure.is_some() {
            20
        } else if !self.alerts.is_empty() {
            10
        } else {
            0
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub sends: u32,
    pub fee_lamports: u64,
}

pub struct Keeper<C: Chain> {
    pub chain: C,
    /// `None` only in dry-run / status, where nothing is signed.
    pub signer: Option<Keypair>,
    pub payer: Pubkey,
    pub keys: Keys,
    pub cluster: Cluster,
    pub cfg: Config,
    pub log: Logger,
    pub sleeper: Box<dyn Sleeper>,
    pub stats: Stats,
    quarantine: HashSet<Pubkey>,
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << attempt.min(6)).min(20_000))
}

impl<C: Chain> Keeper<C> {
    pub fn new(
        chain: C,
        signer: Option<Keypair>,
        payer: Pubkey,
        keys: Keys,
        cluster: Cluster,
        cfg: Config,
        log: Logger,
    ) -> Keeper<C> {
        Keeper {
            chain,
            signer,
            payer,
            keys,
            cluster,
            cfg,
            log,
            sleeper: Box::new(RealSleeper),
            stats: Stats::default(),
            quarantine: HashSet::new(),
        }
    }

    // ---------------------------------------------------------------- reading the chain

    /// Retries transient chain errors with backoff.
    fn read<T>(&self, mut f: impl FnMut() -> Result<T, ChainError>) -> Result<T, ChainError> {
        let mut attempt = 0;
        loop {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) if e.transient() && attempt < self.cfg.max_retries => {
                    attempt += 1;
                    self.sleeper.sleep(backoff(attempt));
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn verify_genesis(&mut self) -> Result<(), GenesisError> {
        let live = self
            .read(|| self.chain.genesis_hash())
            .map_err(|e| GenesisError::Unreadable(format!("cannot read the genesis hash: {e}")))?;
        match check_genesis(&self.cluster, &live) {
            Ok(()) => Ok(()),
            Err(e) => {
                let c = cluster_label(&self.cluster);
                let a = Alert {
                    kind: "genesis_mismatch",
                    key: "genesis".into(),
                    detail: json!({"genesis": live, "cluster": c}),
                };
                self.log.alert(&a, &c);
                Err(GenesisError::Mismatch(e.0))
            }
        }
    }

    fn read_vault(&self) -> Result<VaultView, String> {
        let acc = self.read(|| self.chain.account(&self.keys.vault())).map_err(|e| e.to_string())?;
        let acc = acc.ok_or_else(|| {
            format!(
                "the vault {} does not exist on this cluster (has init_vault run, and is --program-id right?)",
                self.keys.vault()
            )
        })?;
        if acc.owner != self.keys.program_id {
            return Err(format!(
                "the vault account is owned by {}, not by the vault program {}",
                acc.owner, self.keys.program_id
            ));
        }
        let vs = core_vault::state::VaultState::try_deserialize(&mut acc.data.as_slice())
            .map_err(|_| "the vault account does not decode as VaultState".to_string())?;
        Ok(VaultView::from(&vs))
    }

    fn read_pool(&self, key: &Pubkey) -> Result<PoolView, String> {
        let acc = self
            .read(|| self.chain.account(key))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("pool {key} is missing"))?;
        let t = TokAcc::unpack(&acc.data).map_err(|_| format!("pool {key} is not a token account"))?;
        Ok(PoolView { amount: t.amount, frozen: t.state == AccountState::Frozen })
    }

    fn read_products(&self) -> Result<Vec<ProductView>, String> {
        let regs = self.read(|| self.chain.registries()).map_err(|e| e.to_string())?;
        let mut out = vec![];
        for (addr, acc) in regs {
            if acc.owner != self.keys.program_id {
                continue;
            }
            if let Ok(r) = core_vault::state::ProductRegistry::try_deserialize(&mut acc.data.as_slice()) {
                out.push(ProductView {
                    id: r.product_program_id,
                    registry: addr,
                    active: r.active,
                    pause_reason: r.pause_reason,
                });
            }
        }
        out.sort_by_key(|p| p.id.to_bytes());
        Ok(out)
    }

    pub fn read_world(&self) -> Result<World, String> {
        let vault = self.read_vault()?;
        Ok(World {
            now: self.read(|| self.chain.now()).map_err(|e| e.to_string())?,
            usdc: self.read_pool(&vault.usdc_pool)?,
            usdt: self.read_pool(&vault.usdt_pool)?,
            products: self.read_products()?,
            payer_balance: self.read(|| self.chain.balance(&self.payer)).map_err(|e| e.to_string())?,
            vault,
        })
    }

    /// Every claim on chain, freshly read (addresses from `getProgramAccounts`, or from `--claims-file`).
    pub fn load_claims(&self) -> Result<Vec<ClaimView>, String> {
        let addrs: Vec<Pubkey> = match &self.cfg.claims_file {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|_| "cannot read the --claims-file".to_string())?;
                let mut v = vec![];
                for line in text.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    v.push(line.parse::<Pubkey>().map_err(|_| format!("--claims-file: '{}' is not a public key", line.chars().take(50).collect::<String>()))?);
                }
                v
            }
            None => self.read(|| self.chain.claim_addresses()).map_err(|e| match e {
                ChainError::Unsupported(m) => format!("this RPC provider does not support getProgramAccounts ({m}); pass --claims-file <list of claim addresses>"),
                other => other.to_string(),
            })?,
        };
        self.fetch_claims(&addrs)
    }

    fn fetch_claims(&self, addrs: &[Pubkey]) -> Result<Vec<ClaimView>, String> {
        let accs = self.read(|| self.chain.accounts(addrs)).map_err(|e| e.to_string())?;
        Ok(addrs
            .iter()
            .zip(accs)
            .filter_map(|(a, acc)| acc.and_then(|acc| decode_claim(*a, &acc, &self.keys.program_id)))
            .collect())
    }

    /// Open claims whose destinations have stayed unusable through `SKIPPED_CYCLES` cycles.
    fn skip_suspects(&self, v: &VaultView, claims: &[ClaimView]) -> Vec<Pubkey> {
        let mut out = vec![];
        for c in claims.iter().filter(|c| c.owed > 0 && c.last_settled_cycle >= c.created_in_cycle + SKIPPED_CYCLES) {
            let atas = [
                (crate::plan::associated_token_address(&c.trader_wallet, &v.usdc_mint), v.usdc_mint),
                (crate::plan::associated_token_address(&c.trader_wallet, &v.usdt_mint), v.usdt_mint),
            ];
            let usable = atas.iter().all(|(a, mint)| {
                self.chain
                    .account(a)
                    .ok()
                    .flatten()
                    .and_then(|acc| TokAcc::unpack(&acc.data).ok())
                    .map(|t| t.owner == c.trader_wallet && t.mint == *mint && t.state == AccountState::Initialized)
                    .unwrap_or(false)
            });
            if !usable {
                out.push(c.address);
            }
        }
        out
    }

    fn thresholds(&self) -> Thresholds {
        Thresholds { min_payer_balance: self.cfg.min_payer_balance }
    }

    fn raise(&mut self, alerts: Vec<Alert>, rep: &mut PassReport) {
        let cluster = cluster_label(&self.cluster);
        for a in alerts {
            if !rep.alerts.iter().any(|x| x.kind == a.kind && x.key == a.key) {
                self.log.alert(&a, &cluster);
                rep.alerts.push(a);
            }
        }
    }

    // ---------------------------------------------------------------- sending

    fn build_tx(
        &self,
        ixs: &[anchor_lang::solana_program::instruction::Instruction],
        bh: solana_hash::Hash,
    ) -> Result<Vec<u8>, String> {
        let signer = self.signer.as_ref().ok_or("no signing key")?;
        let msg = Message::new_with_blockhash(ixs, Some(&self.payer), &bh);
        let bytes = msg.serialize();
        let sig = signer.try_sign_message(&bytes).map_err(|_| "signing failed".to_string())?;
        let mut tx = vec![1u8];
        tx.extend_from_slice(sig.as_ref());
        tx.extend_from_slice(&bytes);
        Ok(tx)
    }

    fn summarize(ixs: &[anchor_lang::solana_program::instruction::Instruction]) -> Value {
        json!(ixs.iter().map(|i| json!({"program": i.program_id.to_string(), "accounts": i.accounts.len(), "data_bytes": i.data.len()})).collect::<Vec<_>>())
    }

    fn classify_err(&mut self, label: &str, err: &TxErr) -> Sent {
        match err.custom {
            Some(code) => {
                let class = classify(code);
                match class {
                    Class::Race => {
                        self.log.event("info", "lost_race", json!({"label": label, "error": describe_code(code), "note": "another keeper or the state moved first; re-reading"}));
                        Sent::Race(describe_code(code))
                    }
                    Class::Loud if label.starts_with("settle_claims") && is_per_claim(code) => {
                        // a rival that paid a claim in full closes its account, which looks like InvalidClaim:
                        // whether this is an error is decided by the caller after re-reading the claims
                        self.log.event("warn", "settle_refused", json!({"label": label, "error": describe_code(code)}));
                        Sent::Failed { code: Some(code), text: describe_code(code), class }
                    }
                    Class::Loud => {
                        self.log.event("error", "refused_loud", json!({"label": label, "error": describe_code(code), "note": "wrong address or malformed input: not retried"}));
                        Sent::Failed { code: Some(code), text: describe_code(code), class }
                    }
                    Class::Other => {
                        self.log.event("error", "refused", json!({"label": label, "error": describe_code(code)}));
                        Sent::Failed { code: Some(code), text: describe_code(code), class }
                    }
                }
            }
            None => {
                self.log.event(
                    "error",
                    "refused",
                    json!({"label": label, "error": err.text.chars().take(200).collect::<String>()}),
                );
                Sent::Failed { code: None, text: err.text.clone(), class: Class::Other }
            }
        }
    }

    /// Simulate, send and wait for one transaction made of `ixs`. Only the four permissionless instructions
    /// are ever passed here by this module.
    pub fn send(&mut self, label: &str, ixs: Vec<anchor_lang::solana_program::instruction::Instruction>) -> Sent {
        if self.cfg.dry_run {
            self.log.event("info", "would_send", json!({"label": label, "instructions": Self::summarize(&ixs)}));
            return Sent::DryRun;
        }
        if self.signer.is_none() {
            return Sent::Failed { code: None, text: "no signing key".into(), class: Class::Other };
        }
        if self.stats.sends >= self.cfg.max_sends {
            self.log.event("error", "send_cap_reached", json!({"cap": "max-sends-per-run", "sends": self.stats.sends}));
            return Sent::CapReached("max-sends-per-run");
        }
        let mut last = String::from("no attempt");
        for attempt in 0..=self.cfg.max_retries {
            if attempt > 0 {
                self.sleeper.sleep(backoff(attempt));
            }
            let (bh, last_valid) = match self.chain.blockhash() {
                Ok(x) => x,
                Err(e) if e.transient() => {
                    last = e.to_string();
                    continue;
                }
                Err(e) => return Sent::Transient(e.to_string()),
            };
            let tx = match self.build_tx(&ixs, bh) {
                Ok(t) => t,
                Err(e) => return Sent::Failed { code: None, text: e, class: Class::Other },
            };
            let sim = match self.chain.simulate(&tx) {
                Ok(s) => s,
                Err(e) if e.transient() => {
                    last = e.to_string();
                    continue;
                }
                Err(ChainError::Rejected(err)) => return self.classify_err(label, &err),
                Err(e) => return Sent::Transient(e.to_string()),
            };
            if let Some(err) = &sim.err {
                return self.classify_err(label, err);
            }
            // a priority fee is the only reason to add ComputeBudget instructions
            let (tx, fee_estimate) = if self.cfg.priority_fee_micro > 0 {
                let limit = (sim.units.unwrap_or(200_000) * 13 / 10 + 1_000).min(1_400_000) as u32;
                let mut with: Vec<_> = ixs::compute_budget(limit, self.cfg.priority_fee_micro).to_vec();
                with.extend(ixs.iter().cloned());
                match self.build_tx(&with, bh) {
                    Ok(t) => (t, 5_000 + (limit as u64 * self.cfg.priority_fee_micro).div_ceil(1_000_000)),
                    Err(e) => return Sent::Failed { code: None, text: e, class: Class::Other },
                }
            } else {
                (tx, 5_000)
            };
            if self.stats.fee_lamports + fee_estimate > self.cfg.max_fee_lamports {
                self.log.event(
                    "error",
                    "send_cap_reached",
                    json!({"cap": "max-fee-lamports-per-run", "estimated_fees": self.stats.fee_lamports}),
                );
                return Sent::CapReached("max-fee-lamports-per-run");
            }
            match self.chain.send(&tx) {
                Ok(sig) => {
                    self.stats.sends += 1;
                    self.stats.fee_lamports += fee_estimate;
                    return self.wait(label, &sig, last_valid, sim.units);
                }
                Err(ChainError::Rejected(err)) => return self.classify_err(label, &err),
                Err(e) if e.transient() => {
                    last = e.to_string();
                    continue;
                }
                Err(e) => return Sent::Transient(e.to_string()),
            }
        }
        self.log.event("warn", "transient_failure", json!({"label": label, "error": last}));
        Sent::Transient(last)
    }

    fn wait(&mut self, label: &str, sig: &str, last_valid: u64, units: Option<u64>) -> Sent {
        let polls = (self.cfg.confirm_timeout_secs * 2).max(1);
        for _ in 0..polls {
            match self.chain.status(sig) {
                Ok(Some(TxStatus::Confirmed)) => {
                    self.log.event("info", "sent", json!({"label": label, "signature": sig, "units": units}));
                    return Sent::Done { signature: sig.to_string(), units };
                }
                Ok(Some(TxStatus::Failed(err))) => return self.classify_err(label, &err),
                Ok(None) => {
                    if self.chain.block_height().map(|h| h > last_valid).unwrap_or(false) {
                        break;
                    }
                }
                Err(_) => {}
            }
            self.sleeper.sleep(Duration::from_millis(500));
        }
        self.log.event("warn", "unconfirmed", json!({"label": label, "signature": sig, "note": "not confirmed before the blockhash expired or the timeout; state will be re-read before anything is resent"}));
        Sent::Unconfirmed(sig.to_string())
    }

    fn record(&mut self, rep: &mut PassReport, label: &str, s: &Sent) {
        match s {
            Sent::Done { signature, units } => {
                rep.progress = true;
                rep.sent.push(SentTx { label: label.to_string(), signature: signature.clone(), units: *units });
            }
            Sent::Unconfirmed(sig) => {
                rep.sent.push(SentTx { label: label.to_string(), signature: sig.clone(), units: None })
            }
            Sent::DryRun => rep.would_send.push(label.to_string()),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- one pass

    pub fn pass(&mut self) -> PassReport {
        let mut rep = PassReport::default();
        self.quarantine.clear();
        if let Err(e) = self.verify_genesis() {
            rep.hard_failure = Some(e.message().to_string());
            return rep;
        }
        let world = match self.read_world() {
            Ok(w) => w,
            Err(e) => {
                self.log.event("error", "cannot_read_chain", json!({"error": e}));
                rep.hard_failure = Some(e);
                return rep;
            }
        };
        let th = self.thresholds();
        let a = evaluate(&world, &th, &[]);
        self.raise(a, &mut rep);

        match schedule(&world.vault, world.now) {
            Schedule::CycleActive => self.run_cycle(&mut rep),
            Schedule::NoClaims => {
                rep.idle =
                    Some("no open claims: no cycle is begun (an empty cycle would only burn the 5-day slot)".into());
                self.log.event("info", "idle", json!({"reason": "no_open_claims"}));
            }
            Schedule::Wait { seconds_left } => {
                rep.idle = Some(format!("next cycle may begin in {seconds_left} s"));
                self.log.event("info", "idle", json!({"reason": "gap_not_over", "seconds_left": seconds_left}));
            }
            Schedule::Begin => {
                // know that the claims can be listed BEFORE anything is sent: a cycle begun without being able to
                // settle it would only sit open
                if !self.cfg.dry_run {
                    if let Err(e) = self.load_claims() {
                        self.log.event("error", "cannot_load_claims", json!({"error": e, "note": "nothing was sent"}));
                        rep.hard_failure = Some(e);
                    }
                }
                if rep.hard_failure.is_none() {
                    self.reconcile_all(&world, &mut rep);
                }
                if rep.hard_failure.is_none() {
                    self.begin(&mut rep);
                }
                if self.cfg.dry_run {
                    self.dry_run_cycle(&mut rep);
                } else if rep.hard_failure.is_none() {
                    // a cycle is open now (ours, or another keeper's that won the race): continue it
                    if self.read_vault().map(|v| v.cycle_active).unwrap_or(false) {
                        self.run_cycle(&mut rep);
                    }
                }
            }
        }

        // the state after our work: alerts must describe it, not the state we started from
        if rep.progress {
            if let Ok(w) = self.read_world() {
                let a = evaluate(&w, &th, &[]);
                self.raise(a, &mut rep);
            }
        }
        rep
    }

    fn reconcile_all(&mut self, world: &World, rep: &mut PassReport) {
        let (send, skipped) = products_to_reconcile(&world.products);
        for p in &skipped {
            self.log.event(
                "info",
                "reconcile_skipped",
                json!({"product": p.id.to_string(), "reason": "already paused", "pause_reason_code": p.pause_reason}),
            );
        }
        let payer = self.payer;
        for p in send {
            let label = format!("reconcile_product {}", p.id);
            let s = self.send(&label, vec![ixs::reconcile(&self.keys, &payer, &p.id)]);
            self.record(rep, &label, &s);
            match &s {
                Sent::Failed { class: Class::Loud, .. } => {
                    self.log.event("error", "reconcile_failed", json!({"product": p.id.to_string(), "note": "logged, not retried; the cycle goes on without it"}));
                }
                Sent::CapReached(_) => {
                    rep.hard_failure = Some("send cap reached during the reconcile pass".into());
                    return;
                }
                // the node would not take it at all: do not begin a cycle on a reconcile pass that did not happen
                Sent::Transient(t) => {
                    rep.hard_failure = Some(format!("cannot send reconcile_product: {t}"));
                    return;
                }
                Sent::Failed { code: None, text, .. } => {
                    rep.hard_failure = Some(format!("cannot send reconcile_product: {text}"));
                    return;
                }
                _ => {}
            }
        }
        // a mismatching tally pauses its product: say so now
        if let Ok(products) = self.read_products() {
            let mut w = world.clone();
            w.products = products;
            let a: Vec<Alert> =
                evaluate(&w, &self.thresholds(), &[]).into_iter().filter(|a| a.kind == "product_auto_paused").collect();
            self.raise(a, rep);
        }
    }

    fn begin(&mut self, rep: &mut PassReport) {
        // the clock may have moved and another keeper may have begun: decide again on fresh state
        let (vault, now) = match (self.read_vault(), self.read(|| self.chain.now())) {
            (Ok(v), Ok(n)) => (v, n),
            (Err(e), _) => {
                rep.hard_failure = Some(e);
                return;
            }
            (_, Err(e)) => {
                rep.hard_failure = Some(e.to_string());
                return;
            }
        };
        match schedule(&vault, now) {
            Schedule::Begin => {}
            other => {
                self.log.event("info", "begin_not_needed", json!({"now_state": format!("{other:?}")}));
                return;
            }
        }
        let payer = self.payer;
        let s = self.send("begin_heartbeat", vec![ixs::begin(&self.keys, &payer, &vault)]);
        self.record(rep, "begin_heartbeat", &s);
        match s {
            Sent::CapReached(_) => rep.hard_failure = Some("send cap reached before the cycle could begin".into()),
            Sent::Transient(t) => rep.hard_failure = Some(format!("cannot send begin_heartbeat: {t}")),
            Sent::Failed { text, .. } => rep.hard_failure = Some(format!("begin_heartbeat was refused: {text}")),
            _ => {}
        }
    }

    fn run_cycle(&mut self, rep: &mut PassReport) {
        let mut rounds = 0;
        let mut last_processed = u64::MAX;
        let mut stalled = 0;
        loop {
            rounds += 1;
            if rounds > 500 {
                rep.hard_failure = Some("gave up after 500 rounds in one cycle".into());
                return;
            }
            let vault = match self.read_vault() {
                Ok(v) => v,
                Err(e) => {
                    rep.hard_failure = Some(e);
                    return;
                }
            };
            if !vault.cycle_active {
                return;
            }
            if vault.cycle_processed_count == last_processed {
                stalled += 1;
                if stalled >= 3 {
                    let msg = format!(
                        "the cycle is not progressing: {} of {} claims processed after {} rounds",
                        vault.cycle_processed_count, vault.cycle_eligible_count, stalled
                    );
                    self.log.event("error", "cycle_stalled", json!({"error": msg}));
                    rep.hard_failure = Some(msg);
                    return;
                }
            } else {
                stalled = 0;
                last_processed = vault.cycle_processed_count;
            }
            if vault.cycle_processed_count >= vault.cycle_eligible_count {
                let payer = self.payer;
                let s = self.send("finalize_heartbeat", vec![ixs::finalize(&self.keys, &payer, &vault)]);
                self.record(rep, "finalize_heartbeat", &s);
                if let Sent::CapReached(_) = s {
                    rep.hard_failure = Some("send cap reached before the cycle could be finalized".into());
                }
                return;
            }
            let claims = match self.load_claims() {
                Ok(c) => c,
                Err(e) => {
                    self.log.event("error", "cannot_load_claims", json!({"error": e}));
                    rep.hard_failure = Some(e);
                    return;
                }
            };
            // suspects are judged on every claim read, not only the ones left to settle
            let sus = self.skip_suspects(&vault, &claims);
            if !sus.is_empty() {
                let w = self.read_world().ok();
                if let Some(w) = w {
                    let a: Vec<Alert> = evaluate(&w, &self.thresholds(), &sus)
                        .into_iter()
                        .filter(|a| a.kind == "claim_skipped_repeatedly")
                        .collect();
                    self.raise(a, rep);
                }
            }
            let todo: Vec<ClaimView> = eligible_unprocessed(&vault, &claims)
                .into_iter()
                .filter(|c| !self.quarantine.contains(&c.address))
                .collect();
            if todo.is_empty() {
                let msg = format!(
                    "the cycle expects {} claims processed, {} are, and none is left that this keeper can settle (quarantined: {})",
                    vault.cycle_eligible_count,
                    vault.cycle_processed_count,
                    self.quarantine.len()
                );
                self.log.event("error", "cycle_stuck", json!({"error": msg}));
                rep.hard_failure = Some(msg);
                return;
            }
            for batch in batches(&todo) {
                // never trust the list: re-read exactly these claims right before sending
                let addrs: Vec<Pubkey> = batch.iter().map(|c| c.address).collect();
                let fresh: Vec<ClaimView> = match self.fetch_claims(&addrs) {
                    Ok(c) => eligible_unprocessed(&vault, &c),
                    Err(e) => {
                        self.log.event("warn", "refetch_failed", json!({"error": e}));
                        continue;
                    }
                };
                if fresh.is_empty() {
                    self.log.event("info", "batch_already_done", json!({"claims": addrs.len()}));
                    continue;
                }
                if !self.settle_batch(&vault, &fresh, rep) {
                    return;
                }
            }
            if self.cfg.dry_run {
                let payer = self.payer;
                let s = self.send("finalize_heartbeat", vec![ixs::finalize(&self.keys, &payer, &vault)]);
                self.record(rep, "finalize_heartbeat", &s);
                return;
            }
        }
    }

    /// After a refused settle: did another keeper settle (or fully pay and close) one of these claims in this
    /// cycle? Read fresh from the chain; a failed read counts as "no".
    fn rival_got_there(&self, vault: &VaultView, claims: &[ClaimView]) -> bool {
        let addrs: Vec<Pubkey> = claims.iter().map(|c| c.address).collect();
        let Ok(accs) = self.read(|| self.chain.accounts(&addrs)) else { return false };
        addrs.iter().zip(accs).any(|(a, acc)| match acc.and_then(|acc| decode_claim(*a, &acc, &self.keys.program_id)) {
            None => true,
            Some(c) => c.last_settled_cycle == vault.cycle_id,
        })
    }

    /// Returns false when the pass must stop (a cap was reached).
    fn settle_batch(&mut self, vault: &VaultView, claims: &[ClaimView], rep: &mut PassReport) -> bool {
        let payer = self.payer;
        let label = format!("settle_claims x{}", claims.len());
        let s = self.send(&label, vec![ixs::settle_claims_for(&self.keys, &payer, vault, claims)]);
        self.record(rep, &label, &s);
        match s {
            Sent::Done { .. } | Sent::DryRun | Sent::Race(_) | Sent::Unconfirmed(_) => true,
            Sent::CapReached(c) => {
                rep.hard_failure = Some(format!("send cap reached ({c})"));
                false
            }
            Sent::Failed { code: Some(code), .. } if is_per_claim(code) && self.rival_got_there(vault, claims) => {
                // a keeper that paid a claim in full closed its account (InvalidClaim), or processed it
                // (ClaimAlreadySettled): the rival won, nothing is wrong. Re-read in the next round.
                self.log.event("info", "lost_race", json!({"label": label, "error": describe_code(code), "note": "a claim in the batch was settled by another keeper; re-reading"}));
                true
            }
            Sent::Failed { code, .. } if claims.len() > 1 && code.map(is_per_claim).unwrap_or(false) => {
                self.log.event("warn", "batch_split", json!({"claims": claims.len(), "note": "one claim is refused: settling them one by one to isolate it"}));
                for c in claims {
                    let label = format!("settle_claims {}", c.address);
                    let s = self
                        .send(&label, vec![ixs::settle_claims_for(&self.keys, &payer, vault, std::slice::from_ref(c))]);
                    self.record(rep, &label, &s);
                    match s {
                        Sent::Failed { code: Some(code), .. } => {
                            if self.rival_got_there(vault, std::slice::from_ref(c)) {
                                self.log.event("info", "lost_race", json!({"label": label, "error": describe_code(code), "note": "settled by another keeper"}));
                            } else {
                                self.log.event("error", "claim_quarantined", json!({"claim": c.address.to_string(), "error": describe_code(code), "note": "this claim cannot be settled; the others go on"}));
                                self.quarantine.insert(c.address);
                            }
                        }
                        Sent::Failed { code: None, text, .. } | Sent::Transient(text) => {
                            rep.hard_failure = Some(format!("cannot send: {text}"));
                            return false;
                        }
                        Sent::CapReached(cap) => {
                            rep.hard_failure = Some(format!("send cap reached ({cap})"));
                            return false;
                        }
                        _ => {}
                    }
                }
                true
            }
            Sent::Failed { code: Some(code), .. } if claims.len() == 1 => {
                self.log.event("error", "claim_quarantined", json!({"claim": claims[0].address.to_string(), "error": describe_code(code), "note": "this claim cannot be settled; the others go on"}));
                self.quarantine.insert(claims[0].address);
                true
            }
            Sent::Failed { text, .. } | Sent::Transient(text) => {
                rep.hard_failure = Some(format!("cannot send settle_claims: {text}"));
                false
            }
        }
    }

    /// Dry-run, no cycle open yet: show the batches that would follow `begin_heartbeat`.
    fn dry_run_cycle(&mut self, rep: &mut PassReport) {
        let Ok(vault) = self.read_vault() else { return };
        let claims = match self.load_claims() {
            Ok(c) => c,
            Err(e) => {
                self.log.event(
                    "warn",
                    "dry_run_cannot_list_claims",
                    json!({"error": e, "note": "the settle batches cannot be shown"}),
                );
                return;
            }
        };
        let mut v = vault.clone();
        v.cycle_active = true;
        v.cycle_id += 1; // what the cycle id would be
        let todo = eligible_unprocessed(&v, &claims);
        for b in batches(&todo) {
            let payer = self.payer;
            let label = format!("settle_claims x{} (after begin)", b.len());
            let s = self.send(&label, vec![ixs::settle_claims_for(&self.keys, &payer, &v, &b)]);
            self.record(rep, &label, &s);
        }
        let payer = self.payer;
        let s = self.send("finalize_heartbeat (after the claims)", vec![ixs::finalize(&self.keys, &payer, &v)]);
        self.record(rep, "finalize_heartbeat", &s);
    }

    /// Dry-run of an open cycle is handled inside `run_cycle` (it prints each batch and the finalize).
    pub fn status_report(&mut self) -> Result<Value, String> {
        let world = self.read_world()?;
        let claims = self.load_claims().ok();
        let sus = claims.as_ref().map(|c| self.skip_suspects(&world.vault, c)).unwrap_or_default();
        let mut rep = PassReport::default();
        let a = evaluate(&world, &self.thresholds(), &sus);
        self.raise(a, &mut rep);
        let v = &world.vault;
        let sched = schedule(v, world.now);
        let spendable = world.usdc.spendable() as u128 + world.usdt.spendable() as u128;
        Ok(json!({
            "cluster": cluster_label(&self.cluster),
            "program": self.keys.program_id.to_string(),
            "vault": self.keys.vault().to_string(),
            "chain_time": world.now,
            "payer": self.payer.to_string(),
            "payer_balance_lamports": world.payer_balance,
            "cycle": {"id": v.cycle_id, "active": v.cycle_active, "started_at": v.cycle_started_at, "eligible": v.cycle_eligible_count, "processed": v.cycle_processed_count},
            "next": format!("{sched:?}"),
            "earliest_begin": crate::plan::earliest_begin(v),
            "open_claims": {"count": v.open_claims_count, "total": v.open_claims_total},
            "pools": {"usdc": {"amount": world.usdc.amount, "frozen": world.usdc.frozen}, "usdt": {"amount": world.usdt.amount, "frozen": world.usdt.frozen}},
            "coverage_ratio": if v.open_claims_total == 0 { Value::Null } else { json!(crate::alerts::ratio_string(spendable, v.open_claims_total as u128)) },
            "products": world.products.iter().map(|p| json!({"id": p.id.to_string(), "active": p.active, "pause_reason": p.pause_reason})).collect::<Vec<_>>(),
            "claims_seen": claims.as_ref().map(|c| c.len()),
            "alerts": rep.alerts.iter().map(|a| json!({"kind": a.kind, "key": a.key})).collect::<Vec<_>>(),
        }))
    }
}
