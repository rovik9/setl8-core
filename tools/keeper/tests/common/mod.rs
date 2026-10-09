//! LiteSVM harness for the keeper: a `Chain` over LiteSVM (signature verification ON), the real
//! core-vault program (localnet-keys build), the mock sector program (tools/devnet-sector) to create real
//! claims and a real payout tally, and helpers to build worlds the keeper has to deal with.
//! Only generated throwaway keys and the PUBLIC localnet test admin keys are used.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_program;
use anchor_lang::{AccountDeserialize, AccountSerialize};
use anchor_spl::token::spl_token::{
    self,
    solana_program::{program_option::COption, program_pack::Pack},
    state::{Account as SplAccount, AccountState, Mint as SplMint},
};
use litesvm::LiteSVM;
use setl8_admin::admin_ix::{AdminIx, Keys, Product, Tier};
use setl8_admin::cluster::Cluster;
use setl8_keeper::chain::{CResult, Chain, ChainError, RawAccount, Sim, TxErr, TxStatus};
use setl8_keeper::config::Config;
use setl8_keeper::log::Logger;
use setl8_keeper::runner::{Keeper, NoSleep};
use solana_account::Account as RawAcct;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const T0: i64 = 1_700_000_000;
pub const M: u64 = 1_000_000;
pub const DAY: i64 = 86_400;
pub const GAP: i64 = 432_000;
pub const LOCAL_GENESIS: &str = "LocalGenesisHash1111111111111111111111111111";

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn dup(k: &Keypair) -> Keypair {
    Keypair::try_from(k.to_bytes().as_slice()).unwrap()
}

fn fixture(name: &str) -> Keypair {
    let txt = std::fs::read_to_string(root().join("tests/fixtures").join(name)).unwrap();
    let bytes: Vec<u8> = serde_json::from_str(&txt).unwrap();
    Keypair::try_from(bytes.as_slice()).unwrap()
}

pub fn ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    setl8_keeper::plan::associated_token_address(wallet, mint)
}

// ------------------------------------------------------------------ the chain

/// A one-shot action run just before a send is executed.
pub type Hook = Box<dyn FnMut()>;

/// A `Chain` over a shared LiteSVM. Clones share the SVM and all counters/hooks, so several keepers
/// (or a keeper and a test) act on one chain.
#[derive(Clone)]
pub struct LiteChain {
    pub svm: Rc<RefCell<LiteSVM>>,
    pub genesis: Rc<RefCell<String>>,
    /// Transactions handed to `send` (accepted or not).
    pub sends: Rc<Cell<u32>>,
    pub simulations: Rc<Cell<u32>>,
    /// Signatures of transactions that landed OK, in order.
    pub landed: Rc<RefCell<Vec<String>>>,
    /// Compute units of each landed transaction.
    pub units: Rc<RefCell<Vec<u64>>>,
    /// Runs right before the next `send` is executed (to let "another keeper" act first), then is cleared.
    pub before_send: Rc<RefCell<Option<Hook>>>,
    /// The next N `send` calls fail with a rate-limit error before reaching the SVM.
    pub rate_limit_sends: Rc<Cell<u32>>,
    /// The next N `status` calls answer "unknown" (landed but not confirmed yet).
    pub status_unknown: Rc<Cell<u32>>,
    /// `claim_addresses` fails as a provider that disables getProgramAccounts would.
    pub claims_unsupported: Rc<Cell<bool>>,
    pub program: Pubkey,
}

fn raw(a: RawAcct) -> RawAccount {
    RawAccount { lamports: a.lamports, owner: a.owner, data: a.data }
}

fn tx_err(e: &solana_transaction_error::TransactionError) -> TxErr {
    use solana_transaction_error::TransactionError as T;
    let custom = match e {
        T::InstructionError(_, anchor_lang::solana_program::instruction::error::InstructionError::Custom(c)) => {
            Some(*c)
        }
        _ => None,
    };
    TxErr { custom, text: format!("{e:?}") }
}

