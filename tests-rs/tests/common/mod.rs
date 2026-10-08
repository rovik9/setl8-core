//! Shared harness for the core-vault LiteSVM suite.
//!
//! Every test file does `mod common;`. The clock starts at a realistic Unix
//! time (the vault uses `paused_since > 0` as its "is paused" sentinel, so a
//! clock of 0 would silently break pause handling and is not a real-chain
//! state).
#![allow(dead_code, unused_imports)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anchor_lang::solana_program::{
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token::{
    self,
    solana_program::{program_option::COption, program_pack::Pack},
    state::{Account as SplAccount, AccountState, Mint as SplMint},
};
use solana_account::Account as RawAccount;
use core_vault::{
    errors::VaultError,
    state::{PayoutClaim, ProductRegistry, TraderState},
};
use std::cell::RefCell;
use litesvm::{
    types::{TransactionMetadata, TransactionResult},
    LiteSVM,
};
use setl8_shared_interfaces as si;
pub use si::{ActivityOutcome, ChallengeSize, PayoutOutcome};
use anchor_lang::solana_program::instruction::error::InstructionError;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub use core_vault::constants::{
    ACTIVITY_THROTTLE_SECS, INACTIVITY_LIMIT_SECS, MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, PAUSE_NONE,
    PAUSE_PLANNED_UPGRADE,
};
pub use core_vault::state::TraderStatus;

/// Token-2022 program id (present in LiteSVM's default programs, always rejected by the vault).
pub const TOKEN_2022_ID: Pubkey =
    anchor_lang::prelude::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const T0: i64 = 1_700_000_000;
pub const DAY: i64 = 86_400;

/// The program under test must embed the same admin keys this crate was compiled
/// with (the `localnet` test keys). A real-key `.so` (e.g. `target/deploy/`, or a
/// `CORE_VAULT_SO` pointed at the wrong build) would otherwise fail every admin
/// test with a confusing `MissingMultisigSignature`; fail loudly here instead.
fn assert_so_has_these_admin_keys(so: &std::path::Path) {
    use std::sync::OnceLock;
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        let bytes = std::fs::read(so).unwrap_or_else(|e| panic!("cannot read {} ({e})", so.display()));
        let has = |k: &Pubkey| bytes.windows(32).any(|w| w == k.as_ref());
        for (name, k) in [
            ("SL8_ADMIN_PUBKEY", core_vault::constants::SL8_ADMIN_PUBKEY),
            ("ROV_ADMIN_PUBKEY", core_vault::constants::ROV_ADMIN_PUBKEY),
        ] {
            assert!(
                has(&k),
                "{} does not contain {name} {k}: this is not the localnet (test-key) build -- \
                 build it with scripts/build-test-so.sh (never test against target/deploy/)",
                so.display()
            );
        }
    });
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

pub fn dup(k: &Keypair) -> Keypair {
    Keypair::try_from(k.to_bytes().as_slice()).unwrap()
}

fn load_keypair(name: &str) -> Keypair {
    let txt = std::fs::read_to_string(root().join("tests/fixtures").join(name)).unwrap();
    let bytes: Vec<u8> = txt
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    Keypair::try_from(bytes.as_slice()).unwrap()
}

// ---------------------------------------------------------------- fixtures

/// A fake sector program: only its id matters (its `sector_authority` PDA is
/// derived from it with the shared-interfaces helper).
#[derive(Clone)]
pub struct Sector {
    pub id: Pubkey,
    pub authority: Pubkey,
}

impl Sector {
    pub fn new() -> Self {
        let id = Pubkey::new_unique();
        Self { id, authority: si::derive_sector_authority(&id).0 }
    }
    pub fn registry(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"product_registry", self.id.as_ref()], &core_vault::ID).0
    }
    pub fn trader(&self, wallet: &Pubkey, challenge_id: u64) -> Pubkey {
        self.trader_bump(wallet, challenge_id).0
    }
    pub fn trader_bump(&self, wallet: &Pubkey, challenge_id: u64) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[b"trader_state", self.id.as_ref(), wallet.as_ref(), &challenge_id.to_le_bytes()],
            &core_vault::ID,
        )
    }
}

#[derive(Clone)]
pub struct Cfg {
    pub fee_split_bps: u16,
    pub tiers: Vec<ChallengeSize>,
    pub max_payout: u64,
    pub reset_bps: Vec<u16>,
}

pub const TIER_A: (u64, u64) = (10_000, 100);
pub const TIER_B: (u64, u64) = (50_000, 400);
/// Chosen so `size * bps / 10_000` does not divide evenly (floor behaviour).
pub const TIER_ODD: (u64, u64) = (12_345, 77);

impl Default for Cfg {
    fn default() -> Self {
        Self {
            fee_split_bps: 6500,
            tiers: [TIER_A, TIER_B, TIER_ODD]
                .iter()
                .map(|&(size, cost)| ChallengeSize { size, cost })
                .collect(),
            max_payout: 5,
            reset_bps: vec![100, 150, 450],
        }
    }
}

// ---------------------------------------------------------------------- env

