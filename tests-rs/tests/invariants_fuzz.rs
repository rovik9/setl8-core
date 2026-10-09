//! Seeded, model-based random-sequence tester for the whole vault.
//!
//! It drives the REAL program in LiteSVM with long random sequences of every
//! instruction (plus hostile variants) against an INDEPENDENT off-chain model: plain
//! Rust structs whose rules are the README's, restated here from scratch (no
//! program constant, no program state is read back to compute an expectation). After
//! EVERY step the chain is compared with the model and the global invariants below
//! are asserted. Seeds are fixed (see `SEEDS`); a failing seed reproduces
//! deterministically and the failure prints the seed, the step number and the last 10
//! actions.
//!
//! Reproduce one seed:  `FUZZ_SEED=17 [FUZZ_STEPS=400] cargo test --manifest-path
//! tests-rs/Cargo.toml --test invariants_fuzz fuzz_one -- --nocapture`
//!
//! INVARIANTS (the number is printed in every failure message):
//!  1  token conservation: per mint, the sum of ALL token accounts only changes when
//!     the test mints / airdrops / deletes tokens (the model knows when)
//!  2  vault counters equal values recomputed from the accounts that exist:
//!     open_claims_count / open_claims_total / bond_principal_open_total / every
//!     BondCapTracker.open_principal_total
//!  3  pool, SL8, wallet and ATA balances equal the model's
//!  4  no claim ever pays more than it owed; owed never grows; a closed claim never
//!     reappears
//!  5  cycle state machine: processed <= eligible, finalize only when equal, one cycle
//!     at a time, >= 432,000 s between starts, ids +1
//!  6  caps: per wallet <= 50,000 coins, global <= 600,000 coins
//!  7  a paused product never accepts deposit_fee / deposit_reset / request_payout;
//!     its queued claims still settle
//!  8  registry total_requests_emitted / total_requested_amount equal the model's
//!  9  an instruction that errors changes NO account (all non-sysvar state except the
//!     fee payer is fingerprinted before and after)
//! 10  rent: no program-owned account or token account is below rent exemption
//! 11  the sum paid out by a cycle never exceeds min(available, owed) at its snapshot
//!     and a settle never pays more than the pools held
//! plus 12 (chain state != model state), 13 (outcome / error code != the model's),
//! 14 (rent lamports went to / came from the wrong account), 15 (an account is not at
//! its canonical address or is not an account type of this program).
#![allow(clippy::too_many_arguments)]

mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anchor_lang::solana_program::{instruction::Instruction, pubkey::Pubkey};
use anchor_lang::{AccountDeserialize, Discriminator};
use anchor_spl::token::spl_token::{
    self,
    solana_program::program_pack::Pack,
    state::{Account as SplAccount, AccountState},
};
use common::*;
use core_vault::errors::VaultError;
use core_vault::state::{
    BondCapTracker, BondPosition, BondTerm, PayoutClaim, ProductRegistry, TraderState, TraderStatus, VaultState,
};
use setl8_shared_interfaces as si;
use litesvm::types::TransactionResult;
use solana_keypair::Keypair;
use solana_transaction_error::TransactionError;
use solana_signer::Signer;

// =============================================================== configuration

/// The fixed seeds of the normal run (30). A failing seed is reproducible with
/// `FUZZ_SEED=<n>`.
const SEEDS: [u64; 30] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584, 4181, 6765, 10946, 17711, 28657,
    46368, 75025, 121393, 196418, 317811, 514229, 832040, 1346269,
];
const STEPS: usize = 400;
/// The #[ignore]d long run: 20 seeds x 5,000 steps.
const LONG_SEEDS: [u64; 20] = [
    7, 77, 777, 7777, 31, 313, 3131, 65537, 424242, 9001, 1234567, 8675309, 271828, 314159, 161803, 141421, 173205, 223606,
    244948, 264575,
];
const LONG_STEPS: usize = 5_000;

const NP: usize = 16; // people: traders and bond depositors (>= 13 rich ones so the global bond cap can bind)
const NS: usize = 3; // sector programs (index 0 is registered at setup)
const NK: usize = 3; // keepers (settle callers)

// The README's numbers, restated (NOT imported from the program).
const INACTIVITY: i64 = 604_800;
const THROTTLE: i64 = 86_400;
const GAP: i64 = 432_000;
const MAX_BATCH: usize = 6;
const BOND_MIN: u64 = 50_000_000;
const BOND_WALLET_CAP: u64 = 50_000_000_000;
const BOND_GLOBAL_CAP: u64 = 600_000_000_000;
const D: i64 = 86_400;
/// request_payout and request_bond_payout refuse to take the open claims above this (SR-21):
/// $2,500,000 = 2_500_000_000_000 base units (restated here, not imported).
const CLAIMS_CEILING: u64 = 2_500_000_000_000;
/// spl-token TokenError::AccountFrozen: what a transfer into / out of a frozen pool returns.
const FROZEN_CODE: u32 = 17;

fn term_secs(t: BondTerm) -> (i64, i64, u64) {
    // (term, hard lock, interest bps)
    match t {
        BondTerm::SixMonths => (180 * D, 90 * D, 2_000),
        BondTerm::NineMonths => (270 * D, 135 * D, 3_000),
    }
}

// ===================================================================== RNG

#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    /// inclusive
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn pct(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
    fn idx(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }
    fn pick<'a, T>(&mut self, s: &'a [T]) -> &'a T {
        &s[self.idx(s.len())]
    }
}

// ================================================================= the model

fn ev(e: VaultError) -> E {
    E::C(u32::from(e))
}

/// Why the model says an intent must fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum E {
    /// a custom program error code (a VaultError)
    C(u32),
    /// system program "account already in use" (custom 0)
    Sys0,
    /// some error the model does not pin to one code
    Any,
}

#[derive(Clone, Debug, PartialEq)]
struct PCfg {
    fee_bps: u16,
    tiers: Vec<(u64, u64)>, // (size, cost)
    max_payout: u64,
    reset_bps: Vec<u16>,
}
impl PCfg {
    fn to_cfg(&self) -> Cfg {
        Cfg {
            fee_split_bps: self.fee_bps,
            tiers: self.tiers.iter().map(|&(size, cost)| ChallengeSize { size, cost }).collect(),
            max_payout: self.max_payout,
            reset_bps: self.reset_bps.clone(),
        }
    }
    fn valid(&self) -> Vec<E> {
        let mut e = vec![];
        if self.tiers.len() > 32 {
            e.push(ev(VaultError::TooManyChallengeSizes));
        }
        if self.reset_bps.len() > 8 {
            e.push(ev(VaultError::TooManyResetPhases));
        }
        if self.fee_bps > 10_000 {
            e.push(ev(VaultError::InvalidFeeSplit));
        }
        e
    }
}

#[derive(Clone, Debug)]
struct Prod {
    cfg: PCfg,
    active: bool,
    pause_reason: u8,
    paused_since: i64,
    total_paused: i64,
    emitted: u64,
    amount: u64,
}
impl Prod {
    fn paused_secs_at(&self, now: i64) -> i64 {
        let ongoing = if self.paused_since > 0 { (now - self.paused_since).max(0) } else { 0 };
        self.total_paused.saturating_add(ongoing)
    }
    fn pause(&mut self, reason: u8, now: i64) {
        self.active = false;
        self.pause_reason = reason;
        self.paused_since = now;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Active,
    Graduated,
    Failed,
    Abandoned,
}
fn st_of(s: TraderStatus) -> St {
    match s {
        TraderStatus::Active => St::Active,
        TraderStatus::Graduated => St::Graduated,
        TraderStatus::Failed => St::Failed,
        TraderStatus::Abandoned => St::Abandoned,
    }
}

#[derive(Clone, Debug)]
struct Trader {
    size: u64,
    payout_count: u64,
    status: St,
    last: i64,
    snap: i64,
    reset_used: bool,
}
impl Trader {
    fn stale(&self, now: i64, paused_now: i64) -> bool {
        let wall = now.saturating_sub(self.last);
        let paused = paused_now.saturating_sub(self.snap);
        wall.saturating_sub(paused).max(0) > INACTIVITY
    }
}

/// Logical identity of a claim (never keyed by Pubkey order: determinism).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CKey {
    T { s: usize, p: usize, cid: u64, req: u64 },
    B { p: usize, idx: u64 },
}

#[derive(Clone, Debug)]
struct MClaim {
    person: usize,
    owed: u64,
    created: u64,
    last: u64,
}

#[derive(Clone, Debug)]
struct Bond {
    principal: u64,
    term: BondTerm,
    c: usize,
    created_at: i64,
}
#[derive(Clone, Debug, Default)]
struct Tracker {
    open: u64,
    next: u64,
}