impl Chain for LiteChain {
    fn genesis_hash(&self) -> CResult<String> {
        Ok(self.genesis.borrow().clone())
    }
    fn now(&self) -> CResult<i64> {
        let c: anchor_lang::solana_program::clock::Clock = self.svm.borrow().get_sysvar();
        Ok(c.unix_timestamp)
    }
    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>> {
        Ok(self.svm.borrow().get_account(key).filter(|a| a.lamports > 0).map(raw))
    }
    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>> {
        keys.iter().map(|k| self.account(k)).collect()
    }
    fn claim_addresses(&self) -> CResult<Vec<Pubkey>> {
        if self.claims_unsupported.get() {
            return Err(ChainError::Unsupported("getProgramAccounts is disabled by this provider".into()));
        }
        use anchor_lang::Discriminator;
        use solana_account::ReadableAccount;
        let disc = core_vault::state::PayoutClaim::DISCRIMINATOR;
        let svm = self.svm.borrow();
        let mut v: Vec<Pubkey> = svm
            .accounts_db()
            .inner
            .iter()
            .filter(|(_, a)| {
                *a.owner() == self.program
                    && a.data().len() == core_vault::state::PayoutClaim::SPACE
                    && a.data().starts_with(disc)
            })
            .map(|(k, _)| *k)
            .collect();
        v.sort_by_key(|k| k.to_bytes());
        Ok(v)
    }
    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>> {
        use anchor_lang::Discriminator;
        use solana_account::ReadableAccount;
        let disc = core_vault::state::ProductRegistry::DISCRIMINATOR;
        let svm = self.svm.borrow();
        Ok(svm
            .accounts_db()
            .inner
            .iter()
            .filter(|(_, a)| *a.owner() == self.program && a.data().starts_with(disc))
            .map(|(k, a)| (*k, RawAccount { lamports: a.lamports(), owner: *a.owner(), data: a.data().to_vec() }))
            .collect())
    }
    fn balance(&self, key: &Pubkey) -> CResult<u64> {
        Ok(self.svm.borrow().get_balance(key).unwrap_or(0))
    }
    fn blockhash(&self) -> CResult<(Hash, u64)> {
        Ok((self.svm.borrow().latest_blockhash(), u64::MAX))
    }
    fn block_height(&self) -> CResult<u64> {
        Ok(0)
    }
    fn simulate(&self, tx: &[u8]) -> CResult<Sim> {
        self.simulations.set(self.simulations.get() + 1);
        let t: Transaction = bincode::deserialize(tx).map_err(|_| ChainError::Other("bad tx".into()))?;
        match self.svm.borrow().simulate_transaction(t) {
            Ok(i) => Ok(Sim { err: None, logs: i.meta.logs, units: Some(i.meta.compute_units_consumed) }),
            Err(f) => {
                Ok(Sim { err: Some(tx_err(&f.err)), logs: f.meta.logs, units: Some(f.meta.compute_units_consumed) })
            }
        }
    }
    fn send(&self, tx: &[u8]) -> CResult<String> {
        self.sends.set(self.sends.get() + 1);
        if self.rate_limit_sends.get() > 0 {
            self.rate_limit_sends.set(self.rate_limit_sends.get() - 1);
            return Err(ChainError::RateLimited("HTTP 429".into()));
        }
        let hook = self.before_send.borrow_mut().take();
        if let Some(mut h) = hook {
            h();
        }
        let t: Transaction = bincode::deserialize(tx).map_err(|_| ChainError::Other("bad tx".into()))?;
        let sig = t.signatures[0].to_string();
        let r = self.svm.borrow_mut().send_transaction(t);
        self.svm.borrow_mut().expire_blockhash();
        match r {
            Ok(m) => {
                self.landed.borrow_mut().push(sig.clone());
                self.units.borrow_mut().push(m.compute_units_consumed);
                Ok(sig)
            }
            Err(f) => Err(ChainError::Rejected(tx_err(&f.err))),
        }
    }
    fn status(&self, _sig: &str) -> CResult<Option<TxStatus>> {
        if self.status_unknown.get() > 0 {
            self.status_unknown.set(self.status_unknown.get() - 1);
            return Ok(None);
        }
        Ok(Some(TxStatus::Confirmed))
    }
}

// ------------------------------------------------------------------ the world

/// A wallet that owes nothing yet: a keypair with token accounts in both mints.
pub struct Trader {
    pub kp: Keypair,
}