pub struct Env {
    pub svm: LiteSVM,
    pub sl8: Keypair,
    pub rov: Keypair,
    pub payer: Keypair,
    /// Classic-SPL 6-decimal mints standing in for USDC / USDT.
    pub usdc: Pubkey,
    pub usdt: Pubkey,
    /// `VaultState` PDA and the two pool token accounts (PDAs) it owns.
    pub vault: Pubkey,
    pub usdc_pool: Pubkey,
    pub usdt_pool: Pubkey,
    /// SL8-side destination token accounts (owner = SL8_ADMIN_PUBKEY).
    pub sl8_usdc: Pubkey,
    pub sl8_usdt: Pubkey,
    /// Token accounts created for trader wallets (lazily, see `fund_wallet`).
    pub wallets: HashMap<Pubkey, WalletTok>,
    /// Every token account this env created or knows about, for balance snapshots.
    pub tracked: Vec<Pubkey>,
    /// Every PayoutClaim address a payout instruction was ever built for (the
    /// candidates for `open_claims`); filled by `payout_ix`.
    pub claim_addrs: RefCell<Vec<Pubkey>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coin {
    Usdc,
    Usdt,
}

/// A trader wallet's own token accounts (owner = the wallet).
#[derive(Clone, Copy, Debug)]
pub struct WalletTok {
    pub usdc: Pubkey,
    pub usdt: Pubkey,
}

/// Starting balance of each of a funded wallet's token accounts (1,000,000 coins).
pub const WALLET_START: u64 = 1_000_000_000_000;

pub fn vault_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"vault_state",
            core_vault::constants::SL8_ADMIN_PUBKEY.as_ref(),
            core_vault::constants::ROV_ADMIN_PUBKEY.as_ref(),
        ],
        &core_vault::ID,
    )
    .0
}

pub fn pool_pda(vault: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool", vault.as_ref(), mint.as_ref()], &core_vault::ID).0
}

impl Env {
    /// Sigverify OFF, vault initialised (`init_vault` run for real).
    /// PDA signers (sector_authority) can be flagged as signers without a
    /// signature. Real keys are still used where we have them.
    pub fn new() -> Self {
        let mut e = Self::build(false);
        e.init_vault();
        e
    }

    /// As `new()` but the vault has NOT been initialised (for init_vault tests).
    pub fn new_bare() -> Self {
        Self::build(false)
    }

    /// Sigverify ON, vault not initialised: signatures are really checked.
    pub fn new_sigverify_on() -> Self {
        Self::build(true)
    }

    fn build(sigverify: bool) -> Self {
        let mut svm = LiteSVM::new().with_sigverify(sigverify);
        // Default: the LOCALNET (public test keys) build, which lives apart from the
        // real-key build in target/deploy/. CORE_VAULT_SO overrides it (mutation
        // testing, or demonstrating the drift failure against another build).
        let so = std::env::var_os("CORE_VAULT_SO")
            .map(PathBuf::from)
            .unwrap_or_else(|| root().join("target/test-deploy/core_vault.so"));
        assert_so_has_these_admin_keys(&so);
        svm.add_program_from_file(core_vault::ID, &so).unwrap_or_else(|e| {
            panic!("cannot load {} ({e:?}) -- run scripts/build-test-so.sh first", so.display())
        });
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = T0;
        svm.set_sysvar(&clock);

        let sl8 = load_keypair("sl8-admin.json");
        let rov = load_keypair("rov-admin.json");
        assert_eq!(sl8.pubkey(), core_vault::constants::SL8_ADMIN_PUBKEY, "fixture/const drift");
        assert_eq!(rov.pubkey(), core_vault::constants::ROV_ADMIN_PUBKEY, "fixture/const drift");
        let payer = Keypair::new();
        let (usdc, usdt) = (Pubkey::new_unique(), Pubkey::new_unique());
        let vault = vault_pda();
        let mut env = Self {
            svm,
            sl8,
            rov,
            payer,
            usdc,
            usdt,
            vault,
            usdc_pool: pool_pda(&vault, &usdc),
            usdt_pool: pool_pda(&vault, &usdt),
            sl8_usdc: Pubkey::default(),
            sl8_usdt: Pubkey::default(),
            wallets: HashMap::new(),
            tracked: vec![],
            claim_addrs: RefCell::new(vec![]),
        };
        env.tracked.extend([env.usdc_pool, env.usdt_pool]);
        for k in [env.sl8.pubkey(), env.rov.pubkey(), env.payer.pubkey()] {
            env.fund(&k);
        }
        env.set_mint(&usdc, 6, spl_token::ID);
        env.set_mint(&usdt, 6, spl_token::ID);
        let sl8_pk = env.sl8.pubkey();
        env.sl8_usdc = env.new_token_account(&usdc, &sl8_pk, 0);
        env.sl8_usdt = env.new_token_account(&usdt, &sl8_pk, 0);
        env
    }

    /// Runs the real `init_vault` for this env's USDC/USDT mints.
    pub fn init_vault(&mut self) {
        let ix = init_vault_ix(self, self.usdc, self.usdt);
        self.ok(ix);
    }