#[derive(Clone, Debug, Default)]
struct Cycle {
    id: u64,
    started: i64,
    active: bool,
    owed: u64,
    avail: u64,
    eligible: u64,
    processed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtaMode {
    Absent,
    Usable,
    Frozen,
    Reowned,
    Uninit,
    WrongMint,
    SysOwned,
    Foreign, // owned by Token-2022
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tally {
    Missing,
    DustOnly,
    Valid(u64, u64),
    BadLen,
    BadMagic,
    Foreign(u64, u64),
}

/// Immutable context shared by the model and the executor.
struct World {
    people: Vec<Pubkey>,
    sectors: Vec<Sector>,
    usdc: Pubkey,
    usdt: Pubkey,
}
impl World {
    fn mint(&self, c: usize) -> Pubkey {
        if c == 0 {
            self.usdc
        } else {
            self.usdt
        }
    }
    fn claim_addr(&self, k: CKey) -> Pubkey {
        match k {
            CKey::T { s, p, cid, req } => claim_key(&self.sectors[s], &self.people[p], cid, req),
            CKey::B { p, idx } => bond_claim_pda(&self.people[p], idx).0,
        }
    }
}

#[derive(Clone)]
struct Model {
    w: Rc<World>,
    now: i64,
    pools: [u64; 2],
    sl8: [u64; 2],
    wallet: Vec<[u64; 2]>,
    ata_bal: Vec<[u64; 2]>,
    ata_mode: Vec<[AtaMode; 2]>,
    /// supply per coin: moves only on test-side mint / delete events
    supply: [i128; 2],
    /// which payout pools the issuer has frozen (SR-03): a frozen pool counts as empty for the
    /// heartbeat, and transfers into / out of it fail in the token program
    frozen: [bool; 2],
    floors: [u64; 2],
    floor_updated_at: i64,
    withdrawn: [u64; 2],
    fees_retained: u64,
    prods: Vec<Option<Prod>>,
    traders: BTreeMap<(usize, usize, u64), Trader>,
    claims: BTreeMap<CKey, MClaim>,
    dead_claims: Vec<CKey>,
    bonds: BTreeMap<(usize, u64), Bond>,
    trackers: BTreeMap<usize, Tracker>,
    cyc: Cycle,
    tallies: Vec<Tally>,
}

/// Who pays / receives rent lamports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Actor {
    Rent,
    Sl8,
    Person(usize),
    Keeper(usize),
}
#[derive(Clone, Debug)]
enum Ev {
    /// an account was created at `addr`; `payer` paid max(rent(space) - lamports_before, 0)
    Created { addr: Pubkey, space: usize, payer: Actor },
    /// an account at `addr` was closed; all its lamports went to `to`
    Closed { addr: Pubkey, to: Actor },
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Out {
    None,
    Activity(ActivityOutcome),
    Payout(PayoutOutcome),
}

struct Stepped {
    m: Model,
    out: Out,
    evs: Vec<Ev>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtaOp {
    Create(u64),
    Delete,
    Freeze,
    Thaw,
    Reown,
    Uninit,
    WrongMint,
    SysOwned,
    Foreign,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum Act {
    Register { s: usize, cfg: PCfg },
    Update { s: usize, cfg: PCfg },
    Pause { s: usize },
    Reactivate { s: usize },
    Withdraw { c: usize, amount: u64 },
    DepositFee { p: usize, s: usize, c: usize, size: u64, cost: u64, cid: u64 },
    DepositReset { p: usize, s: usize, c: usize, prev: u64, new: u64, amount: u64, phase: u8 },
    Record { s: usize, p: usize, cid: u64 },
    Flag { s: usize, p: usize, cid: u64 },
    Abandon { s: usize, p: usize, cid: u64 },
    Payout { s: usize, p: usize, cid: u64, amount: u64, req: u64 },
    Begin,
    Settle { caller: usize, items: Vec<(Pubkey, Pubkey, Pubkey)>, junk_tail: usize },
    Finalize,
    Reconcile { s: usize, wrong_addr: bool },
    BondDeposit { p: usize, idx: u64, principal: u64, term: BondTerm, c: usize },
    BondRequest { p: usize, idx: u64 },
    // ---- test-side events (not program instructions)
    Warp { t: i64 },
    Ata { p: usize, c: usize, op: AtaOp },
    Airdrop { p: usize, c: usize, amount: u64 },
    Dust { addr: Pubkey, lamports: u64, what: &'static str },
    SetTally { s: usize, t: Tally },
    PoolFreeze { c: usize, frozen: bool },
}

impl Act {
    fn kind(&self) -> &'static str {
        match self {
            Act::Register { .. } => "register_product",
            Act::Update { .. } => "update_product_config",
            Act::Pause { .. } => "pause_product",
            Act::Reactivate { .. } => "reactivate_product",
            Act::Withdraw { .. } => "admin_withdraw_marketing_funds",
            Act::DepositFee { .. } => "deposit_fee",
            Act::DepositReset { .. } => "deposit_reset",
            Act::Record { .. } => "record_activity",
            Act::Flag { .. } => "flag_trader_failed",
            Act::Abandon { .. } => "mark_abandoned",
            Act::Payout { .. } => "request_payout",
            Act::Begin => "begin_heartbeat",
            Act::Settle { .. } => "settle_claims",
            Act::Finalize => "finalize_heartbeat",
            Act::Reconcile { .. } => "reconcile_product",
            Act::BondDeposit { .. } => "deposit_bond",
            Act::BondRequest { .. } => "request_bond_payout",
            Act::Warp { .. } => "env:warp",
            Act::Ata { .. } => "env:ata",
            Act::Airdrop { .. } => "env:airdrop",
            Act::Dust { .. } => "env:dust",
            Act::SetTally { .. } => "env:tally",
            Act::PoolFreeze { .. } => "env:freeze",
        }
    }
    fn is_program_ix(&self) -> bool {
        !self.kind().starts_with("env:")
    }
}

fn ceil_bps(x: u64, bps: u64) -> u64 {
    ((x as u128 * bps as u128 + 9_999) / 10_000) as u64
}
fn floor_bps(x: u64, bps: u64) -> u64 {
    (x as u128 * bps as u128 / 10_000) as u64
}

impl Model {
    fn prod(&self, s: usize) -> Result<&Prod, Vec<E>> {
        self.prods[s].as_ref().ok_or_else(|| vec![E::Any])
    }
    fn claims_total(&self) -> u128 {
        self.claims.values().map(|c| c.owed as u128).sum()
    }
    fn bond_open_total(&self) -> u64 {
        self.bonds.values().map(|b| b.principal).sum()
    }
    /// What pool `i` can pay out now: 0 when frozen.
    fn spend(&self, i: usize) -> u64 {
        if self.frozen[i] {
            0
        } else {
            self.pools[i]
        }
    }
    fn ata_usable(&self, p: usize) -> bool {
        self.ata_mode[p][0] == AtaMode::Usable && self.ata_mode[p][1] == AtaMode::Usable
    }

    /// One payment of `amount` from person `p`'s wallet: pool gets floor(amount*bps/10_000),
    /// SL8 the exact rest.
    fn pay_split(&mut self, p: usize, c: usize, amount: u64, bps: u64) {
        let pool = floor_bps(amount, bps);
        self.wallet[p][c] -= amount;
        self.pools[c] += pool;
        self.sl8[c] += amount - pool;
    }

    /// Pure transition: `Ok` = the instruction must succeed and this is the result;
    /// `Err(list)` = it must fail with one of the listed errors.
    fn step(&self, a: &Act) -> Result<Stepped, Vec<E>> {
        let mut m = self.clone();
        let mut out = Out::None;
        let mut evs: Vec<Ev> = vec![];
        let now = self.now;
        match a {
            Act::Register { s, cfg } => {
                let mut e = cfg.valid();
                if self.prods[*s].is_some() {
                    e.push(E::Sys0);
                }
                if !e.is_empty() {
                    return Err(e);
                }
                m.prods[*s] = Some(Prod {
                    cfg: cfg.clone(),
                    active: true,
                    pause_reason: 0,
                    paused_since: 0,
                    total_paused: 0,
                    emitted: 0,
                    amount: 0,
                });
                evs.push(Ev::Created { addr: self.w.sectors[*s].registry(), space: ProductRegistry::SPACE, payer: Actor::Sl8 });
            }
            Act::Update { s, cfg } => {
                self.prod(*s)?;
                let e = cfg.valid();
                if !e.is_empty() {
                    return Err(e);
                }
                m.prods[*s].as_mut().unwrap().cfg = cfg.clone();
            }
            Act::Pause { s } => {
                if !self.prod(*s)?.active {
                    return Err(vec![ev(VaultError::ProductAlreadyPaused)]);
                }
                m.prods[*s].as_mut().unwrap().pause(1, now);
            }
            Act::Reactivate { s } => {
                self.prod(*s)?;
                let pr = m.prods[*s].as_mut().unwrap();
                if pr.paused_since > 0 {
                    pr.total_paused = pr.total_paused.saturating_add((now - pr.paused_since).max(0));
                }
                pr.paused_since = 0;
                pr.pause_reason = 0;
                pr.active = true;
            }
            Act::Withdraw { c, amount } => {
                let live = self.pools[*c];
                let reserve = ceil_bps(live, 2_500).max(self.floors[*c]);
                let avail = live.saturating_sub(reserve);
                let mut e = vec![];
                if *amount == 0 {
                    e.push(ev(VaultError::ZeroAmount));
                }
                if *amount > avail {
                    e.push(ev(VaultError::WithdrawalExceedsReserve));
                }
                if !e.is_empty() {
                    return Err(e);
                }
                if self.frozen[*c] {
                    // the transfer out of a frozen pool fails inside the token program
                    return Err(vec![E::C(FROZEN_CODE)]);
                }
                m.pools[*c] -= amount;
                m.sl8[*c] += amount;
                m.withdrawn[*c] += amount;
            }
            Act::DepositFee { p, s, c, size, cost, cid } => {
                let pr = self.prod(*s)?;
                let mut e = vec![];
                if !pr.active {
                    e.push(ev(VaultError::ProductNotActive));
                }
                if !pr.cfg.tiers.contains(&(*size, *cost)) {
                    e.push(ev(VaultError::InvalidChallengeTier));
                }
                if self.wallet[*p][*c] < *cost {
                    e.push(ev(VaultError::InsufficientTokenBalance));
                }
                if self.traders.contains_key(&(*s, *p, *cid)) {
                    e.push(E::Sys0);
                }
                if !e.is_empty() {
                    return Err(e);
                }
                if self.frozen[*c] && floor_bps(*cost, pr.cfg.fee_bps as u64) > 0 {
                    // the pool leg fails inside the token program (a zero leg is skipped)
                    return Err(vec![E::C(FROZEN_CODE)]);
                }
                let paused_now = pr.paused_secs_at(now);
                let bps = pr.cfg.fee_bps as u64;
                m.pay_split(*p, *c, *cost, bps);
                m.traders.insert(
                    (*s, *p, *cid),
                    Trader { size: *size, payout_count: 0, status: St::Active, last: now, snap: paused_now, reset_used: false },
                );
                evs.push(Ev::Created {
                    addr: self.w.sectors[*s].trader(&self.w.people[*p], *cid),
                    space: TraderState::SPACE,
                    payer: Actor::Rent,
                });
            }
            Act::DepositReset { p, s, c, prev, new, amount, phase } => {
                let pr = self.prod(*s)?;
                let mut e = vec![];
                if !pr.active {
                    e.push(ev(VaultError::ProductNotActive));
                }
                let Some(pt) = self.traders.get(&(*s, *p, *prev)) else {
                    e.push(E::Any);
                    return Err(e);
                };
                if pt.status != St::Failed || pt.reset_used {
                    e.push(ev(VaultError::ResetNotAllowed));
                }
                match pr.cfg.reset_bps.get(*phase as usize) {
                    None => e.push(ev(VaultError::InvalidResetPhase)),
                    Some(&bps) => {
                        if *amount != floor_bps(pt.size, bps as u64) {
                            e.push(ev(VaultError::WrongAmount));
                        }
                    }
                }
                if self.wallet[*p][*c] < *amount {
                    e.push(ev(VaultError::InsufficientTokenBalance));
                }
                if self.traders.contains_key(&(*s, *p, *new)) {
                    e.push(E::Sys0);
                }
                if !e.is_empty() {
                    return Err(e);
                }
                if self.frozen[*c] && floor_bps(*amount, pr.cfg.fee_bps as u64) > 0 {
                    return Err(vec![E::C(FROZEN_CODE)]);
                }
                let paused_now = pr.paused_secs_at(now);
                let bps = pr.cfg.fee_bps as u64;
                let (size, count) = (pt.size, pt.payout_count);
                m.pay_split(*p, *c, *amount, bps);
                m.traders.get_mut(&(*s, *p, *prev)).unwrap().reset_used = true;
                m.traders.insert(
                    (*s, *p, *new),
                    Trader { size, payout_count: count, status: St::Active, last: now, snap: paused_now, reset_used: false },
                );
                evs.push(Ev::Created {
                    addr: self.w.sectors[*s].trader(&self.w.people[*p], *new),
                    space: TraderState::SPACE,
                    payer: Actor::Rent,
                });
            }
            Act::Record { s, p, cid } => {
                let pr = self.prod(*s)?;
                let Some(t) = self.traders.get(&(*s, *p, *cid)) else { return Err(vec![E::Any]) };
                if t.status != St::Active {
                    return Err(vec![ev(VaultError::InvalidTraderStatus)]);
                }
                let paused_now = pr.paused_secs_at(now);
                let tm = m.traders.get_mut(&(*s, *p, *cid)).unwrap();
                if t.stale(now, paused_now) {
                    tm.status = St::Abandoned;
                    out = Out::Activity(ActivityOutcome::Abandoned);
                } else if now.saturating_sub(t.last) < THROTTLE {
                    out = Out::Activity(ActivityOutcome::Throttled);
                } else {
                    tm.last = now;
                    tm.snap = paused_now;
                    out = Out::Activity(ActivityOutcome::Recorded);
                }
            }
            Act::Flag { s, p, cid } => {
                self.prod(*s)?;
                let Some(t) = self.traders.get(&(*s, *p, *cid)) else { return Err(vec![E::Any]) };
                if t.status != St::Active {
                    return Err(vec![ev(VaultError::InvalidTraderStatus)]);
                }
                m.traders.get_mut(&(*s, *p, *cid)).unwrap().status = St::Failed;
            }
            Act::Abandon { s, p, cid } => {
                let pr = self.prod(*s)?;
                let Some(t) = self.traders.get(&(*s, *p, *cid)) else { return Err(vec![E::Any]) };
                if t.status != St::Active {
                    return Err(vec![ev(VaultError::InvalidTraderStatus)]);
                }
                if !t.stale(now, pr.paused_secs_at(now)) {
                    return Err(vec![ev(VaultError::NotAbandonable)]);
                }
                m.traders.get_mut(&(*s, *p, *cid)).unwrap().status = St::Abandoned;
            }
            Act::Payout { s, p, cid, amount, req } => {
                let pr = self.prod(*s)?;
                let Some(t) = self.traders.get(&(*s, *p, *cid)) else { return Err(vec![E::Any]) };
                // program order matters: the stale path returns Ok before the cap / id checks
                if !pr.active {
                    return Err(vec![ev(VaultError::ProductNotActive)]);
                }
                if *amount == 0 {
                    return Err(vec![ev(VaultError::ZeroAmount)]);
                }
                if t.status != St::Active {
                    return Err(vec![ev(VaultError::InvalidTraderStatus)]);
                }
                let paused_now = pr.paused_secs_at(now);
                if t.stale(now, paused_now) {
                    m.traders.get_mut(&(*s, *p, *cid)).unwrap().status = St::Abandoned;
                    out = Out::Payout(PayoutOutcome::Abandoned);
                } else {
                    if t.payout_count >= pr.cfg.max_payout {
                        return Err(vec![ev(VaultError::PayoutCapReached)]);
                    }
                    if *req != t.payout_count + 1 {
                        return Err(vec![ev(VaultError::RequestIdMismatch)]);
                    }
                    // the ceiling guard comes before the checked additions
                    if *amount as u128 > (CLAIMS_CEILING as u128).saturating_sub(self.claims_total()) {
                        return Err(vec![ev(VaultError::ClaimsCeilingExceeded)]);
                    }
                    if pr.amount as u128 + *amount as u128 > u64::MAX as u128 {
                        return Err(vec![ev(VaultError::MathOverflow)]);
                    }
                    let tm = m.traders.get_mut(&(*s, *p, *cid)).unwrap();
                    tm.payout_count = *req;
                    tm.last = now;
                    tm.snap = paused_now;
                    if tm.payout_count >= pr.cfg.max_payout {
                        tm.status = St::Graduated;
                    }
                    let pm = m.prods[*s].as_mut().unwrap();
                    pm.emitted += 1;
                    pm.amount += *amount;
                    m.claims.insert(
                        CKey::T { s: *s, p: *p, cid: *cid, req: *req },
                        MClaim { person: *p, owed: *amount, created: self.cyc.id, last: 0 },
                    );
                    out = Out::Payout(PayoutOutcome::Paid);
                    evs.push(Ev::Created {
                        addr: self.w.claim_addr(CKey::T { s: *s, p: *p, cid: *cid, req: *req }),
                        space: PayoutClaim::SPACE,
                        payer: Actor::Rent,
                    });
                }
            }
            Act::Begin => {
                let mut e = vec![];
                if self.cyc.active {
                    e.push(ev(VaultError::CycleInProgress));
                }
                if self.cyc.started != 0 && now < self.cyc.started + GAP {
                    e.push(ev(VaultError::HeartbeatTooEarly));
                }
                if !e.is_empty() {
                    return Err(e);
                }
                m.cyc = Cycle {
                    id: self.cyc.id + 1,
                    started: now,
                    active: true,
                    owed: self.claims_total() as u64,
                    avail: self.spend(0) + self.spend(1),
                    eligible: self.claims.len() as u64,
                    processed: 0,
                };
            }
            Act::Settle { caller, items, junk_tail } => {
                if !self.cyc.active {
                    return Err(vec![ev(VaultError::NoCycleInProgress)]);
                }
                let n_acc = items.len() * 3 + junk_tail;
                if n_acc == 0 {
                    return Err(vec![ev(VaultError::EmptyBatch)]);
                }
                if n_acc % 3 != 0 {
                    return Err(vec![ev(VaultError::InvalidClaim)]);
                }
                if items.len() > MAX_BATCH {
                    return Err(vec![ev(VaultError::BatchTooLarge)]);
                }
                let (num, den) = (self.cyc.avail.min(self.cyc.owed), self.cyc.owed);
                let by_addr: Vec<(CKey, Pubkey)> = self.claims.keys().map(|k| (*k, self.w.claim_addr(*k))).collect();
                for (claim_addr, usdc_ata, usdt_ata) in items {
                    let Some((ck, _)) = by_addr.iter().find(|(_, a)| a == claim_addr) else {
                        return Err(vec![ev(VaultError::InvalidClaim)]);
                    };
                    let Some(c) = m.claims.get(ck).cloned() else {
                        // closed earlier in this batch
                        return Err(vec![ev(VaultError::InvalidClaim)]);
                    };
                    if c.created >= self.cyc.id {
                        return Err(vec![ev(VaultError::ClaimNotEligible)]);
                    }
                    if c.last == self.cyc.id {
                        return Err(vec![ev(VaultError::ClaimAlreadySettled)]);
                    }
                    let w = &self.w.people[c.person];
                    if *usdc_ata != ata(w, &self.w.usdc) || *usdt_ata != ata(w, &self.w.usdt) {
                        return Err(vec![ev(VaultError::InvalidTokenAccount)]);
                    }
                    if den == 0 {
                        return Err(vec![E::Any]);
                    }
                    let p = c.person;
                    if !m.ata_usable(p) {
                        m.claims.get_mut(ck).unwrap().last = self.cyc.id;
                        m.cyc.processed += 1;
                        continue;
                    }
                    let target = c.owed as u128 * num as u128 / den as u128;
                    let live = m.spend(0) as u128 + m.spend(1) as u128;
                    let pay = target.min(live) as u64;
                    let usdc_first = m.spend(0) >= m.spend(1);
                    let first_bal = if usdc_first { m.spend(0) } else { m.spend(1) };
                    let from_first = pay.min(first_bal);
                    let from_second = pay - from_first;
                    let (u, t) = if usdc_first { (from_first, from_second) } else { (from_second, from_first) };
                    m.pools[0] -= u;
                    m.pools[1] -= t;
                    m.ata_bal[p][0] += u;
                    m.ata_bal[p][1] += t;
                    let cm = m.claims.get_mut(ck).unwrap();
                    cm.owed -= pay;
                    cm.last = self.cyc.id;
                    m.cyc.processed += 1;
                    if cm.owed == 0 {
                        m.claims.remove(ck);
                        m.dead_claims.push(*ck);
                        evs.push(Ev::Closed { addr: *claim_addr, to: Actor::Keeper(*caller) });
                    }
                }
            }
            Act::Finalize => {
                if !self.cyc.active {
                    return Err(vec![ev(VaultError::NoCycleInProgress)]);
                }
                if self.cyc.processed != self.cyc.eligible {
                    return Err(vec![ev(VaultError::CycleIncomplete)]);
                }
                m.floors = [floor_bps(self.pools[0], 2_500), floor_bps(self.pools[1], 2_500)];
                m.floor_updated_at = now;
                m.cyc.active = false;
            }
            Act::Reconcile { s, wrong_addr } => {
                let pr = self.prod(*s)?;
                let mut e = vec![];
                if !pr.active {
                    e.push(ev(VaultError::ProductAlreadyPaused));
                }
                if *wrong_addr {
                    e.push(ev(VaultError::InvalidTally));
                }
                if !e.is_empty() {
                    return Err(e);
                }
                let ok = match self.tallies[*s] {
                    Tally::Missing | Tally::DustOnly => pr.emitted == 0 && pr.amount == 0,
                    Tally::Valid(c, t) => c == pr.emitted && t == pr.amount,
                    Tally::BadLen | Tally::BadMagic | Tally::Foreign(..) => false,
                };
                if !ok {
                    m.prods[*s].as_mut().unwrap().pause(2, now);
                }
            }
            Act::BondDeposit { p, idx, principal, term, c } => {
                let tr = self.trackers.get(p).cloned().unwrap_or_default();
                let fee = ceil_bps(*principal, 20);
                let mut e = vec![];
                if *principal < BOND_MIN {
                    e.push(ev(VaultError::BondBelowMinimum));
                }
                if *idx != tr.next {
                    e.push(ev(VaultError::BondIndexMismatch));
                }
                if tr.open as u128 + *principal as u128 > BOND_WALLET_CAP as u128 {
                    e.push(ev(VaultError::BondWalletCapExceeded));
                }
                if self.bond_open_total() as u128 + *principal as u128 > BOND_GLOBAL_CAP as u128 {
                    e.push(ev(VaultError::BondGlobalCapExceeded));
                }
                if (self.wallet[*p][*c] as u128) < *principal as u128 + fee as u128 {
                    e.push(ev(VaultError::InsufficientTokenBalance));
                }
                if !e.is_empty() {
                    return Err(e);
                }
                if self.frozen[*c] {
                    return Err(vec![E::C(FROZEN_CODE)]); // the pool leg (half the principal) fails
                }
                let pool = floor_bps(*principal, 5_000);
                m.wallet[*p][*c] -= *principal + fee;
                m.pools[*c] += pool;
                m.sl8[*c] += (*principal - pool) + fee;
                let tm = m.trackers.entry(*p).or_default();
                tm.open += *principal;
                tm.next = *idx + 1;
                m.bonds.insert((*p, *idx), Bond { principal: *principal, term: *term, c: *c, created_at: now });
                if tr.next == 0 && !self.trackers.contains_key(p) {
                    evs.push(Ev::Created { addr: bond_cap_pda(&self.w.people[*p]).0, space: BondCapTracker::SPACE, payer: Actor::Person(*p) });
                }
                evs.push(Ev::Created { addr: bond_pda(&self.w.people[*p], *idx).0, space: BondPosition::SPACE, payer: Actor::Person(*p) });
            }
            Act::BondRequest { p, idx } => {
                let Some(b) = self.bonds.get(&(*p, *idx)) else { return Err(vec![ev(VaultError::InvalidBondPosition)]) };
                let (term, lock, bps) = term_secs(b.term);
                let age = now - b.created_at;
                if age < lock {
                    return Err(vec![ev(VaultError::BondLocked)]);
                }
                let gross = if age >= term { b.principal + floor_bps(b.principal, bps) } else { b.principal };
                let fee = ceil_bps(gross, 20);
                let net = gross - fee;
                // the claims ceiling applies to bond exits too (refused cleanly, retryable)
                if net as u128 > (CLAIMS_CEILING as u128).saturating_sub(self.claims_total()) {
                    return Err(vec![ev(VaultError::ClaimsCeilingExceeded)]);
                }
                let principal = b.principal;
                m.trackers.get_mut(p).unwrap().open -= principal;
                m.bonds.remove(&(*p, *idx));
                m.fees_retained += fee;
                m.claims.insert(CKey::B { p: *p, idx: *idx }, MClaim { person: *p, owed: net, created: self.cyc.id, last: 0 });
                evs.push(Ev::Created { addr: bond_claim_pda(&self.w.people[*p], *idx).0, space: PayoutClaim::SPACE, payer: Actor::Person(*p) });
                evs.push(Ev::Closed { addr: bond_pda(&self.w.people[*p], *idx).0, to: Actor::Person(*p) });
            }
            // ---- test-side events
            Act::Warp { t } => m.now = *t,
            Act::Ata { p, c, op } => {
                let old = m.ata_bal[*p][*c];
                let (mode, bal) = match op {
                    AtaOp::Create(amt) => (AtaMode::Usable, *amt),
                    AtaOp::Delete => (AtaMode::Absent, 0),
                    AtaOp::Freeze => (if m.ata_mode[*p][*c] == AtaMode::Usable { AtaMode::Frozen } else { m.ata_mode[*p][*c] }, old),
                    AtaOp::Thaw => (if m.ata_mode[*p][*c] == AtaMode::Frozen { AtaMode::Usable } else { m.ata_mode[*p][*c] }, old),
                    AtaOp::Reown => (if m.ata_mode[*p][*c] == AtaMode::Usable { AtaMode::Reowned } else { m.ata_mode[*p][*c] }, old),
                    AtaOp::Uninit => (AtaMode::Uninit, 0),
                    AtaOp::WrongMint => (AtaMode::WrongMint, 0),
                    AtaOp::SysOwned => (AtaMode::SysOwned, 0),
                    AtaOp::Foreign => (AtaMode::Foreign, 0),
                };
                m.supply[*c] += bal as i128 - old as i128;
                m.ata_mode[*p][*c] = mode;
                m.ata_bal[*p][*c] = bal;
            }
            Act::Airdrop { p, c, amount } => {
                m.wallet[*p][*c] += amount;
                m.supply[*c] += *amount as i128;
            }
            Act::Dust { .. } => {}
            Act::SetTally { s, t } => m.tallies[*s] = *t,
            Act::PoolFreeze { c, frozen } => m.frozen[*c] = *frozen,
        }
        Ok(Stepped { m, out, evs })
    }
}

// ================================================================= the fuzzer

#[derive(Default, Clone)]
struct Stats {
    ok: BTreeMap<&'static str, u64>,
    err: BTreeMap<&'static str, u64>,
    /// "kind:custom-error-code" of every rejection the model predicted
    codes: BTreeMap<String, u64>,
    /// interesting states / branches reached (coverage of the generator)
    tags: BTreeMap<String, u64>,
    hostile_rejected: u64,
    composite_rejected: u64,
}
impl Stats {
    fn tag(&mut self, t: impl Into<String>) {
        *self.tags.entry(t.into()).or_default() += 1;
    }
    fn merge(&mut self, o: &Stats) {
        for (k, v) in &o.codes {
            *self.codes.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.tags {
            *self.tags.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.ok {
            *self.ok.entry(k).or_default() += v;
        }
        for (k, v) in &o.err {
            *self.err.entry(k).or_default() += v;
        }
        self.hostile_rejected += o.hostile_rejected;
        self.composite_rejected += o.composite_rejected;
    }
}

#[derive(Clone, Copy)]
enum Mutn {
    Unsign(usize),
    Swap(usize, Pubkey),
    Trunc,
    BadDisc,
    DropLast,
}

struct Fuzz {
    seed: u64,
    step: usize,
    rng: Rng,
    env: Env,
    w: Rc<World>,
    m: Model,
    people: Vec<Keypair>,
    keepers: Vec<Keypair>,
    rent_payer: Keypair,
    log: VecDeque<String>,
    stats: Stats,
    // observers (derived from the CHAIN, independent of the model)
    prev_claims: BTreeMap<Pubkey, u64>,
    closed_seen: BTreeSet<Pubkey>,
    cycle_starts: BTreeMap<u64, i64>,
    last_cycle_id: u64,
    cycle_paid: u128,
    base_lam: BTreeMap<Pubkey, u64>,
    exp_lam: BTreeMap<Pubkey, i128>,
    bond_heavy: bool,
    /// acts to run next (a dust transfer is queued in front of the creation it targets)
    queue: VecDeque<Act>,
}

fn det_keypair(seed: u64, i: u64, tag: u8) -> Keypair {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&seed.to_le_bytes());
    b[8..16].copy_from_slice(&i.to_le_bytes());
    b[16] = tag;
    b[17] = 0x5E;
    solana_keypair::keypair_from_seed(&b).unwrap()
}
fn det_pubkey(seed: u64, i: u64) -> Pubkey {
    let k = det_keypair(seed, i, 0xA0);
    k.pubkey()
}

fn rand_cfg(rng: &mut Rng) -> PCfg {
    const POOL: [(u64, u64); 9] = [
        (10_000, 100),
        (50_000, 400),
        (12_345, 77),
        (100_000_000, 99_000_000),
        (1_000_000_000, 750_000_000),
        (7_777_777, 3_333_333),
        (500_000, 0),
        (250_000_000, 1_999_999),
        (3_000, 1),
    ];
    let n = rng.range(1, 4) as usize;
    let mut tiers = vec![];
    for _ in 0..n {
        let t = *rng.pick(&POOL);
        if !tiers.contains(&t) {
            tiers.push(t);
        }
    }
    let n_reset = rng.range(0, 3) as usize;
    let reset_bps = (0..n_reset).map(|_| *rng.pick(&[0u16, 100, 150, 450, 1_000, 10_000])).collect();
    PCfg { fee_bps: *rng.pick(&[0u16, 1, 3_333, 4_000, 6_500, 6_500, 9_999, 10_000]), tiers, max_payout: rng.range(1, 4), reset_bps }
}

impl Fuzz {
    fn new(seed: u64, bond_heavy: bool) -> Self {
        let mut env = Env::new_bare();
        let mut rng = Rng::new(seed);
        // lamport-dust the vault PDA and both pool PDAs before they are created
        let min0 = env.svm.minimum_balance_for_rent_exemption(0);
        for a in [env.vault, env.usdc_pool, env.usdt_pool] {
            if rng.pct(50) {
                let l = min0 + rng.below(3_000_000);
                env.svm.airdrop(&a, l).expect("dust");
                env.svm.expire_blockhash();
            }
        }
        env.init_vault();
        let people: Vec<Keypair> = (0..NP as u64).map(|i| det_keypair(seed, i, 1)).collect();
        let keepers: Vec<Keypair> = (0..NK as u64).map(|i| det_keypair(seed, i, 2)).collect();
        let rent_payer = det_keypair(seed, 0, 3);
        for k in people.iter().chain(keepers.iter()).chain([&rent_payer]) {
            env.fund(&k.pubkey());
        }
        // persons 0..3 are poor (so InsufficientTokenBalance is reachable), the rest are rich
        let start = |p: usize| -> u64 {
            match p {
                0 => 150_000_000,
                1 => 2_000_000_000,
                2 => 60_000_000_000,
                _ => WALLET_START,
            }
        };
        for (p, k) in people.iter().enumerate() {
            env.fund_wallet_with(&k.pubkey(), start(p), start(p));
        }
        let sectors: Vec<Sector> = (0..NS as u64)
            .map(|i| {
                let id = det_pubkey(seed, 100 + i);
                Sector { id, authority: si::derive_sector_authority(&id).0 }
            })
            .collect();
        let w = Rc::new(World { people: people.iter().map(|k| k.pubkey()).collect(), sectors, usdc: env.usdc, usdt: env.usdt });
        let m = Model {
            w: w.clone(),
            now: T0,
            pools: [0, 0],
            sl8: [0, 0],
            wallet: (0..NP).map(|p| [start(p), start(p)]).collect(),
            ata_bal: vec![[0, 0]; NP],
            ata_mode: vec![[AtaMode::Absent, AtaMode::Absent]; NP],
            supply: [(0..NP).map(|p| start(p) as i128).sum::<i128>(); 2],
            frozen: [false; 2],
            floors: [0, 0],
            floor_updated_at: 0,
            withdrawn: [0, 0],
            fees_retained: 0,
            prods: vec![None; NS],
            traders: BTreeMap::new(),
            claims: BTreeMap::new(),
            dead_claims: vec![],
            bonds: BTreeMap::new(),
            trackers: BTreeMap::new(),
            cyc: Cycle::default(),
            tallies: vec![Tally::Missing; NS],
        };
        let mut f = Fuzz {
            seed,
            step: 0,
            rng,
            env,
            w,
            m,
            people,
            keepers,
            rent_payer,
            log: VecDeque::new(),
            stats: Stats::default(),
            prev_claims: BTreeMap::new(),
            closed_seen: BTreeSet::new(),
            cycle_starts: BTreeMap::new(),
            last_cycle_id: 0,
            cycle_paid: 0,
            base_lam: BTreeMap::new(),
            exp_lam: BTreeMap::new(),
            bond_heavy,
            queue: VecDeque::new(),
        };
        let mut actors: Vec<Pubkey> = f.actor_keys();
        // the vault, both pools and SL8's token accounts must never gain or lose lamports
        actors.extend([f.env.vault, f.env.usdc_pool, f.env.usdt_pool, f.env.sl8_usdc, f.env.sl8_usdt]);
        for a in actors {
            let l = f.env.lamports(&a);
            f.base_lam.insert(a, l);
            f.exp_lam.insert(a, 0);
        }
        // sector 0 is registered up front (through the normal, checked path)
        let cfg = PCfg {
            fee_bps: 6_500,
            tiers: vec![(10_000, 100), (50_000, 400), (12_345, 77), (100_000_000, 99_000_000)],
            max_payout: 3,
            reset_bps: vec![100, 150, 450],
        };
        f.exec(Act::Register { s: 0, cfg });
        for p in 0..NP {
            if f.rng.pct(75) {
                for c in 0..2 {
                    f.exec_env(Act::Ata { p, c, op: AtaOp::Create(0) });
                }
            }
        }
        f
    }

    fn actor_keys(&self) -> Vec<Pubkey> {
        let mut v: Vec<Pubkey> = self.people.iter().map(|k| k.pubkey()).collect();
        v.extend(self.keepers.iter().map(|k| k.pubkey()));
        v.push(self.rent_payer.pubkey());
        v.push(self.env.sl8.pubkey());
        v
    }
    fn actor_key(&self, a: Actor) -> Pubkey {
        match a {
            Actor::Rent => self.rent_payer.pubkey(),
            Actor::Sl8 => self.env.sl8.pubkey(),
            Actor::Person(p) => self.people[p].pubkey(),
            Actor::Keeper(k) => self.keepers[k].pubkey(),
        }
    }

    // ------------------------------------------------------------ failure
    fn fail(&self, inv: &str, msg: String) -> ! {
        let last: Vec<String> = self.log.iter().cloned().collect();
        panic!(
            "\n=== FUZZ FAILURE === seed={} step={} INVARIANT {}\n{}\nlast {} actions (oldest first):\n  {}\nreproduce: FUZZ_SEED={} cargo test --manifest-path tests-rs/Cargo.toml --test invariants_fuzz fuzz_one -- --nocapture\n",
            self.seed,
            self.step,
            inv,
            msg,
            last.len(),
            last.join("\n  "),
            self.seed
        );
    }

    // ---------------------------------------------------------- generation
    fn pick_sector(&mut self, prefer_registered: bool) -> usize {
        let reg: Vec<usize> = (0..NS).filter(|&s| self.m.prods[s].is_some()).collect();
        if prefer_registered && !reg.is_empty() && self.rng.pct(92) {
            *self.rng.pick(&reg)
        } else {
            self.rng.idx(NS)
        }
    }
    fn pick_coin(&mut self) -> usize {
        self.rng.idx(2)
    }
    fn traders_where(&self, f: impl Fn(&Prod, &Trader) -> bool) -> Vec<(usize, usize, u64)> {
        self.m
            .traders
            .iter()
            .filter(|((s, _, _), t)| self.m.prods[*s].as_ref().map_or(false, |p| f(p, t)))
            .map(|(k, _)| *k)
            .collect()
    }
    /// A target trader: usually one that matches `f`, sometimes any, sometimes a stranger.
    fn pick_trader(&mut self, f: impl Fn(&Prod, &Trader) -> bool) -> (usize, usize, u64) {
        let good = self.traders_where(f);
        if !good.is_empty() && self.rng.pct(80) {
            return *self.rng.pick(&good);
        }
        let any: Vec<(usize, usize, u64)> = self.m.traders.keys().cloned().collect();
        if !any.is_empty() && self.rng.pct(70) {
            return *self.rng.pick(&any);
        }
        (self.pick_sector(true), self.rng.idx(NP), self.rng.range(1, 6))
    }
    fn payout_amount(&mut self) -> u64 {
        let pt = self.m.pools[0] + self.m.pools[1];
        if self.step > 30 && self.rng.pct(3) {
            // a sector bug or hostile sector: huge amounts, and amounts right at the claims ceiling
            let room = (CLAIMS_CEILING as u128).saturating_sub(self.m.claims_total()) as u64;
            return match self.rng.below(4) {
                0 => u64::MAX - self.rng.below(50),
                1 => {
                    // fill the headroom, then immediately try to exit an unlocked bond
                    let now = self.m.now;
                    let unlocked: Vec<(usize, u64)> = self
                        .m
                        .bonds
                        .iter()
                        .filter(|(_, b)| now - b.created_at >= term_secs(b.term).1)
                        .map(|(k, _)| *k)
                        .collect();
                    if !unlocked.is_empty() {
                        let (p, idx) = unlocked[self.rng.idx(unlocked.len())];
                        self.queue.push_back(Act::BondRequest { p, idx });
                    }
                    room
                }
                2 => room.saturating_add(1),
                _ => CLAIMS_CEILING / 2,
            };
        }
        match self.rng.below(12) {
            0 => 0,
            1 => 1,
            2 => 7,
            3 => self.rng.range(1, pt / 2 + 2),
            4 => pt,
            5 => pt.saturating_mul(2).saturating_add(1),
            6 => pt / 3 + 1,
            7 => self.rng.range(1_000_000, 1_000_000_000),
            8 => 123_456_789,
            9 => self.rng.range(1, 5_000),
            10 => pt / 10 + 1,
            _ => self.rng.range(1, pt.max(2)),
        }
    }

    fn gen_settle(&mut self) -> Act {
        let m = &self.m;
        // the keeper's honest batch: eligible, not yet processed this cycle, correct ATAs
        if m.cyc.active && self.rng.pct(78) {
            let mut elig: Vec<CKey> = m
                .claims
                .iter()
                .filter(|(_, c)| c.created < m.cyc.id && c.last != m.cyc.id)
                .map(|(k, _)| *k)
                .collect();
            if !elig.is_empty() {
                let n = (self.rng.range(1, 6) as usize).min(elig.len());
                let mut items = vec![];
                for _ in 0..n {
                    let k = elig.remove(self.rng.idx(elig.len()));
                    let p = match k {
                        CKey::T { p, .. } | CKey::B { p, .. } => p,
                    };
                    let w = self.w.people[p];
                    items.push((self.w.claim_addr(k), ata(&w, &self.w.usdc), ata(&w, &self.w.usdt)));
                }
                return Act::Settle { caller: self.rng.idx(NK), items, junk_tail: 0 };
            }
        }
        let live: Vec<CKey> = m.claims.keys().cloned().collect();
        let k = match self.rng.below(100) {
            0..=2 => 0,
            3..=4 => 7,
            5..=29 => 1,
            30..=49 => 2,
            50..=69 => 3,
            _ => self.rng.range(4, 6) as usize,
        };
        let mut items = vec![];
        for _ in 0..k {
            let wallet_of = |p: usize| self.w.people[p];
            // choose the claim
            let roll = self.rng.below(100);
            let (claim, owner): (Pubkey, usize) = if roll < 6 && !m.dead_claims.is_empty() {
                let d = *self.rng.pick(&m.dead_claims);
                let p = match d {
                    CKey::T { p, .. } | CKey::B { p, .. } => p,
                };
                (self.w.claim_addr(d), p)
            } else if roll < 10 {
                // not a claim at all: a trader state, the vault or a random key
                let p = self.rng.idx(NP);
                let junk = match self.rng.below(3) {
                    0 => self.w.sectors[self.rng.idx(NS)].trader(&wallet_of(p), self.rng.range(1, 6)),
                    1 => self.env.vault,
                    _ => Pubkey::new_unique(),
                };
                (junk, p)
            } else if !live.is_empty() && roll < 20 && !items.is_empty() {
                // duplicate of an item already in this batch
                let (c, _, _): (Pubkey, Pubkey, Pubkey) = items[self.rng.idx(items.len())];
                let p = live
                    .iter()
                    .find(|k| self.w.claim_addr(**k) == c)
                    .map(|k| match k {
                        CKey::T { p, .. } | CKey::B { p, .. } => *p,
                    })
                    .unwrap_or(0);
                (c, p)
            } else if !live.is_empty() {
                let k = *self.rng.pick(&live);
                let p = match k {
                    CKey::T { p, .. } | CKey::B { p, .. } => p,
                };
                (self.w.claim_addr(k), p)
            } else {
                let p = self.rng.idx(NP);
                (Pubkey::new_unique(), p)
            };
            let w = wallet_of(owner);
            let mut usdc = ata(&w, &self.w.usdc);
            let mut usdt = ata(&w, &self.w.usdt);
            if self.rng.pct(10) {
                let other = wallet_of((owner + 1 + self.rng.idx(NP - 1)) % NP);
                match self.rng.below(6) {
                    0 => std::mem::swap(&mut usdc, &mut usdt),
                    1 => usdc = ata(&other, &self.w.usdc),
                    2 => usdt = ata(&other, &self.w.usdt),
                    3 => usdc = self.env.wallet_ta(&w, Coin::Usdc),
                    4 => usdt = Pubkey::new_unique(),
                    _ => {
                        usdc = ata(&w, &self.w.usdt);
                        usdt = ata(&w, &self.w.usdc);
                    }
                }
            }
            items.push((claim, usdc, usdt));
        }
        let junk_tail = if self.rng.pct(4) { self.rng.range(1, 2) as usize } else { 0 };
        Act::Settle { caller: self.rng.idx(NK), items, junk_tail }
    }

    /// The PDA a creating action is about to create (for dust-before-creation).
    fn creation_addr(&mut self, a: &Act) -> Option<(Pubkey, &'static str)> {
        let w = self.w.clone();
        match a {
            Act::DepositFee { s, p, cid, .. } => Some((w.sectors[*s].trader(&w.people[*p], *cid), "trader_state")),
            Act::DepositReset { s, p, new, .. } => Some((w.sectors[*s].trader(&w.people[*p], *new), "trader_state")),
            Act::Payout { s, p, cid, req, .. } => {
                let ts = w.sectors[*s].trader(&w.people[*p], *cid);
                Some((claim_pda(&ts, *req).0, "payout_claim"))
            }
            Act::BondDeposit { p, idx, .. } => {
                if self.rng.pct(50) {
                    Some((bond_cap_pda(&w.people[*p]).0, "bond_cap"))
                } else {
                    Some((bond_pda(&w.people[*p], *idx).0, "bond_position"))
                }
            }
            Act::BondRequest { p, idx } => Some((bond_claim_pda(&w.people[*p], *idx).0, "bond_claim")),
            Act::Register { s, .. } => Some((w.sectors[*s].registry(), "registry")),
            _ => None,
        }
    }

    fn gen(&mut self) -> Act {
        if let Some(a) = self.queue.pop_front() {
            return a;
        }
        let a = self.gen_inner();
        if self.rng.pct(9) {
            if let Some((addr, what)) = self.creation_addr(&a) {
                let min0 = self.env.svm.minimum_balance_for_rent_exemption(0);
                let lamports = if self.rng.pct(40) { min0 + self.rng.range(0, 40_000_000) } else { min0 + self.rng.below(2_000_000) };
                self.queue.push_back(a);
                return Act::Dust { addr, lamports, what };
            }
        }
        a
    }

    fn gen_inner(&mut self) -> Act {
        // (weight, kind)
        let unreg = (0..NS).any(|s| self.m.prods[s].is_none());
        let bh = if self.bond_heavy { 4 } else { 1 };
        // state-aware weights: keep the heartbeat flow moving so settlement is exercised hard
        let cyc = &self.m.cyc;
        let gap_over = cyc.started == 0 || self.m.now >= cyc.started + GAP;
        let have_claims = !self.m.claims.is_empty();
        let begin_w = if cyc.active { 1 } else if gap_over { if have_claims { 16 } else { 4 } } else { 1 };
        let settle_w = if cyc.active { 28 } else { 1 };
        let finalize_w = if cyc.active && cyc.processed == cyc.eligible { 14 } else { 1 };
        let warp_w = if !cyc.active && !gap_over && have_claims { 22 } else { 8 };
        let table: [(u64, u8); 23] = [
            (14, 0),                       // deposit_fee
            (12, 1),                       // request_payout
            (5, 2),                        // record_activity
            (5, 3),                        // flag
            (6, 4),                        // reset
            (3, 5),                        // mark_abandoned
            (begin_w, 6),                  // begin
            (settle_w, 7),                 // settle
            (finalize_w, 8),               // finalize
            (4, 9),                        // reconcile
            (7 * bh, 10),                  // deposit_bond
            (5 * bh, 11),                  // request_bond_payout
            (2, 12),                       // withdraw
            (warp_w, 13),                  // warp
            (5, 14),                       // ata op
            (1, 15),                       // airdrop
            (3, 16),                       // dust
            (3, 17),                       // tally
            (if unreg { 6 } else { 1 }, 18), // register
            (2, 19),                       // update
            (2, 20),                       // pause
            (2, 21),                       // reactivate
            (3, 22),                       // pool freeze / thaw
        ];
        let total: u64 = table.iter().map(|x| x.0).sum();
        let mut r = self.rng.below(total);
        let mut kind = 0u8;
        for (wt, k) in table {
            if r < wt {
                kind = k;
                break;
            }
            r -= wt;
        }
        match kind {
            0 => {
                let s = self.pick_sector(true);
                let p = self.rng.idx(NP);
                let c = self.pick_coin();
                let tiers = self.m.prods[s].as_ref().map(|p| p.cfg.tiers.clone()).unwrap_or_else(|| vec![(10_000, 100)]);
                let (size, cost) = if self.rng.pct(88) {
                    *self.rng.pick(&tiers)
                } else {
                    (self.rng.pick(&tiers).0, self.rng.range(0, 500))
                };
                Act::DepositFee { p, s, c, size, cost, cid: self.rng.range(1, 7) }
            }
            1 => {
                let (s, p, cid) = self.pick_trader(|_, t| t.status == St::Active);
                let req = match self.m.traders.get(&(s, p, cid)) {
                    Some(t) if self.rng.pct(85) => t.payout_count + 1,
                    _ => self.rng.range(0, 6),
                };
                let amount = self.payout_amount();
                Act::Payout { s, p, cid, amount, req }
            }
            2 => {
                let (s, p, cid) = self.pick_trader(|_, t| t.status == St::Active);
                Act::Record { s, p, cid }
            }
            3 => {
                let (s, p, cid) = self.pick_trader(|_, t| t.status == St::Active);
                Act::Flag { s, p, cid }
            }
            4 => {
                let (s, p, prev) = self.pick_trader(|_, t| t.status == St::Failed && !t.reset_used);
                let c = self.pick_coin();
                let phase = if self.rng.pct(85) { self.rng.range(0, 2) as u8 } else { self.rng.range(0, 12) as u8 };
                let right = match (self.m.prods[s].as_ref(), self.m.traders.get(&(s, p, prev))) {
                    (Some(pr), Some(t)) => pr.cfg.reset_bps.get(phase as usize).map(|&b| floor_bps(t.size, b as u64)),
                    _ => None,
                };
                let amount = match right {
                    Some(a) if self.rng.pct(85) => a,
                    Some(a) => a.wrapping_add(self.rng.range(1, 3)),
                    None => self.rng.range(0, 100),
                };
                Act::DepositReset { p, s, c, prev, new: self.rng.range(1, 9), amount, phase }
            }
            5 => {
                let now = self.m.now;
                let (s, p, cid) = self.pick_trader(move |pr, t| t.status == St::Active && t.stale(now, pr.paused_secs_at(now)));
                Act::Abandon { s, p, cid }
            }
            6 => Act::Begin,
            7 => self.gen_settle(),
            8 => Act::Finalize,
            9 => Act::Reconcile { s: self.pick_sector(true), wrong_addr: self.rng.pct(8) },
            10 => {
                let mut p = self.rng.idx(NP);
                if self.bond_heavy && self.rng.pct(80) {
                    // whales: a rich wallet that still has room under its cap
                    let rich: Vec<usize> = (3..NP).filter(|q| self.m.trackers.get(q).map_or(0, |t| t.open) < BOND_WALLET_CAP).collect();
                    if !rich.is_empty() {
                        p = *self.rng.pick(&rich);
                    }
                }
                let c = self.pick_coin();
                let tr = self.m.trackers.get(&p).cloned().unwrap_or_default();
                let idx = if self.rng.pct(92) { tr.next } else { self.rng.range(0, tr.next + 2) };
                let room = BOND_WALLET_CAP.saturating_sub(tr.open);
                let global_room = BOND_GLOBAL_CAP.saturating_sub(self.m.bond_open_total());
                let principal = match self.rng.below(if self.bond_heavy { 8 } else { 9 }) {
                    _ if self.bond_heavy && self.rng.pct(60) => room.min(BOND_WALLET_CAP),
                    0 => BOND_MIN - 1,
                    1 => BOND_MIN,
                    2 => self.rng.range(BOND_MIN, 5_000_000_000),
                    3 => room,
                    4 => room.saturating_add(1),
                    5 => global_room.min(room),
                    6 => global_room.saturating_add(1),
                    7 => BOND_WALLET_CAP,
                    _ => self.rng.range(BOND_MIN, 200_000_000),
                };
                let term = if self.rng.pct(50) { BondTerm::SixMonths } else { BondTerm::NineMonths };
                Act::BondDeposit { p, idx, principal, term, c }
            }
            11 => {
                let live: Vec<(usize, u64)> = self.m.bonds.keys().cloned().collect();
                if !live.is_empty() && self.rng.pct(85) {
                    let (p, idx) = *self.rng.pick(&live);
                    // sometimes somebody else tries to withdraw my bond
                    let p = if self.rng.pct(5) { (p + 1) % NP } else { p };
                    Act::BondRequest { p, idx }
                } else {
                    Act::BondRequest { p: self.rng.idx(NP), idx: self.rng.range(0, 3) }
                }
            }
            12 => {
                let c = self.pick_coin();
                let live = self.m.pools[c];
                let reserve = ceil_bps(live, 2_500).max(self.m.floors[c]);
                let avail = live.saturating_sub(reserve);
                let amount = match self.rng.below(7) {
                    0 => 0,
                    1 => avail,
                    2 => avail.saturating_add(1),
                    3 => 1,
                    4 => self.rng.range(0, avail),
                    5 => live,
                    _ => avail / 2,
                };
                Act::Withdraw { c, amount }
            }
            13 => self.gen_warp(),
            14 => {
                let p = self.rng.idx(NP);
                let c = self.pick_coin();
                let op = match self.rng.below(14) {
                    0..=4 => AtaOp::Create(if self.rng.pct(70) { 0 } else { self.rng.range(1, 10_000) }),
                    5 => AtaOp::Delete,
                    6..=7 => AtaOp::Freeze,
                    8..=9 => AtaOp::Thaw,
                    10 => AtaOp::Reown,
                    11 => *self.rng.pick(&[AtaOp::Uninit, AtaOp::WrongMint]),
                    12 => AtaOp::SysOwned,
                    _ => AtaOp::Foreign,
                };
                Act::Ata { p, c, op }
            }
            15 => Act::Airdrop { p: self.rng.idx(NP), c: self.pick_coin(), amount: self.rng.range(1, 50_000_000_000) },
            16 => {
                let min0 = self.env.svm.minimum_balance_for_rent_exemption(0);
                let lamports = if self.rng.pct(30) { min0 + self.rng.range(0, 40_000_000) } else { min0 + self.rng.below(6_000_000) };
                let p = self.rng.idx(NP);
                let s = self.rng.idx(NS);
                let w = self.w.people[p];
                let (addr, what) = if self.rng.pct(55) {
                    // dust the address that will be created NEXT (pre-funded PDA creation paths)
                    match self.rng.below(6) {
                        0 => {
                            let act: Vec<(usize, usize, u64)> = self.traders_where(|_, t| t.status == St::Active);
                            if let Some(&(s, p, cid)) = act.get(self.rng.idx(act.len().max(1))) {
                                let t = &self.m.traders[&(s, p, cid)];
                                let ts = self.w.sectors[s].trader(&self.w.people[p], cid);
                                (claim_pda(&ts, t.payout_count + 1).0, "payout_claim(next)")
                            } else {
                                (self.w.sectors[s].registry(), "registry")
                            }
                        }
                        1 => (self.w.sectors[s].trader(&w, self.rng.range(1, 7)), "trader_state(next)"),
                        2 => {
                            let next = self.m.trackers.get(&p).map_or(0, |t| t.next);
                            (bond_pda(&w, next).0, "bond_position(next)")
                        }
                        3 => (bond_cap_pda(&w).0, "bond_cap"),
                        4 => {
                            let live: Vec<(usize, u64)> = self.m.bonds.keys().cloned().collect();
                            if let Some(&(p, idx)) = live.get(self.rng.idx(live.len().max(1))) {
                                (bond_claim_pda(&self.w.people[p], idx).0, "bond_claim(next)")
                            } else {
                                (bond_claim_pda(&w, 0).0, "bond_claim")
                            }
                        }
                        _ => (self.w.sectors[s].registry(), "registry"),
                    }
                } else {
                    match self.rng.below(5) {
                        0 => (self.w.sectors[s].registry(), "registry"),
                        1 => (self.w.sectors[s].trader(&w, self.rng.range(1, 7)), "trader_state"),
                        2 => {
                            let cid = self.rng.range(1, 7);
                            let ts = self.w.sectors[s].trader(&w, cid);
                            (claim_pda(&ts, self.rng.range(1, 4)).0, "payout_claim")
                        }
                        3 => (bond_pda(&w, self.rng.range(0, 4)).0, "bond_position"),
                        _ => (bond_claim_pda(&w, self.rng.range(0, 4)).0, "bond_claim"),
                    }
                };
                Act::Dust { addr, lamports, what }
            }
            17 => {
                let s = self.rng.idx(NS);
                let (e, a) = self.m.prods[s].as_ref().map_or((0, 0), |p| (p.emitted, p.amount));
                let t = match self.rng.below(10) {
                    0..=3 => Tally::Valid(e, a),
                    4 => Tally::Valid(e + 1, a),
                    5 => Tally::Valid(e, a.wrapping_add(1)),
                    6 => *self.rng.pick(&[Tally::Missing, Tally::DustOnly]),
                    7 => *self.rng.pick(&[Tally::BadLen, Tally::BadMagic]),
                    8 => Tally::Foreign(e, a),
                    _ => Tally::Valid(e.saturating_sub(1), a),
                };
                Act::SetTally { s, t }
            }
            18 => {
                let s = if unreg && self.rng.pct(85) {
                    *self.rng.pick(&(0..NS).filter(|&s| self.m.prods[s].is_none()).collect::<Vec<_>>())
                } else {
                    self.rng.idx(NS)
                };
                let mut cfg = rand_cfg(&mut self.rng);
                if self.rng.pct(20) {
                    match self.rng.below(3) {
                        0 => cfg.fee_bps = 10_001,
                        1 => cfg.reset_bps = vec![1; 9],
                        _ => cfg.tiers = (0..33).map(|i| (1_000 + i, 1)).collect(),
                    }
                }
                Act::Register { s, cfg }
            }
            19 => {
                let s = self.pick_sector(true);
                let mut cfg = rand_cfg(&mut self.rng);
                if self.rng.pct(6) {
                    cfg.fee_bps = 10_001;
                }
                if self.rng.pct(35) {
                    // lowering the cap under an Active trader that already got paid: PayoutCapReached
                    let paid: Vec<((usize, usize, u64), u64)> =
                        self.m.traders.iter().filter(|((ts, _, _), t)| *ts == s && t.status == St::Active && t.payout_count >= 1).map(|(k, t)| (*k, t.payout_count)).collect();
                    if paid.is_empty() {
                        cfg.max_payout = 1;
                    } else {
                        let ((ts, tp, tc), count) = *self.rng.pick(&paid);
                        cfg.max_payout = count;
                        // ... and immediately ask that trader for another payout (if the update succeeds)
                        self.queue.push_back(Act::Payout { s: ts, p: tp, cid: tc, amount: 1_000, req: count + 1 });
                    }
                }
                Act::Update { s, cfg }
            }
            20 => Act::Pause { s: self.pick_sector(true) },
            22 => {
                // freeze or thaw a payout pool; often both, so the both-frozen path is visited
                let c = self.pick_coin();
                let frozen = if self.m.frozen[c] { self.rng.pct(35) } else { self.rng.pct(75) };
                if self.rng.pct(30) {
                    self.queue.push_back(Act::PoolFreeze { c: 1 - c, frozen });
                }
                Act::PoolFreeze { c, frozen }
            }
            _ => Act::Reactivate { s: self.pick_sector(true) },
        }
    }

    fn ri(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.rng.below((hi - lo + 1) as u64) as i64
    }

    /// A clock jump. Jumps that land within a second of a rule's boundary also queue the
    /// action that rule governs, so the boundary itself is exercised (not just the jump).
    fn gen_warp(&mut self) -> Act {
        let now = self.m.now;
        let t = match self.rng.below(16) {
            0 => now + self.ri(1, 3_600),
            1 => now + D,
            2 => now + self.ri(1, 3 * D),
            3 | 4 => {
                // around the 5-day heartbeat gap
                if self.m.cyc.started != 0 {
                    self.queue.push_back(Act::Begin);
                    self.m.cyc.started + GAP + self.ri(-1, 1)
                } else {
                    now + 5 * D
                }
            }
            5 | 6 => {
                // around a trader's 7-day inactivity boundary: then poke that trader
                let ts: Vec<((usize, usize, u64), i64)> =
                    self.m.traders.iter().filter(|(_, t)| t.status == St::Active).map(|(k, t)| (*k, t.last)).collect();
                if ts.is_empty() {
                    now + 7 * D
                } else {
                    let ((s, p, cid), last) = ts[self.rng.idx(ts.len())];
                    let follow = match self.rng.below(3) {
                        0 => Act::Record { s, p, cid },
                        1 => Act::Abandon { s, p, cid },
                        _ => {
                            let req = self.m.traders[&(s, p, cid)].payout_count + 1;
                            Act::Payout { s, p, cid, amount: 1_000, req }
                        }
                    };
                    self.queue.push_back(follow);
                    last + INACTIVITY + self.ri(-1, 1)
                }
            }
            7 | 8 | 9 => {
                // around a bond's lock / maturity boundary: then try to withdraw that bond
                let bs: Vec<((usize, u64), Bond)> = self.m.bonds.iter().map(|(k, b)| (*k, b.clone())).collect();
                if bs.is_empty() {
                    now + 91 * D
                } else {
                    let ((p, idx), b) = bs[self.rng.idx(bs.len())].clone();
                    let (term, lock, _) = term_secs(b.term);
                    let base = if self.rng.pct(50) { lock } else { term };
                    self.queue.push_back(Act::BondRequest { p, idx });
                    b.created_at + base + self.ri(-1, 1)
                }
            }
            10 => now + self.ri(30, 120) * D,
            11 => now + 181 * D,
            12 => now - self.ri(1, 3_600), // the clock never goes far back, but it may wobble
            _ => now + self.ri(0, 8 * D),
        };
        // keep time sane: never before the epoch of the run, never backwards by more than an hour
        Act::Warp { t: t.max(now - 3_600).max(T0) }
    }

    // ----------------------------------------------------------- building
    fn build(&mut self, a: &Act) -> Option<Instruction> {
        let w = self.w.clone();
        let coin = |c: usize| if c == 0 { Coin::Usdc } else { Coin::Usdt };
        let ix = match a {
            Act::Register { s, cfg } => register_ix(&self.env, &w.sectors[*s], &cfg.to_cfg()),
            Act::Update { s, cfg } => update_ix(&self.env, &w.sectors[*s], &cfg.to_cfg()),
            Act::Pause { s } => pause_ix(&self.env, &w.sectors[*s]),
            Act::Reactivate { s } => reactivate_ix(&self.env, &w.sectors[*s]),
            Act::Withdraw { c, amount } => withdraw_ix(&self.env, coin(*c), *amount),
            Act::DepositFee { p, s, c, size, cost, cid } => {
                let ix = deposit_fee_ix_coin(&self.env, &w.sectors[*s], &w.people[*p], *cid, *cost, *size, coin(*c));
                self.rent_swap(ix)
            }
            Act::DepositReset { p, s, c, prev, new, amount, phase } => {
                let ix = reset_ix_coin(&self.env, &w.sectors[*s], &w.people[*p], *prev, *new, *amount, *phase, coin(*c));
                self.rent_swap(ix)
            }
            Act::Record { s, p, cid } => record_ix(&w.sectors[*s], &w.people[*p], *cid),
            Act::Flag { s, p, cid } => flag_ix(&w.sectors[*s], &w.people[*p], *cid),
            Act::Abandon { s, p, cid } => abandon_ix(&self.keepers[self.step % NK].pubkey(), &w.sectors[*s], &w.people[*p], *cid),
            Act::Payout { s, p, cid, amount, req } => {
                let ix = payout_ix(&self.env, &w.sectors[*s], &w.people[*p], *cid, *amount, *req);
                self.rent_swap(ix)
            }
            // permissionless callers are keepers, never the fee payer (a fee payer is always a signer)
            Act::Begin => begin_ix(&self.keepers[self.step % NK].pubkey(), &self.env),
            Act::Finalize => finalize_ix(&self.keepers[self.step % NK].pubkey(), &self.env),
            Act::Settle { caller, items, junk_tail } => {
                let mut ix = settle_ix(&self.keepers[*caller].pubkey(), &self.env, items);
                for _ in 0..*junk_tail {
                    ix.accounts.push(anchor_lang::solana_program::instruction::AccountMeta::new(Pubkey::new_unique(), false));
                }
                ix
            }
            Act::Reconcile { s, wrong_addr } => {
                let mut ix = reconcile_ix(&self.keepers[self.step % NK].pubkey(), &w.sectors[*s]);
                if *wrong_addr {
                    let good = tally_addr(&w.sectors[*s]);
                    swap_account(&mut ix, &good, &Pubkey::new_unique());
                }
                ix
            }
            Act::BondDeposit { p, idx, principal, term, c } => deposit_bond_ix(&self.env, &w.people[*p], *idx, *principal, *term, coin(*c)),
            Act::BondRequest { p, idx } => request_bond_payout_ix(&self.env, &w.people[*p], *idx),
            _ => return None,
        };
        Some(ix)
    }

    /// The fee payer must not also be the rent payer (fees would blur the lamport flows).
    fn rent_swap(&mut self, mut ix: Instruction) -> Instruction {
        let old = self.env.payer.pubkey();
        swap_account(&mut ix, &old, &self.rent_payer.pubkey());
        ix
    }

    /// Hostile variants: every one of these MUST be rejected.
    fn mutations(&mut self, a: &Act) -> Vec<(String, Mutn)> {
        let w = self.w.clone();
        let e = &self.env;
        let rnd = Pubkey::new_unique();
        let mut v: Vec<(String, Mutn)> = vec![("truncate data".into(), Mutn::Trunc), ("bad discriminator".into(), Mutn::BadDisc), ("drop last account".into(), Mutn::DropLast)];
        macro_rules! sw {
            ($name:expr, $slot:expr, $k:expr) => {
                v.push((format!("swap {} (slot {})", $name, $slot), Mutn::Swap($slot, $k)))
            };
        }
        macro_rules! un {
            ($name:expr, $slot:expr) => {
                v.push((format!("unsign {} (slot {})", $name, $slot), Mutn::Unsign($slot)))
            };
        }
        let other_sector = |s: usize| (s + 1) % NS;
        match a {
            Act::Register { .. } | Act::Update { .. } | Act::Pause { .. } | Act::Reactivate { .. } => {
                un!("sl8_admin", 0);
                un!("rov_admin", 1);
                v.push(("swap sl8_admin for rov (slot 0)".into(), Mutn::Swap(0, e.rov.pubkey())));
                v.push(("swap rov_admin for random signer (slot 1)".into(), Mutn::Swap(1, rnd)));
            }
            Act::Withdraw { c, .. } => {
                let coin = if *c == 0 { Coin::Usdc } else { Coin::Usdt };
                un!("sl8_admin", 0);
                un!("rov_admin", 1);
                v.push(("swap sl8_admin for random (slot 0)".into(), Mutn::Swap(0, rnd)));
                let (mint, pool, sl8) = e.coin(coin);
                let (omint, opool, osl8) = e.coin(if *c == 0 { Coin::Usdt } else { Coin::Usdc });
                let person_ta = e.wallet_ta(&w.people[0], coin);
                for (n, slot, k) in [
                    ("vault", AW.vault, pool),
                    ("mint", AW.mint, omint),
                    ("pool", AW.pool, opool),
                    ("pool for sl8 acct", AW.pool, sl8),
                    ("sl8 destination for a person account", AW.sl8_ta, person_ta),
                    ("sl8 destination for other coin", AW.sl8_ta, osl8),
                    ("sl8 destination for the pool", AW.sl8_ta, pool),
                    ("token program", AW.token_program, TOKEN_2022_ID),
                    ("mint for random", AW.mint, rnd),
                ] {
                    sw!(n, slot, k);
                }
                let _ = mint;
            }
            Act::DepositFee { p, s, c, .. } | Act::DepositReset { p, s, c, .. } => {
                let reset = matches!(a, Act::DepositReset { .. });
                let sl = if reset { DR } else { DF };
                let coin = if *c == 0 { Coin::Usdc } else { Coin::Usdt };
                let (mint, pool, sl8) = e.coin(coin);
                let (omint, opool, _) = e.coin(if *c == 0 { Coin::Usdt } else { Coin::Usdc });
                let o = (*p + 1) % NP;
                let other_ta = e.wallet_ta(&w.people[o], coin);
                un!("sector_authority", 0);
                un!("trader", sl.trader);
                un!("payer", if reset { 4 } else { 3 });
                sw!("sector_authority for another sector's", 0, w.sectors[other_sector(*s)].authority);
                sw!("registry for another sector's", 1, w.sectors[other_sector(*s)].registry());
                sw!("vault for random", sl.vault, rnd);
                sw!("trader for another person", sl.trader, w.people[o]);
                sw!("trader token account for another person's", sl.trader_ta, other_ta);
                sw!("trader token account for the pool", sl.trader_ta, pool);
                sw!("mint for the other mint", sl.mint, omint);
                sw!("pool for the other pool", sl.pool, opool);
                sw!("pool for the SL8 account", sl.pool, sl8);
                sw!("pool for a person account", sl.pool, other_ta);
                sw!("sl8 destination for a person account", sl.sl8_ta, other_ta);
                sw!("sl8 destination for the pool", sl.sl8_ta, pool);
                sw!("token program for token-2022", sl.token_program, TOKEN_2022_ID);
                sw!("new trader_state for random", if reset { 3 } else { 2 }, rnd);
                let _ = mint;
            }
            Act::Record { s, .. } | Act::Flag { s, .. } => {
                un!("sector_authority", 0);
                sw!("sector_authority for another sector's", 0, w.sectors[other_sector(*s)].authority);
                sw!("sector_authority for random", 0, rnd);
                sw!("registry for another sector's", 1, w.sectors[other_sector(*s)].registry());
                sw!("trader_state for random", 2, rnd);
            }
            Act::Abandon { s, .. } => {
                un!("caller", 0);
                sw!("registry for another sector's", 1, w.sectors[other_sector(*s)].registry());
                sw!("trader_state for random", 2, rnd);
            }
            Act::Payout { s, .. } => {
                un!("sector_authority", 0);
                un!("payer", PO.payer);
                sw!("sector_authority for another sector's", 0, w.sectors[other_sector(*s)].authority);
                sw!("registry for another sector's", 1, w.sectors[other_sector(*s)].registry());
                sw!("trader_state for random", PO.trader_state, rnd);
                sw!("vault for the registry", PO.vault, w.sectors[*s].registry());
                sw!("claim for random", PO.claim, rnd);
                sw!("system program for token program", PO.system, spl_token::ID);
            }
            Act::Begin | Act::Finalize => {
                un!("caller", 0);
                sw!("vault for random", 1, rnd);
                sw!("usdc pool for the usdt pool", 2, e.usdt_pool);
                sw!("usdt pool for the usdc pool", 3, e.usdc_pool);
            }
            Act::Settle { .. } => {
                un!("caller", 0);
                sw!("vault for random", 1, rnd);
                sw!("usdc mint for usdt mint", 2, e.usdt);
                sw!("usdt mint for usdc mint", 3, e.usdc);
                sw!("usdc pool for usdt pool", 4, e.usdt_pool);
                sw!("usdt pool for usdc pool", 5, e.usdc_pool);
                sw!("token program for token-2022", 6, TOKEN_2022_ID);
            }
            Act::Reconcile { s, .. } => {
                un!("caller", 0);
                sw!("registry for another sector's", 1, w.sectors[other_sector(*s)].registry());
            }
            Act::BondDeposit { p, c, .. } => {
                let coin = if *c == 0 { Coin::Usdc } else { Coin::Usdt };
                let (_, pool, sl8) = e.coin(coin);
                let (omint, opool, _) = e.coin(if *c == 0 { Coin::Usdt } else { Coin::Usdc });
                let o = (*p + 1) % NP;
                un!("depositor", BD.depositor);
                sw!("depositor for another person", BD.depositor, w.people[o]);
                sw!("vault for random", BD.vault, rnd);
                sw!("source token account for another person's", BD.source, e.wallet_ta(&w.people[o], coin));
                sw!("source token account for the pool", BD.source, pool);
                sw!("mint for the other mint", BD.mint, omint);
                sw!("pool for the other pool", BD.pool, opool);
                sw!("pool for the SL8 account", BD.pool, sl8);
                sw!("sl8 for the pool", BD.sl8, pool);
                sw!("position for random", BD.position, rnd);
                sw!("tracker for another person's", BD.tracker, bond_cap_pda(&w.people[o]).0);
                sw!("token program for token-2022", BD.token_program, TOKEN_2022_ID);
            }
            Act::BondRequest { p, idx } => {
                let o = (*p + 1) % NP;
                un!("depositor", BR.depositor);
                sw!("vault for random", BR.vault, rnd);
                sw!("position for another person's", BR.position, bond_pda(&w.people[o], *idx).0);
                sw!("position for the tracker", BR.position, bond_cap_pda(&w.people[*p]).0);
                sw!("tracker for another person's", BR.tracker, bond_cap_pda(&w.people[o]).0);
                sw!("tracker for the position", BR.tracker, bond_pda(&w.people[*p], *idx).0);
                sw!("claim for random", BR.claim, rnd);
            }
            _ => {}
        }
        v
    }

    fn apply_mutation(&self, ix: &mut Instruction, m: Mutn) {
        match m {
            Mutn::Unsign(slot) => unsign_slot(ix, slot),
            Mutn::Swap(slot, k) => ix.accounts[slot].pubkey = k,
            Mutn::Trunc => {
                if ix.data.len() > 8 {
                    let cut = 1 + (ix.data.len() - 9).min(2);
                    ix.data.truncate(ix.data.len() - cut);
                } else {
                    ix.data.truncate(4);
                }
            }
            Mutn::BadDisc => ix.data[0] ^= 0xFF,
            Mutn::DropLast => {
                ix.accounts.pop();
            }
        }
    }

    // ------------------------------------------------------------ running
    fn run(&mut self, ixs: &[Instruction]) -> TransactionResult {
        let fp = dup(&self.env.payer);
        self.env.send_with(ixs, &fp, &[])
    }

    fn err_matches(r: &TransactionResult, allowed: &[E]) -> bool {
        let Err(f) = r else { return false };
        if allowed.contains(&E::Any) {
            return true;
        }
        match &f.err {
            TransactionError::InstructionError(_, anchor_lang::solana_program::instruction::error::InstructionError::Custom(code)) => {
                allowed.iter().any(|e| match e {
                    E::C(c) => code == c,
                    E::Sys0 => *code == 0,
                    E::Any => true,
                })
            }
            _ => false,
        }
    }

    fn describe(r: &TransactionResult) -> String {
        match r {
            Ok(_) => "ok".into(),
            Err(f) => format!("{:?}", f.err),
        }
    }

    fn logs_of(r: &TransactionResult) -> String {
        match r {
            Ok(m) => m.logs.join("\n"),
            Err(f) => f.meta.logs.join("\n"),
        }
    }

    fn note(&mut self, s: String) {
        if self.log.len() == 10 {
            self.log.pop_front();
        }
        self.log.push_back(format!("#{} {}", self.step, s));
    }

    fn bump(&mut self, kind: &'static str, ok: bool) {
        let m = if ok { &mut self.stats.ok } else { &mut self.stats.err };
        *m.entry(kind).or_default() += 1;
    }

    /// Plan, run and check one action.
    fn exec(&mut self, a: Act) {
        self.step += 1;
        // keep the fee payer, rent payer and keepers solvent (these are test-side top-ups)
        self.topup();
        let kind = a.kind();
        let desc = format!("{a:?}");
        let desc = if desc.len() > 260 { format!("{}...", &desc[..260]) } else { desc };

        if !a.is_program_ix() {
            let st = self.m.step(&a).unwrap_or_else(|_| unreachable!("env events never fail in the model"));
            for t in Self::tags_for(&self.m, &a, &st.m, st.out) {
                self.stats.tag(t);
            }
            self.apply_env(&a);
            self.m = st.m;
            self.note(format!("{desc} -> env"));
            self.bump(kind, true);
            self.check_all();
            return;
        }

        let pred = self.m.step(&a);
        let Some(mut ix) = self.build(&a) else { unreachable!() };

        // choose a variant: honest | hostile mutation | composite (valid ix + failing ix)
        #[derive(PartialEq)]
        enum Mode {
            Honest,
            Hostile,
            Composite,
        }
        let mut label = String::new();
        let mut double = false;
        let mut mode = Mode::Honest;
        if self.rng.pct(11) {
            let ms = self.mutations(&a);
            if !ms.is_empty() {
                let (name, mu) = ms[self.rng.idx(ms.len())].clone();
                self.apply_mutation(&mut ix, mu);
                label = name;
                mode = Mode::Hostile;
            }
        } else if pred.is_ok() && self.rng.pct(5) {
            mode = Mode::Composite;
            // [valid, failing] must roll back the valid one; [ix, ix] must fail on the second
            // (nothing that closes or creates an account may be done twice in one transaction)
            double = matches!(
                a,
                Act::Register { .. }
                    | Act::Pause { .. }
                    | Act::DepositFee { .. }
                    | Act::DepositReset { .. }
                    | Act::Begin
                    | Act::Finalize
                    | Act::Settle { .. }
                    | Act::Flag { .. }
                    | Act::Abandon { .. }
                    | Act::BondDeposit { .. }
                    | Act::BondRequest { .. }
                    | Act::Payout { .. }
            ) && self.rng.pct(60);
            label = if double { "composite(the same instruction twice in one transaction)" } else { "composite(valid + failing zero-amount withdrawal)" }.into();
        }
        // fee-payer excluded from the fingerprint (it really pays the fee)
        let payer = self.env.payer.pubkey();
        let before = state_fingerprint(&self.env.svm, &[payer]);
        // lamports of the accounts the model says get created / closed
        let pre_lam: BTreeMap<Pubkey, u64> = match &pred {
            Ok(st) => st
                .evs
                .iter()
                .map(|ev| match ev {
                    Ev::Created { addr, .. } | Ev::Closed { addr, .. } => (*addr, self.env.lamports(addr)),
                })
                .collect(),
            Err(_) => BTreeMap::new(),
        };
        let pre_pools = self.env.pools();
        let r = match mode {
            Mode::Composite => {
                let second = if double { ix.clone() } else { withdraw_ix(&self.env, Coin::Usdc, 0) };
                self.run(&[ix.clone(), second])
            }
            _ => self.run(&[ix.clone()]),
        };
        let after = state_fingerprint(&self.env.svm, &[payer]);

        match mode {
            Mode::Hostile | Mode::Composite => {
                self.note(format!("{desc} [{label}] -> {}", Self::describe(&r)));
                if r.is_ok() {
                    self.fail(
                        "13",
                        format!("a HOSTILE call SUCCEEDED ({label}) for {desc}\nlogs:\n{}", Self::logs_of(&r)),
                    );
                }
                self.assert_unchanged(&format!("a rejected hostile call ({label})"), &desc, &before, &after);
                self.bump(kind, false);
                if mode == Mode::Hostile {
                    self.stats.hostile_rejected += 1;
                } else {
                    self.stats.composite_rejected += 1;
                }
                self.check_all();
                return;
            }
            Mode::Honest => {}
        }

        match pred {
            Err(allowed) => {
                self.note(format!("{desc} -> {}", Self::describe(&r)));
                if r.is_ok() {
                    // invariant 7 is the paused-product special case of this
                    let inv = if matches!(a, Act::DepositFee { .. } | Act::DepositReset { .. } | Act::Payout { .. })
                        && allowed.contains(&ev(VaultError::ProductNotActive))
                    {
                        "7"
                    } else {
                        "13"
                    };
                    self.fail(inv, format!("the model says this must FAIL with one of {allowed:?}, but it SUCCEEDED: {desc}\nlogs:\n{}", Self::logs_of(&r)));
                }
                if !Self::err_matches(&r, &allowed) {
                    self.fail(
                        "13",
                        format!("wrong error: the model expects one of {allowed:?}, the chain returned {}: {desc}\nlogs:\n{}", Self::describe(&r), Self::logs_of(&r)),
                    );
                }
                self.assert_unchanged("a rejected instruction", &desc, &before, &after);
                self.bump(kind, false);
                if let Err(f) = &r {
                    if let TransactionError::InstructionError(_, anchor_lang::solana_program::instruction::error::InstructionError::Custom(code)) = &f.err {
                        *self.stats.codes.entry(format!("{kind}:{code}")).or_default() += 1;
                    }
                }
            }
            Ok(st) => {
                self.note(format!("{desc} -> {}", Self::describe(&r)));
                let meta = match &r {
                    Ok(m) => m,
                    Err(_) => self.fail(
                        "13",
                        format!(
                            "the model says this must SUCCEED, the chain returned {}: {desc}\nlogs:\n{}",
                            Self::describe(&r),
                            Self::logs_of(&r)
                        ),
                    ),
                };
                match st.out {
                    Out::None => {}
                    Out::Activity(o) => {
                        if meta.return_data.data != vec![o as u8] {
                            self.fail("13", format!("return data {:?} != model outcome {:?}: {desc}", meta.return_data.data, o));
                        }
                    }
                    Out::Payout(o) => {
                        if meta.return_data.data != vec![o as u8] {
                            self.fail("13", format!("return data {:?} != model outcome {:?}: {desc}", meta.return_data.data, o));
                        }
                    }
                }
                // invariant 11 (per transaction): a settle pays out exactly what leaves the pools
                if matches!(a, Act::Settle { .. }) {
                    let post = self.env.pools();
                    let paid = (pre_pools.0 as u128 + pre_pools.1 as u128).saturating_sub(post.0 as u128 + post.1 as u128);
                    self.cycle_paid += paid;
                }
                // rent lamport flows (invariant 14)
                for ev in &st.evs {
                    match ev {
                        Ev::Created { addr, space, payer } => {
                            let required = self.env.svm.minimum_balance_for_rent_exemption(*space);
                            let have = pre_lam.get(addr).copied().unwrap_or(0);
                            let paid = required.saturating_sub(have) as i128;
                            let k = self.actor_key(*payer);
                            *self.exp_lam.entry(k).or_default() -= paid;
                        }
                        Ev::Closed { addr, to } => {
                            let got = pre_lam.get(addr).copied().unwrap_or(0) as i128;
                            let k = self.actor_key(*to);
                            *self.exp_lam.entry(k).or_default() += got;
                        }
                    }
                }
                for t in Self::tags_for(&self.m, &a, &st.m, st.out) {
                    self.stats.tag(t);
                }
                for ev in &st.evs {
                    if let Ev::Created { addr, .. } = ev {
                        if pre_lam.get(addr).copied().unwrap_or(0) > 0 {
                            self.stats.tag(format!("created_over_dust:{kind}"));
                        }
                    }
                }
                self.m = st.m;
                self.bump(kind, true);
                // the sector keeps its payout tally in step with accepted requests (usually)
                if let Act::Payout { s, .. } = &a {
                    if st.out == Out::Payout(PayoutOutcome::Paid) && self.rng.pct(90) {
                        let pr = self.m.prods[*s].as_ref().unwrap();
                        let t = Tally::Valid(pr.emitted, pr.amount);
                        self.exec_env(Act::SetTally { s: *s, t });
                    }
                }
            }
        }
        self.check_all();
    }

    /// Which interesting branches this accepted action walked through.
    fn tags_for(old: &Model, a: &Act, new: &Model, out: Out) -> Vec<String> {
        let mut t: Vec<String> = vec![];
        match a {
            Act::Settle { .. } => {
                if old.cyc.avail < old.cyc.owed {
                    t.push("settle:ratio_below_1".into());
                }
                if old.frozen[0] && old.frozen[1] {
                    t.push("settle:both_pools_frozen".into());
                } else if (old.frozen[0] && old.pools[0] > 0) || (old.frozen[1] && old.pools[1] > 0) {
                    t.push("settle:with_a_frozen_pool".into());
                    if (0..2).any(|i| new.pools[i] < old.pools[i]) {
                        t.push("settle:paid_from_the_other_pool_while_one_is_frozen".into());
                    }
                }
                for (k, oc) in &old.claims {
                    match new.claims.get(k) {
                        None => t.push("settle:claim_closed".into()),
                        Some(nc) if nc.last != oc.last => {
                            if nc.owed < oc.owed {
                                t.push("settle:partial_payment".into());
                            } else if !old.ata_usable(oc.person) {
                                t.push("settle:skipped_unusable_destination".into());
                            } else {
                                t.push("settle:zero_payment_stays_open".into());
                            }
                        }
                        _ => {}
                    }
                    if let CKey::B { .. } = k {
                        if new.claims.get(k).map_or(true, |nc| nc.last != oc.last) && !t.is_empty() {
                            t.push("settle:bond_claim_touched".into());
                        }
                    }
                }
                if new.pools[0] < old.pools[0] && new.pools[1] < old.pools[1] {
                    t.push("settle:both_pools_drained_in_one_batch".into());
                }
            }
            Act::Payout { .. } => {
                if out == Out::Payout(PayoutOutcome::Abandoned) {
                    t.push("payout:stale_abandoned".into());
                }
                let g_old = old.traders.values().filter(|x| x.status == St::Graduated).count();
                let g_new = new.traders.values().filter(|x| x.status == St::Graduated).count();
                if g_new > g_old {
                    t.push("payout:graduated".into());
                }
                if out == Out::Payout(PayoutOutcome::Paid) {
                    t.push("payout:queued".into());
                    if new.claims_total() >= CLAIMS_CEILING as u128 {
                        t.push("payout:filled_to_the_ceiling".into());
                    }
                }
            }
            Act::Record { .. } => t.push(format!("record:{:?}", out)),
            Act::BondRequest { p, idx } => {
                if let Some(b) = old.bonds.get(&(*p, *idx)) {
                    let (term, _, _) = term_secs(b.term);
                    t.push(if old.now - b.created_at >= term { "bond:withdraw_matured" } else { "bond:withdraw_early" }.into());
                    if old.now - b.created_at == term || old.now - b.created_at == term_secs(b.term).1 {
                        t.push("bond:withdraw_exactly_on_boundary".into());
                    }
                }
            }
            Act::BondDeposit { .. } => {
                if new.bond_open_total() > 550_000_000_000 {
                    t.push("bond:global_cap_nearly_full".into());
                }
                if new.trackers.values().any(|x| x.open >= 49_000_000_000) {
                    t.push("bond:wallet_nearly_full".into());
                }
            }
            Act::Withdraw { c, amount } => {
                let live = old.pools[*c];
                if *amount == live.saturating_sub(ceil_bps(live, 2_500).max(old.floors[*c])) {
                    t.push("withdraw:exact_max".into());
                }
                if old.floors[*c] > ceil_bps(live, 2_500) {
                    t.push("withdraw:stored_floor_binds".into());
                }
            }
            Act::Reconcile { s, .. } => {
                if new.prods[*s].as_ref().map_or(false, |p| !p.active) {
                    t.push("reconcile:paused_a_product".into());
                } else {
                    t.push("reconcile:matched".into());
                }
            }
            Act::Begin => {
                if (0..2).any(|i| old.frozen[i] && old.pools[i] > 0) {
                    t.push("begin:frozen_pool_left_out".into());
                }
                t.push(if old.claims.is_empty() { "begin:empty_cycle" } else { "begin:with_claims" }.into());
                if old.cyc.started != 0 && old.now == old.cyc.started + GAP {
                    t.push("begin:exactly_at_gap".into());
                }
            }
            Act::Finalize => t.push("finalize:ok".into()),
            Act::Warp { t: nt } => {
                if *nt < old.now {
                    t.push("warp:backwards".into());
                }
            }
            Act::Pause { .. } => t.push("pause:ok".into()),
            Act::Abandon { .. } => t.push("abandon:ok".into()),
            Act::Update { .. } => t.push("update:ok".into()),
            _ => {}
        }
        t
    }

    /// A test-side event executed inside another step (no step counter bump, no check).
    fn exec_env(&mut self, a: Act) {
        let st = self.m.step(&a).unwrap_or_else(|_| unreachable!());
        self.apply_env(&a);
        self.m = st.m;
    }

    fn topup(&mut self) {
        let min = 1_000_000_000u64;
        let mut need: Vec<Pubkey> = vec![self.env.payer.pubkey()];
        need.extend(self.actor_keys());
        for k in need {
            if self.env.lamports(&k) < min {
                self.env.fund(&k);
                self.env.svm.expire_blockhash();
                if let Some(e) = self.exp_lam.get_mut(&k) {
                    *e += 10_000_000_000;
                }
            }
        }
    }

    /// Makes the test-side event real on the chain.
    fn apply_env(&mut self, a: &Act) {
        let w = self.w.clone();
        match a {
            Act::Warp { t } => self.env.set_time(*t),
            Act::Ata { p, c, op } => {
                let coin = if *c == 0 { Coin::Usdc } else { Coin::Usdt };
                let mint = w.mint(*c);
                let other_mint = w.mint(1 - *c);
                let wallet = w.people[*p];
                let addr = ata(&wallet, &mint);
                let old_mode = self.m.ata_mode[*p][*c];
                match op {
                    AtaOp::Create(amt) => {
                        self.env.make_ata(&wallet, coin, *amt);
                    }
                    AtaOp::Delete => self.remove(&addr),
                    AtaOp::Freeze | AtaOp::Thaw | AtaOp::Reown => {
                        // only meaningful on a live token account the model calls usable / frozen
                        let applicable = match op {
                            AtaOp::Freeze | AtaOp::Reown => old_mode == AtaMode::Usable,
                            _ => old_mode == AtaMode::Frozen,
                        };
                        if applicable {
                            let other = w.people[(*p + 1) % NP];
                            let op = *op;
                            self.env.edit_token_account(&addr, |t| match op {
                                AtaOp::Freeze => t.state = AccountState::Frozen,
                                AtaOp::Thaw => t.state = AccountState::Initialized,
                                _ => t.owner = other,
                            });
                        }
                    }
                    AtaOp::Uninit => {
                        self.env.set_raw(&addr, vec![0u8; SplAccount::LEN], spl_token::ID);
                    }
                    AtaOp::WrongMint => {
                        self.env.set_token_account(&addr, &other_mint, &wallet, 0);
                    }
                    AtaOp::SysOwned => {
                        self.remove(&addr);
                        let l = self.env.svm.minimum_balance_for_rent_exemption(0) + 5;
                        self.env
                            .svm
                            .set_account(addr, solana_account::Account { lamports: l, data: vec![], owner: anchor_lang::solana_program::system_program::ID, executable: false, rent_epoch: 0 })
                            .unwrap();
                    }
                    AtaOp::Foreign => {
                        let mut data = vec![0u8; SplAccount::LEN];
                        SplAccount::pack(
                            SplAccount {
                                mint,
                                owner: wallet,
                                amount: 0,
                                delegate: anchor_spl::token::spl_token::solana_program::program_option::COption::None,
                                state: AccountState::Initialized,
                                is_native: anchor_spl::token::spl_token::solana_program::program_option::COption::None,
                                delegated_amount: 0,
                                close_authority: anchor_spl::token::spl_token::solana_program::program_option::COption::None,
                            },
                            &mut data,
                        )
                        .unwrap();
                        self.env.set_raw(&addr, data, TOKEN_2022_ID);
                    }
                }
                if !self.env.tracked.contains(&addr) {
                    self.env.tracked.push(addr);
                }
            }
            Act::Airdrop { p, c, amount } => {
                let coin = if *c == 0 { Coin::Usdc } else { Coin::Usdt };
                let ta = self.env.wallet_ta(&w.people[*p], coin);
                let mint = w.mint(*c);
                let cur = self.env.token_balance(&ta);
                self.env.set_token_account(&ta, &mint, &w.people[*p], cur + amount);
            }
            Act::Dust { addr, lamports, .. } => {
                if self.env.svm.airdrop(addr, *lamports).is_ok() {
                    self.env.svm.expire_blockhash();
                }
            }
            Act::PoolFreeze { c, frozen } => {
                let pool = if *c == 0 { self.env.usdc_pool } else { self.env.usdt_pool };
                // keep the account's lamports: the pool may have been dusted before init_vault and
                // the rent invariant (14) tracks its exact balance
                let acct = self.env.svm.get_account(&pool).unwrap();
                let mut t = SplAccount::unpack(&acct.data).unwrap();
                t.state = if *frozen { AccountState::Frozen } else { AccountState::Initialized };
                let mut data = vec![0u8; SplAccount::LEN];
                SplAccount::pack(t, &mut data).unwrap();
                self.env.svm.set_account(pool, solana_account::Account { data, ..acct }).unwrap();
            }
            Act::SetTally { s, t } => {
                let sec = &w.sectors[*s];
                match t {
                    Tally::Missing => self.env.remove_tally(sec),
                    Tally::DustOnly => {
                        let l = self.env.svm.minimum_balance_for_rent_exemption(0) + 17;
                        self.env.dust_tally(sec, l);
                    }
                    Tally::Valid(c, tot) => self.env.set_tally(sec, *c, *tot),
                    Tally::BadLen => self.env.set_tally_raw(sec, vec![0xCD; 7], sec.id),
                    Tally::BadMagic => self.env.set_tally_raw(sec, vec![0u8; si::PAYOUT_TALLY_MIN_LEN], sec.id),
                    Tally::Foreign(c, tot) => {
                        let mut data = vec![0u8; si::PAYOUT_TALLY_MIN_LEN];
                        si::PayoutTally { requested_count: *c, requested_total: *tot }.write_into(&mut data).unwrap();
                        self.env.set_tally_raw(sec, data, Pubkey::new_unique());
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn remove(&mut self, addr: &Pubkey) {
        self.env.svm.set_account(*addr, solana_account::Account::default()).unwrap();
    }

    /// Invariant 9: a call that errored must leave every non-sysvar account (except the fee
    /// payer) byte-for-byte as it was.
    fn assert_unchanged(&self, what: &str, desc: &str, before: &BTreeMap<Pubkey, u64>, after: &BTreeMap<Pubkey, u64>) {
        if before != after {
            self.fail("9", format!("{what} changed account state: {desc}\n{}", self.diff(before, after)));
        }
    }

    fn diff(&self, before: &BTreeMap<Pubkey, u64>, after: &BTreeMap<Pubkey, u64>) -> String {
        let mut out = vec![];
        for (k, v) in before {
            match after.get(k) {
                None => out.push(format!("  removed {k}")),
                Some(a) if a != v => out.push(format!("  changed {k}")),
                _ => {}
            }
        }
        for k in after.keys() {
            if !before.contains_key(k) {
                out.push(format!("  created {k}"));
            }
        }
        out.join("\n")
    }
}

// ============================================================ invariant checks

struct ChainView {
    vault: VaultState,
    regs: Vec<(Pubkey, ProductRegistry)>,
    traders: Vec<(Pubkey, TraderState)>,
    claims: Vec<(Pubkey, PayoutClaim)>,
    bonds: Vec<(Pubkey, BondPosition)>,
    trackers: Vec<(Pubkey, BondCapTracker)>,
    toks: BTreeMap<Pubkey, SplAccount>,
}

impl Fuzz {
    fn read_chain(&self) -> ChainView {
        let mut vault = None;
        let (mut regs, mut traders, mut claims, mut bonds, mut trackers) = (vec![], vec![], vec![], vec![], vec![]);
        let mut toks = BTreeMap::new();
        let rent = |len: usize| self.env.svm.minimum_balance_for_rent_exemption(len);
        for (addr, owner, lamports, data) in scan_accounts(&self.env.svm) {
            if owner == core_vault::ID {
                if lamports < rent(data.len()) {
                    self.fail("10", format!("program-owned account {addr} holds {lamports} lamports, below rent exemption {} for {} bytes", rent(data.len()), data.len()));
                }
                if data.len() < 8 {
                    self.fail("15", format!("program-owned account {addr} has {} bytes (closed accounts must be system-owned)", data.len()));
                }
                let d = &data[..8];
                let mut s = data.as_slice();
                macro_rules! dec {
                    ($t:ty, $v:expr) => {
                        if d == <$t as Discriminator>::DISCRIMINATOR {
                            $v.push((addr, <$t>::try_deserialize(&mut s).unwrap_or_else(|e| self.fail("15", format!("{addr}: undecodable {}: {e:?}", stringify!($t))))));
                            continue;
                        }
                    };
                }
                if d == <VaultState as Discriminator>::DISCRIMINATOR {
                    vault = Some((addr, VaultState::try_deserialize(&mut s).unwrap()));
                    continue;
                }
                dec!(ProductRegistry, regs);
                dec!(TraderState, traders);
                dec!(PayoutClaim, claims);
                dec!(BondPosition, bonds);
                dec!(BondCapTracker, trackers);
                self.fail("15", format!("program-owned account {addr} has an unknown discriminator {d:?}"));
            } else if owner == spl_token::ID && data.len() == SplAccount::LEN {
                if lamports < rent(data.len()) {
                    self.fail("10", format!("token account {addr} holds {lamports} lamports, below rent exemption"));
                }
                let t = SplAccount::unpack_unchecked(&data).unwrap();
                if t.state != AccountState::Uninitialized {
                    toks.insert(addr, t);
                }
            }
        }
        let (vaddr, vault) = vault.unwrap_or_else(|| self.fail("15", "VaultState account missing".into()));
        if vaddr != self.env.vault {
            self.fail("15", "VaultState is not at its canonical address".into());
        }
        ChainView { vault, regs, traders, claims, bonds, trackers, toks }
    }

    fn check_all(&mut self) {
        let c = self.read_chain();
        let w = self.w.clone();
        let m = self.m.clone();
        let e = &self.env;

        // The checks that do NOT consult the model run first (1, 2, 5, 6, 11, 4); then the balance
        // and field comparisons with the model (3, 12, 8, 15); then the rent flows (14).
        // ---- 1: token conservation per mint (all token accounts on the chain)
        for ci in 0..2 {
            let mint = w.mint(ci);
            let sum: i128 = c.toks.values().filter(|t| t.mint == mint).map(|t| t.amount as i128).sum();
            if sum != m.supply[ci] {
                self.fail("1", format!("token conservation: the sum of all {} token accounts is {sum}, expected {}", if ci == 0 { "USDC" } else { "USDT" }, m.supply[ci]));
            }
        }
        // ---- 2: counters recomputed from the accounts that exist
        let vs = &c.vault;
        let claims_total: u128 = c.claims.iter().map(|(_, k)| k.owed as u128).sum();
        if vs.open_claims_count != c.claims.len() as u64 {
            self.fail("2", format!("open_claims_count {} != number of PayoutClaim accounts {}", vs.open_claims_count, c.claims.len()));
        }
        if vs.open_claims_total as u128 != claims_total {
            self.fail("2", format!("open_claims_total {} != sum(owed) {claims_total}", vs.open_claims_total));
        }
        let bonds_total: u128 = c.bonds.iter().map(|(_, b)| b.principal as u128).sum();
        if vs.bond_principal_open_total as u128 != bonds_total {
            self.fail("2", format!("bond_principal_open_total {} != sum of open positions' principal {bonds_total}", vs.bond_principal_open_total));
        }
        let mut own_by_dep: std::collections::HashMap<Pubkey, u128> = std::collections::HashMap::new();
        for (_, b) in &c.bonds {
            *own_by_dep.entry(b.depositor).or_default() += b.principal as u128;
        }
        for (a, t) in &c.trackers {
            let own: u128 = own_by_dep.get(&t.depositor).copied().unwrap_or(0);
            if t.open_principal_total as u128 != own {
                self.fail("2", format!("tracker {a}: open_principal_total {} != that wallet's open positions {own}", t.open_principal_total));
            }
            // ---- 6 caps
            if t.open_principal_total > BOND_WALLET_CAP {
                self.fail("6", format!("wallet {} has {} open principal, above the {BOND_WALLET_CAP} cap", t.depositor, t.open_principal_total));
            }
        }
        let tracker_deps: std::collections::HashSet<Pubkey> = c.trackers.iter().map(|(_, t)| t.depositor).collect();
        for (_, b) in &c.bonds {
            if !tracker_deps.contains(&b.depositor) {
                self.fail("2", format!("a position of {} has no tracker", b.depositor));
            }
        }
        if vs.bond_principal_open_total > BOND_GLOBAL_CAP {
            self.fail("6", format!("global open principal {} above the {BOND_GLOBAL_CAP} cap", vs.bond_principal_open_total));
        }
        // ---- 5: cycle machine, observed on the chain
        if vs.cycle_processed_count > vs.cycle_eligible_count {
            self.fail("5", format!("processed {} > eligible {}", vs.cycle_processed_count, vs.cycle_eligible_count));
        }
        if vs.cycle_id != self.last_cycle_id {
            if vs.cycle_id != self.last_cycle_id + 1 {
                self.fail("5", format!("cycle id jumped from {} to {}", self.last_cycle_id, vs.cycle_id));
            }
            if let Some((_, prev_start)) = self.cycle_starts.iter().next_back() {
                if vs.cycle_started_at < prev_start + GAP {
                    self.fail("5", format!("cycle {} started {}s after the previous start (< {GAP})", vs.cycle_id, vs.cycle_started_at - prev_start));
                }
            }
            self.cycle_starts.insert(vs.cycle_id, vs.cycle_started_at);
            self.last_cycle_id = vs.cycle_id;
            self.cycle_paid = 0;
        }
        if !vs.cycle_active && self.m.cyc.active {
            self.fail("5", "the cycle is closed on chain but open in the model".into());
        }
        // ---- 11: a cycle never pays out more than min(available, owed) at its snapshot
        if vs.cycle_active || self.cycle_paid > 0 {
            let cap = vs.cycle_available_snapshot.min(vs.cycle_owed_snapshot) as u128;
            if self.cycle_paid > cap {
                self.fail("11", format!("cycle {} paid {} in total, above min(available, owed) = {cap}", vs.cycle_id, self.cycle_paid));
            }
        }
        // ---- 4: claims
        let now_claims: BTreeMap<Pubkey, u64> = c.claims.iter().map(|(a, k)| (*a, k.owed)).collect();
        for (a, owed) in &now_claims {
            if self.closed_seen.contains(a) {
                self.fail("4", format!("closed claim {a} reappeared"));
            }
            if let Some(prev) = self.prev_claims.get(a) {
                if owed > prev {
                    self.fail("4", format!("claim {a}: owed grew from {prev} to {owed}"));
                }
            }
        }
        for a in self.prev_claims.keys() {
            if !now_claims.contains_key(a) {
                self.closed_seen.insert(*a);
            }
        }
        self.prev_claims = now_claims;
        // ---- 3: balances equal the model's
        let mut expected: BTreeMap<Pubkey, u64> = BTreeMap::new();
        for ci in 0..2 {
            let (_, pool, sl8) = e.coin(if ci == 0 { Coin::Usdc } else { Coin::Usdt });
            expected.insert(pool, m.pools[ci]);
            expected.insert(sl8, m.sl8[ci]);
            for p in 0..NP {
                expected.insert(e.wallet_ta(&w.people[p], if ci == 0 { Coin::Usdc } else { Coin::Usdt }), m.wallet[p][ci]);
                if matches!(m.ata_mode[p][ci], AtaMode::Usable | AtaMode::Frozen | AtaMode::Reowned) {
                    expected.insert(ata(&w.people[p], &w.mint(ci)), m.ata_bal[p][ci]);
                }
            }
        }
        for (addr, want) in &expected {
            match c.toks.get(addr) {
                Some(t) if t.amount == *want => {}
                Some(t) => self.fail("3", format!("balance of {addr} is {} but the model says {want}", t.amount)),
                None => self.fail("3", format!("token account {addr} is missing (model balance {want})")),
            }
        }
        for (addr, t) in &c.toks {
            if !expected.contains_key(addr) && t.amount != 0 {
                self.fail("3", format!("unexpected token account {addr} holds {} (mint {})", t.amount, t.mint));
            }
        }
        // ATA modes that must be unusable are NOT usable on chain (sanity of the test fixture itself)
        // ---- 12/8/15: chain state equals the model
        self.compare_model(&c);
        // ---- 14: rent lamports
        for (k, base) in &self.base_lam {
            let want = *base as i128 + self.exp_lam.get(k).copied().unwrap_or(0);
            let have = self.env.lamports(k) as i128;
            if have != want {
                self.fail("14", format!("lamports of {k}: chain {have}, expected {want} (rent went to / came from the wrong account; diff {})", have - want));
            }
        }
    }

    fn compare_model(&self, c: &ChainView) {
        let m = &self.m;
        let w = &self.w;
        let vs = &c.vault;
        let reg_ix = by_addr(&c.regs);
        let trader_ix = by_addr(&c.traders);
        let claim_ix = by_addr(&c.claims);
        let bond_ix = by_addr(&c.bonds);
        let tracker_ix = by_addr(&c.trackers);
        let mk = |what: &str, got: String, want: String| {
            if got != want {
                self.fail("12", format!("{what}: chain {got}, model {want}"));
            }
        };
        mk("cycle_id", vs.cycle_id.to_string(), m.cyc.id.to_string());
        mk("cycle_started_at", vs.cycle_started_at.to_string(), m.cyc.started.to_string());
        mk("cycle_active", vs.cycle_active.to_string(), m.cyc.active.to_string());
        mk("cycle_owed_snapshot", vs.cycle_owed_snapshot.to_string(), m.cyc.owed.to_string());
        mk("cycle_available_snapshot", vs.cycle_available_snapshot.to_string(), m.cyc.avail.to_string());
        mk("cycle_eligible_count", vs.cycle_eligible_count.to_string(), m.cyc.eligible.to_string());
        mk("cycle_processed_count", vs.cycle_processed_count.to_string(), m.cyc.processed.to_string());
        mk("usdc_floor", vs.usdc_floor.to_string(), m.floors[0].to_string());
        mk("usdt_floor", vs.usdt_floor.to_string(), m.floors[1].to_string());
        mk("floor_updated_at", vs.floor_updated_at.to_string(), m.floor_updated_at.to_string());
        mk("marketing_withdrawn_usdc", vs.marketing_withdrawn_usdc.to_string(), m.withdrawn[0].to_string());
        mk("marketing_withdrawn_usdt", vs.marketing_withdrawn_usdt.to_string(), m.withdrawn[1].to_string());
        mk("bond_withdrawal_fees_retained", vs.bond_withdrawal_fees_retained.to_string(), m.fees_retained.to_string());
        mk("open_claims_count", vs.open_claims_count.to_string(), m.claims.len().to_string());
        mk("open_claims_total", vs.open_claims_total.to_string(), m.claims_total().to_string());
        mk("bond_principal_open_total", vs.bond_principal_open_total.to_string(), m.bond_open_total().to_string());
        mk("sl8_wallet", vs.sl8_wallet.to_string(), self.env.sl8.pubkey().to_string());

        // registries
        for s in 0..NS {
            let sec = &w.sectors[s];
            let chain = reg_ix.get(&sec.registry()).map(|i| &c.regs[*i]);
            match (&m.prods[s], chain) {
                (None, None) => {}
                (None, Some(_)) => self.fail("12", format!("sector {s}: a registry exists on chain but the model has none")),
                (Some(_), None) => self.fail("12", format!("sector {s}: the model has a registry but none exists on chain")),
                (Some(p), Some((_, r))) => {
                    // ---- 8: registry totals
                    if r.total_requests_emitted != p.emitted || r.total_requested_amount != p.amount {
                        self.fail(
                            "8",
                            format!("sector {s}: registry totals chain ({}, {}) != model ({}, {})", r.total_requests_emitted, r.total_requested_amount, p.emitted, p.amount),
                        );
                    }
                    let tiers: Vec<(u64, u64)> = r.challenge_sizes.iter().map(|t| (t.size, t.cost)).collect();
                    mk(&format!("sector {s} tiers"), format!("{tiers:?}"), format!("{:?}", p.cfg.tiers));
                    mk(&format!("sector {s} fee_split_bps"), r.fee_split_bps.to_string(), p.cfg.fee_bps.to_string());
                    mk(&format!("sector {s} max_payout_count"), r.max_payout_count.to_string(), p.cfg.max_payout.to_string());
                    mk(&format!("sector {s} reset_price_bps"), format!("{:?}", r.reset_price_bps), format!("{:?}", p.cfg.reset_bps));
                    mk(&format!("sector {s} active"), r.active.to_string(), p.active.to_string());
                    mk(&format!("sector {s} pause_reason"), r.pause_reason.to_string(), p.pause_reason.to_string());
                    mk(&format!("sector {s} paused_since"), r.paused_since.to_string(), p.paused_since.to_string());
                    mk(&format!("sector {s} total_paused_secs"), r.total_paused_secs.to_string(), p.total_paused.to_string());
                    mk(&format!("sector {s} product_program_id"), r.product_program_id.to_string(), sec.id.to_string());
                    let (canon, bump) = Pubkey::find_program_address(&[b"product_registry", sec.id.as_ref()], &core_vault::ID);
                    if canon != sec.registry() || r.bump != bump {
                        self.fail("15", format!("sector {s}: registry bump {} is not canonical ({bump})", r.bump));
                    }
                }
            }
        }
        if c.regs.len() != m.prods.iter().filter(|p| p.is_some()).count() {
            self.fail("12", "number of registries on chain != the model's".into());
        }
        // trader states
        if c.traders.len() != m.traders.len() {
            self.fail("12", format!("{} TraderState accounts on chain, model has {}", c.traders.len(), m.traders.len()));
        }
        for ((s, p, cid), t) in &m.traders {
            let addr = w.sectors[*s].trader(&w.people[*p], *cid);
            let Some((_, ct)) = trader_ix.get(&addr).map(|i| &c.traders[*i]) else {
                self.fail("12", format!("trader ({s},{p},{cid}) missing on chain"));
            };
            let tag = format!("trader ({s},{p},{cid})");
            mk(&format!("{tag} status"), format!("{:?}", st_of(ct.status)), format!("{:?}", t.status));
            mk(&format!("{tag} payout_count"), ct.payout_count.to_string(), t.payout_count.to_string());
            mk(&format!("{tag} last_activity"), ct.last_activity_timestamp.to_string(), t.last.to_string());
            mk(&format!("{tag} paused_snapshot"), ct.paused_secs_snapshot.to_string(), t.snap.to_string());
            mk(&format!("{tag} reset_used"), ct.reset_used.to_string(), t.reset_used.to_string());
            mk(&format!("{tag} account_size"), ct.account_size.to_string(), t.size.to_string());
            mk(&format!("{tag} wallet"), ct.trader_wallet.to_string(), w.people[*p].to_string());
            mk(&format!("{tag} product"), ct.product_program_id.to_string(), w.sectors[*s].id.to_string());
            mk(&format!("{tag} challenge_id"), ct.challenge_id.to_string(), cid.to_string());
            let (_, bump) = w.sectors[*s].trader_bump(&w.people[*p], *cid);
            mk(&format!("{tag} bump"), ct.bump.to_string(), bump.to_string());
        }
        // claims
        if c.claims.len() != m.claims.len() {
            self.fail("12", format!("{} PayoutClaim accounts on chain, model has {}", c.claims.len(), m.claims.len()));
        }
        for (k, mc) in &m.claims {
            let addr = w.claim_addr(*k);
            let Some((_, cc)) = claim_ix.get(&addr).map(|i| &c.claims[*i]) else {
                self.fail("12", format!("claim {k:?} missing on chain at {addr}"));
            };
            let tag = format!("claim {k:?}");
            mk(&format!("{tag} owed"), cc.owed.to_string(), mc.owed.to_string());
            mk(&format!("{tag} created_in_cycle"), cc.created_in_cycle.to_string(), mc.created.to_string());
            mk(&format!("{tag} last_settled_cycle"), cc.last_settled_cycle.to_string(), mc.last.to_string());
            mk(&format!("{tag} trader_wallet"), cc.trader_wallet.to_string(), w.people[mc.person].to_string());
            match k {
                CKey::T { s, p, cid, req } => {
                    mk(&format!("{tag} kind"), cc.kind.to_string(), "0".into());
                    mk(&format!("{tag} request_id"), cc.request_id.to_string(), req.to_string());
                    mk(&format!("{tag} trader_state"), cc.trader_state.to_string(), w.sectors[*s].trader(&w.people[*p], *cid).to_string());
                    mk(&format!("{tag} product"), cc.product_program_id.to_string(), w.sectors[*s].id.to_string());
                    let (a, b) = claim_pda(&cc.trader_state, *req);
                    if a != addr || b != cc.bump {
                        self.fail("15", format!("{tag}: bump {} is not the canonical one ({b})", cc.bump));
                    }
                }
                CKey::B { p, idx } => {
                    mk(&format!("{tag} kind"), cc.kind.to_string(), "1".into());
                    mk(&format!("{tag} request_id"), cc.request_id.to_string(), idx.to_string());
                    mk(&format!("{tag} product"), cc.product_program_id.to_string(), Pubkey::default().to_string());
                    mk(&format!("{tag} trader_state"), cc.trader_state.to_string(), bond_pda(&w.people[*p], *idx).0.to_string());
                    let (a, b) = bond_claim_pda(&w.people[*p], *idx);
                    if a != addr || b != cc.bump {
                        self.fail("15", format!("{tag}: bump {} is not the canonical one ({b})", cc.bump));
                    }
                }
            }
        }
        // bonds and trackers
        if c.bonds.len() != m.bonds.len() {
            self.fail("12", format!("{} BondPosition accounts on chain, model has {}", c.bonds.len(), m.bonds.len()));
        }
        for ((p, idx), b) in &m.bonds {
            let (addr, bump) = bond_pda(&w.people[*p], *idx);
            let Some((_, cb)) = bond_ix.get(&addr).map(|i| &c.bonds[*i]) else {
                self.fail("12", format!("bond ({p},{idx}) missing on chain"));
            };
            let tag = format!("bond ({p},{idx})");
            mk(&format!("{tag} principal"), cb.principal.to_string(), b.principal.to_string());
            mk(&format!("{tag} term"), format!("{:?}", cb.term), format!("{:?}", b.term));
            mk(&format!("{tag} interest_bps"), cb.interest_bps.to_string(), term_secs(b.term).2.to_string());
            mk(&format!("{tag} created_at"), cb.created_at.to_string(), b.created_at.to_string());
            mk(&format!("{tag} mint"), cb.mint.to_string(), w.mint(b.c).to_string());
            mk(&format!("{tag} depositor"), cb.depositor.to_string(), w.people[*p].to_string());
            mk(&format!("{tag} deposit_index"), cb.deposit_index.to_string(), idx.to_string());
            mk(&format!("{tag} bump"), cb.bump.to_string(), bump.to_string());
        }
        if c.trackers.len() != m.trackers.len() {
            self.fail("12", format!("{} BondCapTracker accounts on chain, model has {}", c.trackers.len(), m.trackers.len()));
        }
        for (p, t) in &m.trackers {
            let (addr, bump) = bond_cap_pda(&w.people[*p]);
            let Some((_, ct)) = tracker_ix.get(&addr).map(|i| &c.trackers[*i]) else {
                self.fail("12", format!("tracker of person {p} missing on chain"));
            };
            mk(&format!("tracker {p} open"), ct.open_principal_total.to_string(), t.open.to_string());
            mk(&format!("tracker {p} next"), ct.next_deposit_index.to_string(), t.next.to_string());
            mk(&format!("tracker {p} bump"), ct.bump.to_string(), bump.to_string());
        }
    }
}

fn by_addr<T>(v: &[(Pubkey, T)]) -> std::collections::HashMap<Pubkey, usize> {
    v.iter().enumerate().map(|(i, (a, _))| (*a, i)).collect()
}

// ================================================================== runners

fn run_seed(seed: u64, steps: usize) -> Stats {
    let bond_heavy = seed % 3 == 0;
    let mut f = Fuzz::new(seed, bond_heavy);
    for _ in 0..steps {
        let a = f.gen();
        f.exec(a);
    }
    f.stats
}

fn run_many(seeds: &[u64], steps: usize) -> Stats {
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<(u64, String)>> = Mutex::new(vec![]);
    let total: Mutex<Stats> = Mutex::new(Stats::default());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= seeds.len() {
                    break;
                }
                let seed = seeds[i];
                match catch_unwind(AssertUnwindSafe(|| run_seed(seed, steps))) {
                    Ok(s) => total.lock().unwrap().merge(&s),
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "panic".into());
                        failures.lock().unwrap().push((seed, msg));
                    }
                }
            });
        }
    });
    let fails = failures.into_inner().unwrap();
    if !fails.is_empty() {
        let mut msg = format!("{} of {} seeds FAILED\n", fails.len(), seeds.len());
        for (s, m) in fails.iter().take(3) {
            msg.push_str(&format!("---- seed {s}\n{m}\n"));
        }
        panic!("{msg}");
    }
    total.into_inner().unwrap()
}

fn report(label: &str, st: &Stats) {
    println!("== {label}: per-instruction outcomes (ok / rejected as the model predicted) ==");
    let mut kinds: BTreeSet<&str> = st.ok.keys().cloned().collect();
    kinds.extend(st.err.keys().cloned());
    for k in kinds {
        println!("  {:<34} ok {:>6}   rejected {:>6}", k, st.ok.get(k).unwrap_or(&0), st.err.get(k).unwrap_or(&0));
    }
    println!("  -- coverage tags --");
    for (k, v) in &st.tags {
        println!("  {k:<48} {v}");
    }
    println!("  -- rejection codes (instruction:custom code) --");
    let line: Vec<String> = st.codes.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("  {}", line.join("  "));
    println!("  hostile variants rejected cleanly: {}, composite (valid+failing) txs rolled back: {}", st.hostile_rejected, st.composite_rejected);
}

const PROGRAM_IXS: [&str; 17] = [
    "register_product",
    "update_product_config",
    "pause_product",
    "reactivate_product",
    "admin_withdraw_marketing_funds",
    "deposit_fee",
    "deposit_reset",
    "record_activity",
    "flag_trader_failed",
    "mark_abandoned",
    "request_payout",
    "begin_heartbeat",
    "settle_claims",
    "finalize_heartbeat",
    "reconcile_product",
    "deposit_bond",
    "request_bond_payout",
];

/// States and branches the normal run must reach at least once (so a green run means
/// something): the generator is steered, not hoped, towards each of them.
const REQUIRED_TAGS: &[&str] = &[
    "settle:ratio_below_1",
    "settle:claim_closed",
    "settle:partial_payment",
    "settle:skipped_unusable_destination",
    "settle:zero_payment_stays_open",
    "settle:bond_claim_touched",
    "settle:both_pools_drained_in_one_batch",
    "payout:stale_abandoned",
    "payout:graduated",
    "payout:queued",
    "payout:filled_to_the_ceiling",
    "begin:frozen_pool_left_out",
    "settle:both_pools_frozen",
    "settle:with_a_frozen_pool",
    "settle:paid_from_the_other_pool_while_one_is_frozen",
    "record:Activity(Recorded)",
    "record:Activity(Throttled)",
    "record:Activity(Abandoned)",
    "bond:withdraw_matured",
    "bond:withdraw_early",
    "bond:withdraw_exactly_on_boundary",
    "withdraw:exact_max",
    "reconcile:paused_a_product",
    "reconcile:matched",
    "begin:with_claims",
    "begin:empty_cycle",
    "begin:exactly_at_gap",
    "finalize:ok",
    "warp:backwards",
    "created_over_dust:deposit_fee",
    "created_over_dust:request_payout",
    "created_over_dust:deposit_bond",
    "created_over_dust:register_product",
];

/// Every reachable error of every instruction must have been produced (and matched
/// by the model) at least once. 17 = spl-token AccountFrozen (a frozen pool), 0 = system "already in use", 3012 = Anchor
/// AccountNotInitialized, 600x = VaultError (6000 + variant index).
const REQUIRED_CODES: &[&str] = &[
    "register_product:6004", "register_product:6013", "register_product:6024", "register_product:0",
    "update_product_config:6024", "pause_product:6014", "reconcile_product:6014", "reconcile_product:6034",
    "deposit_fee:6001", "deposit_fee:6006", "deposit_fee:6023", "deposit_fee:0",
    "deposit_reset:6001", "deposit_reset:6007", "deposit_reset:6011", "deposit_reset:6012", "deposit_reset:0",
    "record_activity:6005", "flag_trader_failed:6005", "mark_abandoned:6005", "mark_abandoned:6010",
    "request_payout:6001", "request_payout:6005", "request_payout:6008", "request_payout:6009", "request_payout:6015",
    "request_payout:6043", "request_bond_payout:6043",
    "deposit_fee:17", "deposit_reset:17", "deposit_bond:17", "admin_withdraw_marketing_funds:17",
    "begin_heartbeat:6025", "begin_heartbeat:6027",
    "settle_claims:6026", "settle_claims:6029", "settle_claims:6030", "settle_claims:6031", "settle_claims:6032",
    "settle_claims:6033", "settle_claims:6021",
    "finalize_heartbeat:6026", "finalize_heartbeat:6028",
    "deposit_bond:6023", "deposit_bond:6036", "deposit_bond:6037", "deposit_bond:6038", "deposit_bond:6041",
    "request_bond_payout:6039", "request_bond_payout:6040",
    "admin_withdraw_marketing_funds:6015", "admin_withdraw_marketing_funds:6042",
];

/// The normal run: 30 fixed seeds x 400 steps. Every instruction must have succeeded
/// at least once somewhere in the run (init_vault runs in every seed's setup).
#[test]
fn fuzz_normal() {
    let st = run_many(&SEEDS, STEPS);
    report(&format!("{} seeds x {} steps", SEEDS.len(), STEPS), &st);
    for k in PROGRAM_IXS {
        assert!(st.ok.get(k).copied().unwrap_or(0) > 0, "coverage: {k} never succeeded in the normal run");
        assert!(st.err.get(k).copied().unwrap_or(0) > 0, "coverage: {k} was never rejected in the normal run");
    }
    for c in REQUIRED_CODES {
        assert!(st.codes.get(*c).copied().unwrap_or(0) > 0, "coverage: the rejection `{c}` was never produced in the normal run");
    }
    for t in REQUIRED_TAGS {
        assert!(st.tags.get(*t).copied().unwrap_or(0) > 0, "coverage: the generator never reached `{t}` in the normal run");
    }
}

/// 20 seeds x 5,000 steps. Run with:
/// `cargo test --manifest-path tests-rs/Cargo.toml --test invariants_fuzz -- --ignored fuzz_long --nocapture`
#[test]
#[ignore]
fn fuzz_long() {
    let st = run_many(&LONG_SEEDS, LONG_STEPS);
    report(&format!("{} seeds x {} steps (long run)", LONG_SEEDS.len(), LONG_STEPS), &st);
}

// ============================================== the tester tests itself
// Mutation testing (see docs/SECURITY-REVIEW.md) shows the tester catches wrong PROGRAM
// behaviour. These tests show that every invariant can really FIRE: the state is corrupted
// behind the model's back and the matching invariant must be the one that trips. Invariants
// 6, 7 and 13 are reached through program mutants only (6 is shadowed by 13: a program that
// breaks a cap also breaks the model's expectation first).

fn warmed(seed: u64, min_claims: usize, min_bonds: usize) -> Fuzz {
    let mut f = Fuzz::new(seed, true);
    let mut n = 0;
    while (f.m.claims.len() < min_claims || f.m.bonds.len() < min_bonds || f.m.pools[0] == 0) && n < 3_000 {
        let a = f.gen();
        f.exec(a);
        n += 1;
    }
    assert!(f.m.claims.len() >= min_claims && f.m.bonds.len() >= min_bonds, "could not warm up seed {seed}");
    f
}

fn trips(inv: &str, what: &str, f: &mut Fuzz, corrupt: impl FnOnce(&mut Fuzz)) {
    corrupt(f);
    let r = catch_unwind(AssertUnwindSafe(|| f.check_all()));
    let msg = match r {
        Ok(()) => panic!("self-test `{what}`: the corruption was NOT detected (expected INVARIANT {inv})"),
        Err(e) => e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default(),
    };
    assert!(
        msg.contains(&format!("INVARIANT {inv}\n")),
        "self-test `{what}`: expected INVARIANT {inv}, got:\n{}",
        msg.lines().take(4).collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn every_invariant_can_fire() {
    let sys = anchor_lang::solana_program::system_program::ID;
    let fresh = || warmed(3, 1, 1);

    trips("1", "tokens appear from nowhere", &mut fresh(), |f| {
        let pool = f.env.usdc_pool;
        let cur = f.env.token_balance(&pool);
        f.env.edit_token_account(&pool, |t| t.amount = cur + 1);
    });
    trips("2", "open_claims_count disagrees with the accounts", &mut fresh(), |f| f.env.set_vault_state(|v| v.open_claims_count += 1));
    trips("2", "a wallet's tracker disagrees with its positions", &mut fresh(), |f| {
        let (p, _) = *f.m.bonds.keys().next().unwrap();
        let addr = bond_cap_pda(&f.w.people[p]).0;
        let acct = f.env.svm.get_account(&addr).unwrap();
        let mut t = BondCapTracker::try_deserialize(&mut acct.data.as_slice()).unwrap();
        t.open_principal_total += 1;
        let mut data = vec![];
        anchor_lang::AccountSerialize::try_serialize(&t, &mut data).unwrap();
        data.resize(acct.data.len(), 0);
        f.env.svm.set_account(addr, solana_account::Account { data, ..acct }).unwrap();
    });
    trips("5", "processed > eligible", &mut fresh(), |f| f.env.set_vault_state(|v| v.cycle_processed_count = v.cycle_eligible_count + 1));
    trips("5", "the cycle id jumps", &mut fresh(), |f| f.env.set_vault_state(|v| v.cycle_id += 1));
    trips("11", "a cycle paid more than it could", &mut fresh(), |f| f.cycle_paid = u128::MAX / 2);
    trips("4", "a claim's owed grew", &mut fresh(), |f| {
        let k = *f.m.claims.keys().next().unwrap();
        let addr = f.w.claim_addr(k);
        f.prev_claims.insert(addr, 0);
    });
    trips("4", "a closed claim reappeared", &mut fresh(), |f| {
        let k = *f.m.claims.keys().next().unwrap();
        let addr = f.w.claim_addr(k);
        f.closed_seen.insert(addr);
    });
    trips("3", "tokens moved between accounts behind the model's back", &mut fresh(), |f| {
        let (pool, sl8) = (f.env.usdc_pool, f.env.sl8_usdc);
        let (p, s) = (f.env.token_balance(&pool), f.env.token_balance(&sl8));
        f.env.edit_token_account(&pool, |t| t.amount = p + 1);
        f.env.edit_token_account(&sl8, |t| t.amount = s.saturating_sub(1));
        if s == 0 {
            f.env.edit_token_account(&sl8, |t| t.amount = 0);
            f.env.edit_token_account(&pool, |t| t.amount = p.saturating_sub(1));
            let w0 = f.env.wallet_ta(&f.w.people[3], Coin::Usdc);
            let b = f.env.token_balance(&w0);
            f.env.edit_token_account(&w0, |t| t.amount = b + 1);
        }
    });
    trips("8", "registry totals disagree", &mut fresh(), |f| f.m.prods[0].as_mut().unwrap().emitted += 1);
    trips("12", "a model field disagrees with the chain", &mut fresh(), |f| f.m.floors[0] += 1);
    trips("10", "a program-owned account fell below rent exemption", &mut fresh(), |f| {
        let acct = f.env.svm.get_account(&f.env.vault).unwrap();
        f.env.svm.set_account(f.env.vault, solana_account::Account { lamports: 1, ..acct }).unwrap();
    });
    trips("15", "a program-owned account of an unknown type appeared", &mut fresh(), |f| {
        f.env.svm.set_account(Pubkey::new_unique(), solana_account::Account { lamports: 10_000_000, data: vec![7u8; 16], owner: core_vault::ID, executable: false, rent_epoch: 0 }).unwrap();
    });
    trips("15", "a registry sits at a non-canonical bump", &mut fresh(), |f| {
        let s0 = f.w.sectors[0].registry();
        let acct = f.env.svm.get_account(&s0).unwrap();
        let mut r = ProductRegistry::try_deserialize(&mut acct.data.as_slice()).unwrap();
        r.bump = r.bump.wrapping_sub(1);
        let mut data = vec![];
        anchor_lang::AccountSerialize::try_serialize(&r, &mut data).unwrap();
        data.resize(acct.data.len(), 0);
        f.env.svm.set_account(s0, solana_account::Account { data, ..acct }).unwrap();
    });
    trips("14", "rent lamports went to the wrong account", &mut fresh(), |f| {
        let k = f.keepers[0].pubkey();
        *f.exp_lam.entry(k).or_default() += 1;
    });
    trips("14", "the vault PDA gained lamports", &mut fresh(), |f| {
        let v = f.env.vault;
        let l = f.env.lamports(&v);
        let acct = f.env.svm.get_account(&v).unwrap();
        f.env.svm.set_account(v, solana_account::Account { lamports: l + 1, ..acct }).unwrap();
    });
    // invariant 9: a call that changed state must not look like one that did not
    let f = fresh();
    let before = state_fingerprint(&f.env.svm, &[f.env.payer.pubkey()]);
    let mut after = before.clone();
    *after.get_mut(&f.env.vault).unwrap() ^= 1;
    let r = catch_unwind(AssertUnwindSafe(|| f.assert_unchanged("a rejected instruction", "test", &before, &after)));
    let msg = r.err().and_then(|e| e.downcast_ref::<String>().cloned()).expect("invariant 9 must fire");
    assert!(msg.contains("INVARIANT 9\n"), "{msg}");
    let _ = sys;
}

/// Reproduce a single seed: `FUZZ_SEED=17 [FUZZ_STEPS=400] ... fuzz_one -- --nocapture`.
#[test]
fn fuzz_one() {
    let Some(seed) = std::env::var("FUZZ_SEED").ok().and_then(|s| s.parse::<u64>().ok()) else {
        println!("fuzz_one: set FUZZ_SEED=<n> to replay one seed");
        return;
    };
    let steps = std::env::var("FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(STEPS);
    let st = run_seed(seed, steps);
    report(&format!("seed {seed} x {steps} steps"), &st);
}