pub struct Env {
    pub chain: LiteChain,
    pub keys: Keys,
    pub sl8: Keypair,
    pub rov: Keypair,
    /// Pays for everything the test sets up (never the keeper).
    pub payer: Keypair,
    pub usdc: Pubkey,
    pub usdt: Pubkey,
    pub sector: Pubkey,
    pub traders: Vec<Trader>,
    pub logs: Rc<RefCell<Vec<String>>>,
}

fn so_vault() -> PathBuf {
    std::env::var_os("CORE_VAULT_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/test-deploy/core_vault.so"))
}

fn so_sector() -> PathBuf {
    std::env::var_os("MOCK_SECTOR_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("tools/devnet-sector/target/deploy/mock_sector.so"))
}

fn sector_data(tag: u8, fields: &[u64]) -> Vec<u8> {
    let mut d = vec![tag];
    for f in fields {
        d.extend_from_slice(&f.to_le_bytes());
    }
    d
}

impl Env {
    /// Vault initialised, one product registered (the mock sector), tally initialised, no claims.
    pub fn new() -> Env {
        let mut svm = LiteSVM::new().with_sigverify(true);
        let so = so_vault();
        let bytes = std::fs::read(&so)
            .unwrap_or_else(|_| panic!("cannot read {} (run scripts/build-test-so.sh)", so.display()));
        let has = |k: &Pubkey| bytes.windows(32).any(|w| w == k.as_ref());
        assert!(
            has(&core_vault::constants::SL8_ADMIN_PUBKEY) && has(&core_vault::constants::ROV_ADMIN_PUBKEY),
            "{} is not the localnet build",
            so.display()
        );
        svm.add_program_from_file(core_vault::ID, &so).unwrap();
        let sector = Pubkey::new_unique();
        let sso = so_sector();
        svm.add_program_from_file(sector, &sso)
            .unwrap_or_else(|_| panic!("cannot load {} (cargo build-sbf the mock sector first)", sso.display()));
        let mut clock: anchor_lang::solana_program::clock::Clock = svm.get_sysvar();
        clock.unix_timestamp = T0;
        svm.set_sysvar(&clock);
        let sl8 = fixture("sl8-admin.json");
        let rov = fixture("rov-admin.json");
        assert_eq!(sl8.pubkey(), core_vault::constants::SL8_ADMIN_PUBKEY);
        let payer = Keypair::new();
        for k in [&sl8, &rov, &payer] {
            svm.airdrop(&k.pubkey(), 1_000_000_000_000).unwrap();
        }
        let chain = LiteChain {
            svm: Rc::new(RefCell::new(svm)),
            genesis: Rc::new(RefCell::new(LOCAL_GENESIS.into())),
            sends: Rc::new(Cell::new(0)),
            simulations: Rc::new(Cell::new(0)),
            landed: Rc::new(RefCell::new(vec![])),
            units: Rc::new(RefCell::new(vec![])),
            before_send: Rc::new(RefCell::new(None)),
            rate_limit_sends: Rc::new(Cell::new(0)),
            status_unknown: Rc::new(Cell::new(0)),
            claims_unsupported: Rc::new(Cell::new(false)),
            program: core_vault::ID,
        };
        let keys = Keys::compiled();
        let e = Env {
            chain,
            keys,
            sl8,
            rov,
            payer,
            usdc: Pubkey::new_unique(),
            usdt: Pubkey::new_unique(),
            sector,
            traders: vec![],
            logs: Rc::new(RefCell::new(vec![])),
        };
        let (usdc, usdt) = (e.usdc, e.usdt);
        e.set_mint(&usdc);
        e.set_mint(&usdt);
        let sl8pk = e.sl8.pubkey();
        for m in [usdc, usdt] {
            e.set_token(&ata(&sl8pk, &m), &m, &sl8pk, 0);
        }
        // init_vault and register_product, signed by the public test admin keys
        let init = AdminIx::InitVault { usdc_mint: usdc, usdt_mint: usdt }.build(&e.keys);
        e.send_admin(init);
        let product = Product {
            product_program_id: sector,
            fee_split_bps: 6500,
            challenge_sizes: vec![Tier { size: 5_000_000_000, cost: 50_000_000 }],
            max_payout_count: 1000,
            reset_price_bps: vec![100],
        };
        e.send_admin(AdminIx::RegisterProduct(product).build(&e.keys));
        let pay = e.payer.pubkey();
        let init_tally = Instruction {
            program_id: sector,
            accounts: vec![
                AccountMeta::new(pay, true),
                AccountMeta::new(e.tally(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: sector_data(0, &[]),
        };
        e.send(&[init_tally], &[]);
        e
    }

    pub fn tally(&self) -> Pubkey {
        si::derive_payout_tally(&self.sector).0
    }

    pub fn vault(&self) -> Pubkey {
        self.keys.vault()
    }

    pub fn pool(&self, mint: &Pubkey) -> Pubkey {
        self.keys.pool(mint)
    }

    // ---- raw state
    pub fn set_raw(&self, addr: &Pubkey, data: Vec<u8>, owner: Pubkey) {
        let mut svm = self.chain.svm.borrow_mut();
        let lamports = svm.minimum_balance_for_rent_exemption(data.len());
        svm.set_account(*addr, RawAcct { lamports, data, owner, executable: false, rent_epoch: 0 }).unwrap();
    }

    pub fn set_mint(&self, addr: &Pubkey) {
        let mut data = vec![0u8; SplMint::LEN];
        SplMint::pack(
            SplMint {
                mint_authority: COption::None,
                supply: 0,
                decimals: 6,
                is_initialized: true,
                freeze_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        self.set_raw(addr, data, spl_token::ID);
    }

    pub fn set_token(&self, addr: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
        self.set_token_state(addr, mint, owner, amount, AccountState::Initialized);
    }

    pub fn set_token_state(&self, addr: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64, state: AccountState) {
        let lamports = self.chain.svm.borrow().get_account(addr).map(|a| a.lamports);
        let mut data = vec![0u8; SplAccount::LEN];
        SplAccount::pack(
            SplAccount {
                mint: *mint,
                owner: *owner,
                amount,
                delegate: COption::None,
                state,
                is_native: COption::None,
                delegated_amount: 0,
                close_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        let mut svm = self.chain.svm.borrow_mut();
        let l = lamports.filter(|l| *l > 0).unwrap_or_else(|| svm.minimum_balance_for_rent_exemption(data.len()));
        svm.set_account(*addr, RawAcct { lamports: l, data, owner: spl_token::ID, executable: false, rent_epoch: 0 })
            .unwrap();
    }

    pub fn token(&self, addr: &Pubkey) -> Option<SplAccount> {
        self.chain
            .svm
            .borrow()
            .get_account(addr)
            .filter(|a| a.lamports > 0)
            .and_then(|a| SplAccount::unpack(&a.data).ok())
    }

    pub fn balance(&self, addr: &Pubkey) -> u64 {
        self.token(addr).map(|t| t.amount).unwrap_or(0)
    }

    pub fn fill_pool(&self, mint: &Pubkey, amount: u64) {
        let frozen = self.token(&self.pool(mint)).map(|t| t.state == AccountState::Frozen).unwrap_or(false);
        self.set_token_state(
            &self.pool(mint),
            mint,
            &self.vault(),
            amount,
            if frozen { AccountState::Frozen } else { AccountState::Initialized },
        );
    }

    pub fn freeze(&self, mint: &Pubkey, frozen: bool) {
        let amount = self.balance(&self.pool(mint));
        self.set_token_state(
            &self.pool(mint),
            mint,
            &self.vault(),
            amount,
            if frozen { AccountState::Frozen } else { AccountState::Initialized },
        );
    }

    pub fn freeze_ata(&self, wallet: &Pubkey, mint: &Pubkey) {
        let a = ata(wallet, mint);
        let amount = self.balance(&a);
        self.set_token_state(&a, mint, wallet, amount, AccountState::Frozen);
    }

    pub fn delete_account(&self, addr: &Pubkey) {
        self.chain.svm.borrow_mut().set_account(*addr, RawAcct::default()).unwrap();
    }

    pub fn vault_state(&self) -> core_vault::state::VaultState {
        let a = self.chain.svm.borrow().get_account(&self.vault()).unwrap();
        core_vault::state::VaultState::try_deserialize(&mut a.data.as_slice()).unwrap()
    }

    pub fn set_vault_state(&self, f: impl FnOnce(&mut core_vault::state::VaultState)) {
        let key = self.vault();
        let acc = self.chain.svm.borrow().get_account(&key).unwrap();
        let mut vs = core_vault::state::VaultState::try_deserialize(&mut acc.data.as_slice()).unwrap();
        f(&mut vs);
        let mut data = vec![];
        vs.try_serialize(&mut data).unwrap();
        data.resize(acc.data.len(), 0);
        self.chain.svm.borrow_mut().set_account(key, RawAcct { data, ..acc }).unwrap();
    }

    pub fn registry(&self, product: &Pubkey) -> core_vault::state::ProductRegistry {
        let a = self.chain.svm.borrow().get_account(&self.keys.registry(product)).unwrap();
        core_vault::state::ProductRegistry::try_deserialize(&mut a.data.as_slice()).unwrap()
    }

    // ---- time
    pub fn now(&self) -> i64 {
        self.chain.now().unwrap()
    }

    pub fn set_time(&self, t: i64) {
        let mut svm = self.chain.svm.borrow_mut();
        let mut c: anchor_lang::solana_program::clock::Clock = svm.get_sysvar();
        c.unix_timestamp = t;
        svm.set_sysvar(&c);
    }

    pub fn advance(&self, secs: i64) {
        self.set_time(self.now() + secs);
    }

    // ---- sending (setup transactions; panics on failure)
    pub fn send(&self, ixs: &[Instruction], signers: &[&Keypair]) {
        self.try_send(ixs, signers).unwrap_or_else(|e| panic!("setup transaction failed: {e:?}"));
    }

    pub fn try_send(
        &self,
        ixs: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<(), solana_transaction_error::TransactionError> {
        let mut svm = self.chain.svm.borrow_mut();
        let bh = svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&self.payer.pubkey()), &bh);
        let mut tx = Transaction::new_unsigned(msg);
        let mut all: Vec<&Keypair> = vec![&self.payer];
        all.extend(signers.iter().copied());
        tx.sign(&all, bh);
        let r = svm.send_transaction(tx);
        svm.expire_blockhash();
        r.map(|_| ()).map_err(|f| f.err)
    }

    pub fn send_admin(&self, ix: Instruction) {
        let (sl8, rov) = (dup(&self.sl8), dup(&self.rov));
        self.send(&[ix], &[&sl8, &rov]);
    }

    // ---- traders and claims
    /// A new wallet with both token accounts funded (1,000 of each).
    pub fn new_trader(&mut self) -> usize {
        let kp = Keypair::new();
        let w = kp.pubkey();
        for m in [self.usdc, self.usdt] {
            self.set_token(&ata(&w, &m), &m, &w, 1_000 * M);
        }
        self.chain.svm.borrow_mut().airdrop(&w, 100_000_000).unwrap();
        self.traders.push(Trader { kp });
        self.traders.len() - 1
    }

    fn trader_state(&self, wallet: &Pubkey, challenge: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[b"trader_state", self.sector.as_ref(), wallet.as_ref(), &challenge.to_le_bytes()],
            &self.keys.program_id,
        )
        .0
    }

    pub fn claim_address(&self, wallet: &Pubkey, challenge: u64, request: u64) -> Pubkey {
        let ts = self.trader_state(wallet, challenge);
        Pubkey::find_program_address(&[b"payout_claim", ts.as_ref(), &request.to_le_bytes()], &self.keys.program_id).0
    }

    fn authority(&self) -> Pubkey {
        si::derive_sector_authority(&self.sector).0
    }

    /// The trader buys the $50 challenge `challenge` in `mint` through the mock sector (a real deposit_fee).
    pub fn buy_challenge(&self, trader: usize, challenge: u64, mint: &Pubkey) {
        let t = &self.traders[trader];
        let w = t.kp.pubkey();
        let remaining = [
            AccountMeta::new(self.trader_state(&w, challenge), false),
            AccountMeta::new(self.payer.pubkey(), true),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(self.vault(), false),
            AccountMeta::new_readonly(w, true),
            AccountMeta::new(ata(&w, mint), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(self.pool(mint), false),
            AccountMeta::new(self.keys.sl8_ata(mint), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ];
        let v = si::deposit_fee(
            self.keys.program_id,
            self.authority(),
            self.keys.registry(&self.sector),
            &remaining,
            si::DepositFeeArgs {
                amount: 50_000_000,
                product_program_id: self.sector,
                challenge_id: challenge,
                trader_wallet: w,
                account_size: 5_000_000_000,
            },
        );
        let mut accounts = v.accounts;
        accounts[0].is_signer = false;
        accounts.push(AccountMeta::new_readonly(self.keys.program_id, false));
        let ix = Instruction {
            program_id: self.sector,
            accounts,
            data: sector_data(2, &[50_000_000, challenge, 5_000_000_000]),
        };
        self.send(&[ix], &[&dup(&t.kp)]);
    }

    /// The mock sector queues a payout claim of `amount` (request id `request`) for the trader's challenge.
    pub fn request_payout(&self, trader: usize, challenge: u64, amount: u64, request: u64) -> Pubkey {
        let w = self.traders[trader].kp.pubkey();
        let claim = self.claim_address(&w, challenge, request);
        let v = si::request_payout(
            self.keys.program_id,
            self.authority(),
            self.keys.registry(&self.sector),
            &[
                AccountMeta::new(self.trader_state(&w, challenge), false),
                AccountMeta::new(self.vault(), false),
                AccountMeta::new(claim, false),
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            si::RequestPayoutArgs {
                trader_wallet: w,
                amount,
                product_program_id: self.sector,
                challenge_id: challenge,
                proposed_request_id: request,
            },
        );
        let mut accounts = v.accounts;
        accounts[0].is_signer = false;
        accounts.push(AccountMeta::new(self.tally(), false));
        accounts.push(AccountMeta::new_readonly(self.keys.program_id, false));
        self.send(
            &[Instruction { program_id: self.sector, accounts, data: sector_data(3, &[amount, challenge, request]) }],
            &[],
        );
        claim
    }

    /// Buys challenge 1 (in `mint`) for a fresh trader and queues one claim of `amount`. Returns (trader index, claim).
    pub fn queue_claim(&mut self, mint: &Pubkey, amount: u64) -> (usize, Pubkey) {
        let t = self.new_trader();
        self.buy_challenge(t, 1, mint);
        let c = self.request_payout(t, 1, amount, 1);
        (t, c)
    }

    /// A bond claim: the depositor bonds $50 (6 months, in `mint`), 90 days pass, then the exit is requested.
    /// The clock is advanced, so call this before setting up the timing the test is about.
    pub fn queue_bond_claim(&mut self, mint: &Pubkey) -> (usize, Pubkey) {
        let t = self.new_trader();
        let w = self.traders[t].kp.pubkey();
        let position =
            Pubkey::find_program_address(&[b"bond", w.as_ref(), &0u64.to_le_bytes()], &self.keys.program_id).0;
        let tracker = Pubkey::find_program_address(&[b"bond_cap", w.as_ref()], &self.keys.program_id).0;
        use anchor_lang::{InstructionData, ToAccountMetas};
        let dep = Instruction {
            program_id: self.keys.program_id,
            accounts: core_vault::accounts::DepositBond {
                depositor: w,
                vault_state: self.vault(),
                depositor_token_account: ata(&w, mint),
                mint: *mint,
                pool_token_account: self.pool(mint),
                sl8_token_account: self.keys.sl8_ata(mint),
                bond_position: position,
                bond_cap_tracker: tracker,
                token_program: spl_token::ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: core_vault::instruction::DepositBond {
                deposit_index: 0,
                principal: 50_000_000,
                term: core_vault::state::BondTerm::SixMonths,
            }
            .data(),
        };
        self.send(&[dep], &[&dup(&self.traders[t].kp)]);
        self.advance(core_vault::constants::BOND_6M_LOCK_SECS);
        let claim =
            Pubkey::find_program_address(&[b"bond_claim", w.as_ref(), &0u64.to_le_bytes()], &self.keys.program_id).0;
        let req = Instruction {
            program_id: self.keys.program_id,
            accounts: core_vault::accounts::RequestBondPayout {
                depositor: w,
                vault_state: self.vault(),
                bond_position: position,
                bond_cap_tracker: tracker,
                payout_claim: claim,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: core_vault::instruction::RequestBondPayout { deposit_index: 0 }.data(),
        };
        self.send(&[req], &[&dup(&self.traders[t].kp)]);
        (t, claim)
    }

    /// Overwrites the mock sector's tally (the "deliberately wrong tally" switch).
    pub fn set_tally(&self, count: u64, total: u64) {
        let ix = Instruction {
            program_id: self.sector,
            accounts: vec![AccountMeta::new(self.tally(), false)],
            data: sector_data(1, &[count, total]),
        };
        self.send(&[ix], &[]);
    }

    pub fn claim(&self, addr: &Pubkey) -> Option<core_vault::state::PayoutClaim> {
        self.chain
            .svm
            .borrow()
            .get_account(addr)
            .filter(|a| a.lamports > 0)
            .and_then(|a| core_vault::state::PayoutClaim::try_deserialize(&mut a.data.as_slice()).ok())
    }

    pub fn wallet(&self, trader: usize) -> Pubkey {
        self.traders[trader].kp.pubkey()
    }

    pub fn open_claims(&self) -> Vec<(Pubkey, core_vault::state::PayoutClaim)> {
        self.chain.claim_addresses().unwrap().into_iter().filter_map(|a| self.claim(&a).map(|c| (a, c))).collect()
    }

    // ---- keepers
    /// A keeper with its own funded throwaway fee payer, sharing this chain. `cfg` tweaks welcome.
    pub fn keeper(&self) -> Keeper<LiteChain> {
        self.keeper_with(Config::default())
    }

    pub fn keeper_with(&self, cfg: Config) -> Keeper<LiteChain> {
        let kp = Keypair::new();
        self.chain.svm.borrow_mut().airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        self.keeper_for(kp, cfg)
    }

    pub fn keeper_for(&self, kp: Keypair, cfg: Config) -> Keeper<LiteChain> {
        let payer = kp.pubkey();
        let mut log = Logger::quiet();
        log.capture = Some(self.logs.clone());
        let mut k = Keeper::new(self.chain.clone(), Some(kp), payer, self.keys, Cluster::Localnet, cfg, log);
        k.sleeper = Box::new(NoSleep);
        k
    }

    pub fn log_lines(&self) -> Vec<serde_json::Value> {
        self.logs.borrow().iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    pub fn logged(&self, event: &str) -> Vec<serde_json::Value> {
        self.log_lines().into_iter().filter(|l| l["event"] == event).collect()
    }

    pub fn alert_kinds(&self) -> Vec<String> {
        self.log_lines()
            .iter()
            .filter(|l| l["level"] == "alert")
            .map(|l| l["alert"].as_str().unwrap().to_string())
            .collect()
    }

    /// Runs passes until the keeper reports nothing more to do or `max` passes happened.
    pub fn drive(&self, k: &mut Keeper<LiteChain>, max: usize) -> Vec<setl8_keeper::runner::PassReport> {
        let mut out = vec![];
        for _ in 0..max {
            let r = k.pass();
            let done = !r.progress;
            out.push(r);
            if done {
                break;
            }
        }
        out
    }
}

/// What `settle_claims` must pay a claim, from the program's own planner and the cycle snapshot.
pub fn expected_payment(e: &Env, owed: u64) -> (u64, u64) {
    let vs = e.vault_state();
    let (num, den) = core_vault::utils::cycle_ratio(vs.cycle_available_snapshot, vs.cycle_owed_snapshot);
    let (uf, tf) = (
        e.token(&e.pool(&e.usdc)).map(|t| t.state == AccountState::Frozen).unwrap_or(false),
        e.token(&e.pool(&e.usdt)).map(|t| t.state == AccountState::Frozen).unwrap_or(false),
    );
    let p = core_vault::utils::plan_settlement_with_frozen(
        owed,
        num,
        den,
        e.balance(&e.pool(&e.usdc)),
        uf,
        e.balance(&e.pool(&e.usdt)),
        tf,
    )
    .unwrap();
    (p.from_usdc, p.from_usdt)
}

pub fn queue<T>(v: Vec<T>) -> VecDeque<T> {
    v.into_iter().collect()
}