    // ---- raw token state (set directly; the token program itself is real)
    pub fn set_raw(&mut self, addr: &Pubkey, data: Vec<u8>, owner: Pubkey) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(*addr, RawAccount { lamports, data, owner, executable: false, rent_epoch: 0 })
            .unwrap();
    }
    /// A mint with `decimals`, owned by `owner_program` (classic or Token-2022).
    pub fn set_mint(&mut self, addr: &Pubkey, decimals: u8, owner_program: Pubkey) {
        let mut data = vec![0u8; SplMint::LEN];
        SplMint::pack(
            SplMint {
                mint_authority: COption::None,
                supply: 0,
                decimals,
                is_initialized: true,
                freeze_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        self.set_raw(addr, data, owner_program);
    }
    pub fn set_token_account(&mut self, addr: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
        let mut data = vec![0u8; SplAccount::LEN];
        SplAccount::pack(
            SplAccount {
                mint: *mint,
                owner: *owner,
                amount,
                delegate: COption::None,
                state: AccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 0,
                close_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        self.set_raw(addr, data, spl_token::ID);
    }
    pub fn new_token_account(&mut self, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
        let addr = Pubkey::new_unique();
        self.set_token_account(&addr, mint, owner, amount);
        self.tracked.push(addr);
        addr
    }

    /// (mint, pool, sl8 destination) for a coin.
    pub fn coin(&self, c: Coin) -> (Pubkey, Pubkey, Pubkey) {
        match c {
            Coin::Usdc => (self.usdc, self.usdc_pool, self.sl8_usdc),
            Coin::Usdt => (self.usdt, self.usdt_pool, self.sl8_usdt),
        }
    }

    /// Creates `w`'s ASSOCIATED token account for `c` (the only destination
    /// settle_claims accepts) holding `amount`.
    pub fn make_ata(&mut self, w: &Pubkey, c: Coin, amount: u64) -> Pubkey {
        let mint = self.coin(c).0;
        let addr = ata(w, &mint);
        self.set_token_account(&addr, &mint, w, amount);
        if !self.tracked.contains(&addr) {
            self.tracked.push(addr);
        }
        addr
    }
    /// Both ATAs of `w`, empty.
    pub fn make_atas(&mut self, w: &Pubkey) -> WalletTok {
        WalletTok { usdc: self.make_ata(w, Coin::Usdc, 0), usdt: self.make_ata(w, Coin::Usdt, 0) }
    }

    /// Idempotent: gives `w` a USDC and a USDT token account holding
    /// `WALLET_START` each.
    pub fn fund_wallet(&mut self, w: &Pubkey) -> WalletTok {
        if let Some(t) = self.wallets.get(w) {
            return *t;
        }
        let (usdc, usdt) = (self.usdc, self.usdt);
        let t = WalletTok {
            usdc: self.new_token_account(&usdc, w, WALLET_START),
            usdt: self.new_token_account(&usdt, w, WALLET_START),
        };
        self.wallets.insert(*w, t);
        t
    }
    /// Like `fund_wallet` but with exact starting balances.
    pub fn fund_wallet_with(&mut self, w: &Pubkey, usdc_amount: u64, usdt_amount: u64) -> WalletTok {
        let t = self.fund_wallet(w);
        let (usdc, usdt) = (self.usdc, self.usdt);
        self.set_token_account(&t.usdc, &usdc, w, usdc_amount);
        self.set_token_account(&t.usdt, &usdt, w, usdt_amount);
        t
    }
    pub fn wallet_tok(&self, w: &Pubkey) -> WalletTok {
        *self.wallets.get(w).unwrap_or_else(|| panic!("wallet {w} has no token accounts: call fund_wallet first"))
    }
    pub fn wallet_ta(&self, w: &Pubkey, c: Coin) -> Pubkey {
        let t = self.wallet_tok(w);
        match c {
            Coin::Usdc => t.usdc,
            Coin::Usdt => t.usdt,
        }
    }

    /// Balance of every tracked token account that currently exists.
    pub fn token_snapshot(&self) -> BTreeMap<Pubkey, u64> {
        let mut m = BTreeMap::new();
        for a in self.tracked.iter().chain([&self.sl8_usdc, &self.sl8_usdt]) {
            if let Some(acct) = self.svm.get_account(a) {
                if acct.owner == spl_token::ID && acct.data.len() == SplAccount::LEN {
                    // an uninitialised token account (a test may plant one) holds nothing
                    if let Ok(t) = SplAccount::unpack(&acct.data) {
                        m.insert(*a, t.amount);
                    }
                }
            }
        }
        m
    }
    /// Sum of all tracked balances for `mint` (conservation checks).
    pub fn total_of(&self, mint: &Pubkey) -> u128 {
        self.token_snapshot()
            .keys()
            .filter(|a| self.token_state(a).mint == *mint)
            .map(|a| self.token_state(a).amount as u128)
            .sum()
    }
    /// Edit a token account's state in place (e.g. freeze it).
    pub fn edit_token_account(&mut self, addr: &Pubkey, f: impl FnOnce(&mut SplAccount)) {
        let mut st = self.token_state(addr);
        f(&mut st);
        let mut data = vec![0u8; SplAccount::LEN];
        SplAccount::pack(st, &mut data).unwrap();
        self.set_raw(addr, data, spl_token::ID);
    }
    pub fn token_state(&self, addr: &Pubkey) -> SplAccount {
        let a = self.svm.get_account(addr).expect("token account");
        SplAccount::unpack(&a.data).expect("valid token account")
    }
    pub fn token_balance(&self, addr: &Pubkey) -> u64 {
        self.token_state(addr).amount
    }

    pub fn fund(&mut self, who: &Pubkey) {
        self.svm.airdrop(who, 10_000_000_000).unwrap();
    }

    // ---- clock
    pub fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }
    pub fn set_time(&mut self, t: i64) {
        let mut c: Clock = self.svm.get_sysvar();
        c.unix_timestamp = t;
        self.svm.set_sysvar(&c);
    }
    pub fn advance(&mut self, secs: i64) {
        let t = self.now() + secs;
        self.set_time(t);
    }

    // ---- sending
    /// `fee_payer` always signs (gives every tx a unique signature); `signers`
    /// sign only if they are real required signers of the message. PDA signers
    /// are never listed here -- with sigverify off they just stay unsigned.
    pub fn send_with(&mut self, ixs: &[Instruction], fee_payer: &Keypair, signers: &[&Keypair]) -> TransactionResult {
        let bh = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&fee_payer.pubkey()), &bh);
        let mut tx = Transaction::new_unsigned(msg);
        let n = tx.message.header.num_required_signatures as usize;
        let required: Vec<Pubkey> = tx.message.account_keys[..n].to_vec();
        let mut sigs: Vec<&Keypair> = vec![fee_payer];
        sigs.extend(signers.iter().copied().filter(|k| required.contains(&k.pubkey())));
        tx.partial_sign(sigs.as_slice(), bh);
        let r = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        r
    }

    pub fn send(&mut self, ix: Instruction) -> TransactionResult {
        let p = dup(&self.payer);
        let (sl8, rov) = (dup(&self.sl8), dup(&self.rov));
        self.send_with(&[ix], &p, &[&sl8, &rov])
    }

    pub fn ok(&mut self, ix: Instruction) -> TransactionMetadata {
        let r = self.send(ix);
        assert_ok(r)
    }

    // ---- admin flows (assert success)
    pub fn register(&mut self, s: &Sector, c: &Cfg) {
        self.ok(register_ix(self, s, c));
    }
    pub fn registered(c: &Cfg) -> (Self, Sector) {
        let mut e = Self::new();
        let s = Sector::new();
        e.register(&s, c);
        (e, s)
    }
    pub fn update(&mut self, s: &Sector, c: &Cfg) {
        self.ok(update_ix(self, s, c));
    }
    pub fn pause(&mut self, s: &Sector) {
        self.ok(pause_ix(self, s));
    }
    pub fn resume(&mut self, s: &Sector) {
        self.ok(reactivate_ix(self, s));
    }

    // ---- sector flows (assert success)
    pub fn deposit(&mut self, s: &Sector, w: &Pubkey, id: u64) -> TransactionMetadata {
        self.deposit_tier(s, w, id, TIER_A)
    }
    pub fn deposit_tier(&mut self, s: &Sector, w: &Pubkey, id: u64, tier: (u64, u64)) -> TransactionMetadata {
        self.deposit_coin(s, w, id, tier, Coin::Usdc)
    }
    pub fn deposit_coin(&mut self, s: &Sector, w: &Pubkey, id: u64, (size, cost): (u64, u64), c: Coin) -> TransactionMetadata {
        self.fund_wallet(w);
        let ix = deposit_fee_ix_coin(self, s, w, id, cost, size, c);
        self.ok(ix)
    }
    pub fn record(&mut self, s: &Sector, w: &Pubkey, id: u64) -> TransactionMetadata {
        self.ok(record_ix(s, w, id))
    }
    pub fn flag(&mut self, s: &Sector, w: &Pubkey, id: u64) {
        self.ok(flag_ix(s, w, id));
    }
    /// Successful `request_payout` (queues a claim; no tokens move).
    pub fn payout(&mut self, s: &Sector, w: &Pubkey, id: u64, amount: u64, req: u64) -> TransactionMetadata {
        let ix = payout_ix(self, s, w, id, amount, req);
        self.ok(ix)
    }
    /// Sets a pool's token balance directly (edits the token account).
    pub fn set_pool(&mut self, c: Coin, amount: u64) {
        let pool = self.coin(c).1;
        self.edit_token_account(&pool, |a| a.amount = amount);
    }
    /// Permissionless; `caller` pays the fee and signs.
    pub fn abandon_as(&mut self, caller: &Keypair, s: &Sector, w: &Pubkey, id: u64) -> TransactionResult {
        let ix = abandon_ix(&caller.pubkey(), s, w, id);
        self.send_with(&[ix], caller, &[caller])
    }
    pub fn abandon(&mut self, s: &Sector, w: &Pubkey, id: u64) -> TransactionResult {
        let ix = abandon_ix(&self.payer.pubkey(), s, w, id);
        self.send(ix)
    }

    /// (USDC pool, USDT pool) balances.
    pub fn pools(&self) -> (u64, u64) {
        (self.token_balance(&self.usdc_pool), self.token_balance(&self.usdt_pool))
    }
    /// Total of every tracked token account per mint: (USDC, USDT). Conserved by settlement.
    pub fn totals(&self) -> (u128, u128) {
        (self.total_of(&self.usdc.clone()), self.total_of(&self.usdt.clone()))
    }
    pub fn lamports(&self, a: &Pubkey) -> u64 {
        self.svm.get_balance(a).unwrap_or(0)
    }
    /// Everything a rejected heartbeat instruction must leave untouched: all
    /// tracked token balances, the VaultState bytes and every known claim's bytes.
    pub fn digest(&self) -> (BTreeMap<Pubkey, u64>, Vec<u8>, Vec<(Pubkey, Vec<u8>)>) {
        let claims = self
            .claim_addrs
            .borrow()
            .iter()
            .map(|a| (*a, self.svm.get_account(a).map(|x| x.data).unwrap_or_default()))
            .collect();
        (self.token_snapshot(), self.svm.get_account(&self.vault).unwrap().data, claims)
    }

    // ---- heartbeat (assert success)
    /// A funded caller distinct from the fee payer (so its own balance moves only
    /// by what the instruction does to it).
    pub fn new_caller(&mut self) -> Keypair {
        let k = Keypair::new();
        self.fund(&k.pubkey());
        k
    }
    pub fn begin(&mut self) -> TransactionMetadata {
        let ix = begin_ix(&self.payer.pubkey(), self);
        self.ok(ix)
    }
    pub fn begin_result(&mut self) -> TransactionResult {
        let ix = begin_ix(&self.payer.pubkey(), self);
        self.send(ix)
    }
    pub fn settle(&mut self, triples: &[Triple]) -> TransactionMetadata {
        let ix = settle_ix(&self.payer.pubkey(), self, triples);
        self.ok(ix)
    }
    pub fn settle_result(&mut self, triples: &[Triple]) -> TransactionResult {
        let ix = settle_ix(&self.payer.pubkey(), self, triples);
        self.send(ix)
    }
    pub fn finalize(&mut self) -> TransactionMetadata {
        let ix = finalize_ix(&self.payer.pubkey(), self);
        self.ok(ix)
    }
    pub fn finalize_result(&mut self) -> TransactionResult {
        let ix = finalize_ix(&self.payer.pubkey(), self);
        self.send(ix)
    }
    /// A request_payout for a fresh, funded trader (challenge 1, request 1) with
    /// the given claim size. Returns (wallet, claim address).
    pub fn queue_claim(&mut self, s: &Sector, owed: u64) -> (Pubkey, Pubkey) {
        let w = wallet();
        self.deposit(s, &w, 1);
        self.payout(s, &w, 1, owed, 1);
        (w, claim_key(s, &w, 1, 1))
    }

    // ---- decoding
    pub fn vault_state(&self) -> core_vault::state::VaultState {
        let a = self.svm.get_account(&self.vault).expect("vault_state account");
        assert_eq!(a.owner, core_vault::ID);
        core_vault::state::VaultState::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    /// Rewrites the registry account's data directly (bypassing the program's
    /// own validation), to build states the instructions would refuse to create.
    pub fn set_registry(&mut self, s: &Sector, f: impl FnOnce(&mut ProductRegistry)) {
        let mut reg = self.registry(s);
        f(&mut reg);
        let old = self.svm.get_account(&s.registry()).unwrap();
        let mut data = Vec::with_capacity(old.data.len());
        anchor_lang::AccountSerialize::try_serialize(&reg, &mut data).unwrap();
        assert!(data.len() <= old.data.len());
        data.resize(old.data.len(), 0);
        self.svm
            .set_account(s.registry(), RawAccount { data, ..old })
            .unwrap();
    }
    /// Rewrites the VaultState account's data directly (to build states, such as
    /// a mid-cycle vault, that need instructions this suite does not call).
    pub fn set_vault_state(&mut self, f: impl FnOnce(&mut core_vault::state::VaultState)) {
        let mut vs = self.vault_state();
        f(&mut vs);
        let old = self.svm.get_account(&self.vault).unwrap();
        let mut data = Vec::with_capacity(old.data.len());
        anchor_lang::AccountSerialize::try_serialize(&vs, &mut data).unwrap();
        assert_eq!(data.len(), old.data.len());
        self.svm.set_account(self.vault, RawAccount { data, ..old }).unwrap();
    }
    pub fn registry(&self, s: &Sector) -> ProductRegistry {
        let a = self.svm.get_account(&s.registry()).expect("registry account");
        assert_eq!(a.owner, core_vault::ID);
        ProductRegistry::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    /// The claim at `addr`, if a live PayoutClaim account exists there.
    pub fn claim_at(&self, addr: &Pubkey) -> Option<PayoutClaim> {
        let a = self.svm.get_account(addr)?;
        if a.data.is_empty() || a.owner != core_vault::ID {
            return None;
        }
        Some(PayoutClaim::try_deserialize(&mut a.data.as_slice()).unwrap())
    }
    pub fn claim(&self, s: &Sector, w: &Pubkey, id: u64, req: u64) -> PayoutClaim {
        self.claim_at(&claim_key(s, w, id, req)).expect("payout claim account")
    }
    pub fn claim_opt(&self, s: &Sector, w: &Pubkey, id: u64, req: u64) -> Option<PayoutClaim> {
        self.claim_at(&claim_key(s, w, id, req))
    }
    /// Every open claim among the addresses payout instructions were built for.
    pub fn open_claims(&self) -> Vec<(Pubkey, PayoutClaim)> {
        self.claim_addrs
            .borrow()
            .iter()
            .filter_map(|a| self.claim_at(a).map(|c| (*a, c)))
            .collect()
    }
    /// CLAIM INVARIANT: the vault's counters match the claim accounts that exist.
    pub fn assert_claim_invariant(&self) {
        let claims = self.open_claims();
        let vs = self.vault_state();
        let total: u64 = claims.iter().map(|(_, c)| c.owed).sum();
        assert_eq!(vs.open_claims_total, total, "open_claims_total != sum of owed");
        assert_eq!(vs.open_claims_count, claims.len() as u64, "open_claims_count != number of claims");
        for (_, c) in &claims {
            assert!(c.owed > 0, "a claim with owed == 0 must have been closed");
        }
    }
    pub fn trader(&self, s: &Sector, w: &Pubkey, id: u64) -> TraderState {
        self.trader_opt(s, w, id).expect("trader_state account")
    }
    pub fn trader_opt(&self, s: &Sector, w: &Pubkey, id: u64) -> Option<TraderState> {
        let a = self.svm.get_account(&s.trader(w, id))?;
        if a.data.is_empty() {
            return None;
        }
        assert_eq!(a.owner, core_vault::ID);
        Some(TraderState::try_deserialize(&mut a.data.as_slice()).unwrap())
    }
}

// ------------------------------------------------------- instruction builders
// All go through the real shared-interfaces builders; account order for the
// vault-side accounts the builders don't know about follows the vault's
// `#[derive(Accounts)]` structs.

fn sys() -> AccountMeta {
    AccountMeta::new_readonly(system_program::ID, false)
}

/// Account metas in the program's declared order:
/// 0 sl8_admin, 1 rov_admin, 2 vault_state, 3 usdc_mint, 4 usdt_mint,
/// 5 usdc_pool, 6 usdt_pool, 7 token_program, 8 system_program.
/// Built from the program crate's own generated client types.
pub fn init_vault_ix(e: &Env, usdc_mint: Pubkey, usdt_mint: Pubkey) -> Instruction {
    let vault = vault_pda();
    let accounts = core_vault::accounts::InitVault {
        sl8_admin: e.sl8.pubkey(),
        rov_admin: e.rov.pubkey(),
        vault_state: vault,
        usdc_mint,
        usdt_mint,
        usdc_pool: pool_pda(&vault, &usdc_mint),
        usdt_pool: pool_pda(&vault, &usdt_mint),
        token_program: spl_token::ID,
        system_program: system_program::ID,
    };
    Instruction {
        program_id: core_vault::ID,
        accounts: accounts.to_account_metas(None),
        data: core_vault::instruction::InitVault { usdc_mint, usdt_mint }.data(),
    }
}

pub fn register_ix(e: &Env, s: &Sector, c: &Cfg) -> Instruction {
    si::register_product(
        core_vault::ID,
        e.sl8.pubkey(),
        e.rov.pubkey(),
        s.registry(),
        &[sys()],
        si::RegisterProductArgs {
            product_program_id: s.id,
            fee_split_bps: c.fee_split_bps,
            challenge_sizes: c.tiers.clone(),
            max_payout_count: c.max_payout,
            reset_price_bps: c.reset_bps.clone(),
        },
    )
}

pub fn update_ix(e: &Env, s: &Sector, c: &Cfg) -> Instruction {
    si::update_product_config(
        core_vault::ID,
        e.sl8.pubkey(),
        e.rov.pubkey(),
        s.registry(),
        &[],
        si::UpdateProductConfigArgs {
            product_program_id: s.id,
            challenge_sizes: c.tiers.clone(),
            fee_split_bps: c.fee_split_bps,
            max_payout_count: c.max_payout,
            reset_price_bps: c.reset_bps.clone(),
        },
    )
}

pub fn pause_ix(e: &Env, s: &Sector) -> Instruction {
    si::pause_product(
        core_vault::ID,
        e.sl8.pubkey(),
        e.rov.pubkey(),
        s.registry(),
        &[],
        si::PauseProductArgs { product_program_id: s.id },
    )
}

pub fn reactivate_ix(e: &Env, s: &Sector) -> Instruction {
    si::reactivate_product(
        core_vault::ID,
        e.sl8.pubkey(),
        e.rov.pubkey(),
        s.registry(),
        &[],
        si::ReactivateProductArgs { product_program_id: s.id },
    )
}

/// Where the appended token-movement accounts sit in each instruction's
/// account list (for tests that corrupt one of them).
#[derive(Clone, Copy)]
pub struct Slots {
    pub vault: usize,
    pub trader: usize,
    pub trader_ta: usize,
    pub mint: usize,
    pub pool: usize,
    pub sl8_ta: usize,
    pub token_program: usize,
}
/// deposit_fee: 0 auth, 1 registry, 2 trader_state, 3 payer, 4 system, then these.
pub const DF: Slots = Slots { vault: 5, trader: 6, trader_ta: 7, mint: 8, pool: 9, sl8_ta: 10, token_program: 11 };
/// deposit_reset: 0 auth, 1 registry, 2 prev, 3 new, 4 payer, 5 system, then these.
pub const DR: Slots = Slots { vault: 6, trader: 7, trader_ta: 8, mint: 9, pool: 10, sl8_ta: 11, token_program: 12 };

/// The seven appended accounts, in order: vault_state, trader (signer),
/// trader_token_account, mint, pool_token_account, sl8_token_account,
/// token_program.
fn token_metas(e: &Env, w: &Pubkey, c: Coin) -> [AccountMeta; 7] {
    let (mint, pool, sl8) = e.coin(c);
    [
        AccountMeta::new_readonly(e.vault, false),
        AccountMeta::new_readonly(*w, true),
        AccountMeta::new(e.wallet_ta(w, c), false),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new(pool, false),
        AccountMeta::new(sl8, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ]
}

/// USDC deposit_fee. The wallet must already have token accounts (`fund_wallet`).
pub fn deposit_fee_ix(e: &Env, s: &Sector, w: &Pubkey, id: u64, amount: u64, size: u64) -> Instruction {
    deposit_fee_ix_coin(e, s, w, id, amount, size, Coin::Usdc)
}

pub fn deposit_fee_ix_coin(e: &Env, s: &Sector, w: &Pubkey, id: u64, amount: u64, size: u64, c: Coin) -> Instruction {
    let mut remaining = vec![
        AccountMeta::new(s.trader(w, id), false),
        AccountMeta::new(e.payer.pubkey(), true),
        sys(),
    ];
    remaining.extend(token_metas(e, w, c));
    si::deposit_fee(
        core_vault::ID,
        s.authority,
        s.registry(),
        &remaining,
        si::DepositFeeArgs {
            amount,
            product_program_id: s.id,
            challenge_id: id,
            trader_wallet: *w,
            account_size: size,
        },
    )
}

/// USDC deposit_reset. The wallet must already have token accounts.
pub fn reset_ix(e: &Env, s: &Sector, w: &Pubkey, prev: u64, new: u64, amount: u64, phase: u8) -> Instruction {
    reset_ix_coin(e, s, w, prev, new, amount, phase, Coin::Usdc)
}

#[allow(clippy::too_many_arguments)]
pub fn reset_ix_coin(
    e: &Env,
    s: &Sector,
    w: &Pubkey,
    prev: u64,
    new: u64,
    amount: u64,
    phase: u8,
    c: Coin,
) -> Instruction {
    let mut remaining = vec![
        AccountMeta::new(s.trader(w, prev), false),
        AccountMeta::new(s.trader(w, new), false),
        AccountMeta::new(e.payer.pubkey(), true),
        sys(),
    ];
    remaining.extend(token_metas(e, w, c));
    si::deposit_reset(
        core_vault::ID,
        s.authority,
        s.registry(),
        &remaining,
        si::DepositResetArgs {
            amount,
            trader_wallet: *w,
            product_program_id: s.id,
            prev_challenge_id: prev,
            new_challenge_id: new,
            reset_phase: phase,
        },
    )
}

pub fn record_ix(s: &Sector, w: &Pubkey, id: u64) -> Instruction {
    si::record_activity(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[AccountMeta::new(s.trader(w, id), false)],
        si::RecordActivityArgs { trader_wallet: *w, product_program_id: s.id, challenge_id: id },
    )
}

pub fn abandon_ix(caller: &Pubkey, s: &Sector, w: &Pubkey, id: u64) -> Instruction {
    si::mark_abandoned(
        core_vault::ID,
        *caller,
        s.registry(),
        &[AccountMeta::new(s.trader(w, id), false)],
        si::MarkAbandonedArgs { trader_wallet: *w, product_program_id: s.id, challenge_id: id },
    )
}

pub fn flag_ix(s: &Sector, w: &Pubkey, id: u64) -> Instruction {
    si::flag_trader_failed(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[AccountMeta::new(s.trader(w, id), false)],
        si::FlagTraderFailedArgs { trader_wallet: *w, product_program_id: s.id, challenge_id: id },
    )
}

/// request_payout account positions: 0 auth, 1 registry, 2 trader_state, then
/// the appended payout-queue accounts.
#[derive(Clone, Copy)]
pub struct PayoutSlots {
    pub trader_state: usize,
    pub vault: usize,
    pub claim: usize,
    pub payer: usize,
    pub system: usize,
}
pub const PO: PayoutSlots = PayoutSlots { trader_state: 2, vault: 3, claim: 4, payer: 5, system: 6 };

pub fn claim_pda(trader_state: &Pubkey, req: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"payout_claim", trader_state.as_ref(), &req.to_le_bytes()], &core_vault::ID)
}
pub fn claim_key(s: &Sector, w: &Pubkey, id: u64, req: u64) -> Pubkey {
    claim_pda(&s.trader(w, id), req).0
}

/// request_payout. `e.payer` pays the claim's rent. No token accounts: nothing
/// moves until a heartbeat cycle settles the claim.
pub fn payout_ix(e: &Env, s: &Sector, w: &Pubkey, id: u64, amount: u64, req: u64) -> Instruction {
    let claim = claim_key(s, w, id, req);
    {
        let mut v = e.claim_addrs.borrow_mut();
        if !v.contains(&claim) {
            v.push(claim);
        }
    }
    si::request_payout(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[
            AccountMeta::new(s.trader(w, id), false),
            AccountMeta::new(e.vault, false),
            AccountMeta::new(claim, false),
            AccountMeta::new(e.payer.pubkey(), true),
            sys(),
        ],
        si::RequestPayoutArgs {
            trader_wallet: *w,
            amount,
            product_program_id: s.id,
            challenge_id: id,
            proposed_request_id: req,
        },
    )
}

// ------------------------------------------------------------- heartbeat builders

/// The associated token account of `wallet` for `mint` (classic Token program).
pub fn ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &core_vault::constants::ATA_PROGRAM_ID,
    )
    .0
}

/// One settle_claims batch entry: (claim, trader USDC account, trader USDT account).
pub type Triple = (Pubkey, Pubkey, Pubkey);

/// The correct triple for a wallet's claim (its associated token accounts).
pub fn triple(e: &Env, s: &Sector, w: &Pubkey, id: u64, req: u64) -> Triple {
    (claim_key(s, w, id, req), ata(w, &e.usdc), ata(w, &e.usdt))
}

pub fn begin_ix(caller: &Pubkey, e: &Env) -> Instruction {
    Instruction {
        program_id: core_vault::ID,
        accounts: core_vault::accounts::BeginHeartbeat {
            caller: *caller,
            vault_state: e.vault,
            usdc_pool: e.usdc_pool,
            usdt_pool: e.usdt_pool,
        }
        .to_account_metas(None),
        data: core_vault::instruction::BeginHeartbeat {}.data(),
    }
}

pub fn finalize_ix(caller: &Pubkey, e: &Env) -> Instruction {
    Instruction {
        program_id: core_vault::ID,
        accounts: core_vault::accounts::FinalizeHeartbeat {
            caller: *caller,
            vault_state: e.vault,
            usdc_pool: e.usdc_pool,
            usdt_pool: e.usdt_pool,
        }
        .to_account_metas(None),
        data: core_vault::instruction::FinalizeHeartbeat {}.data(),
    }
}

/// Account positions in settle_claims: 0 caller, 1 vault, 2 usdc_mint, 3 usdt_mint,
/// 4 usdc_pool, 5 usdt_pool, 6 token_program, then the triples from 7.
pub const ST: usize = 7;

pub fn settle_ix(caller: &Pubkey, e: &Env, triples: &[Triple]) -> Instruction {
    let mut accounts = core_vault::accounts::SettleClaims {
        caller: *caller,
        vault_state: e.vault,
        usdc_mint: e.usdc,
        usdt_mint: e.usdt,
        usdc_pool: e.usdc_pool,
        usdt_pool: e.usdt_pool,
        token_program: spl_token::ID,
    }
    .to_account_metas(None);
    for (claim, usdc_ata, usdt_ata) in triples {
        accounts.push(AccountMeta::new(*claim, false));
        accounts.push(AccountMeta::new(*usdc_ata, false));
        accounts.push(AccountMeta::new(*usdt_ata, false));
    }
    Instruction { program_id: core_vault::ID, accounts, data: core_vault::instruction::SettleClaims {}.data() }
}

/// ComputeBudget `SetComputeUnitLimit`.
pub fn compute_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data,
    }
}

// ------------------------------------------------------------ error helpers

/// The (instruction index, InstructionError) of a failed tx; panics with logs
/// if the tx succeeded or failed at a non-instruction level.
pub fn instruction_error(r: &TransactionResult) -> (u8, InstructionError) {
    match r {
        Ok(_) => panic!("expected failure, but the transaction succeeded"),
        Err(f) => match &f.err {
            TransactionError::InstructionError(i, e) => (*i, e.clone()),
            other => panic!("expected an InstructionError, got {other:?}\nlogs:\n{}", f.meta.logs.join("\n")),
        },
    }
}

fn logs(r: &TransactionResult) -> String {
    match r {
        Ok(m) => m.logs.join("\n"),
        Err(f) => f.meta.logs.join("\n"),
    }
}

/// Exact custom error code, on instruction 0.
pub fn assert_custom_code(r: &TransactionResult, want: u32, what: &str) {
    let (idx, e) = instruction_error(r);
    assert_eq!(idx, 0, "{what}: failed on unexpected instruction index\nlogs:\n{}", logs(r));
    match e {
        InstructionError::Custom(code) => {
            assert_eq!(code, want, "{what}: wrong error code (got {code}, want {want})\nlogs:\n{}", logs(r))
        }
        other => panic!("{what}: expected Custom({want}), got {other:?}\nlogs:\n{}", logs(r)),
    }
}

/// `VaultError::X` => 6000 + variant index, taken from the enum itself.
pub fn assert_vault_err(r: &TransactionResult, want: VaultError) {
    let name = format!("{want:?}");
    assert_custom_code(r, u32::from(want), &format!("VaultError::{name}"));
}

/// An Anchor framework error (e.g. `AccountNotSigner`).
pub fn assert_anchor_err(r: &TransactionResult, want: anchor_lang::error::ErrorCode) {
    let name = format!("{want:?}");
    assert_custom_code(r, want as u32, &format!("anchor ErrorCode::{name}"));
}

/// `init` on an existing account: system program `AccountAlreadyInUse` (0).
pub fn assert_already_in_use(r: &TransactionResult) {
    assert_custom_code(r, 0, "system AccountAlreadyInUse");
    assert!(
        logs(r).contains("already in use"),
        "expected an 'already in use' log\nlogs:\n{}",
        logs(r)
    );
}

pub fn assert_ok(r: TransactionResult) -> TransactionMetadata {
    match r {
        Ok(m) => m,
        Err(f) => panic!("expected success, got {:?}\nlogs:\n{}", f.err, f.meta.logs.join("\n")),
    }
}

/// Return data must come from the vault and equal `expected`.
pub fn assert_return(m: &TransactionMetadata, expected: &[u8]) {
    assert_eq!(m.return_data.program_id, core_vault::ID, "return data not set by the vault");
    assert_eq!(m.return_data.data, expected);
}
pub fn assert_activity(m: &TransactionMetadata, o: ActivityOutcome) {
    assert_return(m, &[o as u8]);
}
pub fn assert_payout_outcome(m: &TransactionMetadata, o: PayoutOutcome) {
    assert_return(m, &[o as u8]);
}

/// A fresh wallet.
pub fn wallet() -> Pubkey {
    Pubkey::new_unique()
}

/// No token account moved and no TraderState-like account appeared/changed.
pub fn assert_snapshot_unchanged(before: &BTreeMap<Pubkey, u64>, after: &BTreeMap<Pubkey, u64>) {
    assert_eq!(before, after, "a rejected payment must not move any token balance");
}

/// Serialized size of a legacy transaction with these instructions, one fee-payer
/// signature, and no lookup tables: `1 + 64 * signatures + message`.
pub fn legacy_tx_size(ixs: &[Instruction], fee_payer: &Pubkey) -> usize {
    let msg = Message::new(ixs, Some(fee_payer));
    let n = msg.header.num_required_signatures as usize;
    1 + 64 * n + msg.serialize().len()
}
