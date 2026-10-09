//! Shared harness: a LiteSVM "cluster" behind the tool's `Rpc` trait, a scripted `Host`, and
//! helpers to run the real command line against temp files. Only generated throwaway keys
//! are used, plus the PUBLIC test admin keys the `localnet` build embeds (their private
//! halves are committed in tests/fixtures/ by design).
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountDeserialize;
use anchor_spl::token::spl_token::{
    self,
    solana_program::{program_option::COption, program_pack::Pack},
    state::{Account as SplAccount, AccountState, Mint as SplMint},
};
use litesvm::LiteSVM;
use setl8_admin::admin_ix::Keys;
use setl8_admin::constants::ata_address;
use setl8_admin::error::{Error, Result};
use setl8_admin::host::Host;
use setl8_admin::rpc::{Rpc, RpcAccount, SimResult};
use setl8_admin::txfile::TxFile;
use solana_account::Account as RawAccount;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub const T0: i64 = 1_700_000_000;
pub const M: u64 = 1_000_000;
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

/// Writes `kp` as a solana-keygen style JSON file with mode 0600.
pub fn write_key(dir: &Path, name: &str, kp: &Keypair) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_string(&kp.to_bytes().to_vec()).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    p
}

// -------------------------------------------------------------------- the fake cluster

#[derive(Clone)]
pub struct LiteRpc {
    pub svm: Rc<RefCell<LiteSVM>>,
    pub genesis: Rc<RefCell<String>>,
    pub sends: Rc<Cell<u32>>,
    pub simulations: Rc<Cell<u32>>,
    pub last_error: Rc<RefCell<Option<TransactionError>>>,
    pub last_sig: Rc<RefCell<Option<String>>>,
}

impl Rpc for LiteRpc {
    fn genesis_hash(&self) -> Result<String> {
        Ok(self.genesis.borrow().clone())
    }
    fn account(&self, key: &Pubkey) -> Result<Option<RpcAccount>> {
        Ok(self.svm.borrow().get_account(key).filter(|a| a.lamports > 0).map(|a| RpcAccount {
            lamports: a.lamports,
            owner: a.owner,
            data: a.data,
            executable: a.executable,
        }))
    }
    fn program_accounts(&self, program: &Pubkey, disc: &[u8; 8]) -> Result<Vec<(Pubkey, RpcAccount)>> {
        use solana_account::ReadableAccount;
        let svm = self.svm.borrow();
        Ok(svm
            .accounts_db()
            .inner
            .iter()
            .filter(|(_, a)| a.owner() == program && a.data().starts_with(disc))
            .map(|(k, a)| {
                (
                    *k,
                    RpcAccount {
                        lamports: a.lamports(),
                        owner: *a.owner(),
                        data: a.data().to_vec(),
                        executable: false,
                    },
                )
            })
            .collect())
    }
    fn latest_blockhash(&self) -> Result<String> {
        Ok(self.svm.borrow().latest_blockhash().to_string())
    }
    fn min_balance_for_rent(&self, len: usize) -> Result<u64> {
        Ok(self.svm.borrow().minimum_balance_for_rent_exemption(len))
    }
    fn simulate(&self, tx: &[u8]) -> Result<SimResult> {
        self.simulations.set(self.simulations.get() + 1);
        let t: Transaction = bincode::deserialize(tx).map_err(|_| Error("bad tx bytes".into()))?;
        match self.svm.borrow().simulate_transaction(t) {
            Ok(i) => Ok(SimResult { err: None, logs: i.meta.logs, units: Some(i.meta.compute_units_consumed) }),
            Err(f) => Ok(SimResult {
                err: Some(format!("{:?}", f.err)),
                logs: f.meta.logs,
                units: Some(f.meta.compute_units_consumed),
            }),
        }
    }
    fn send(&self, tx: &[u8]) -> Result<String> {
        self.sends.set(self.sends.get() + 1);
        let t: Transaction = bincode::deserialize(tx).map_err(|_| Error("bad tx bytes".into()))?;
        let sig = t.signatures[0].to_string();
        let r = self.svm.borrow_mut().send_transaction(t);
        self.svm.borrow_mut().expire_blockhash();
        match r {
            Ok(_) => {
                *self.last_sig.borrow_mut() = Some(sig.clone());
                Ok(sig)
            }
            Err(f) => {
                *self.last_error.borrow_mut() = Some(f.err.clone());
                Err(Error(format!("transaction failed: {:?}", f.err)))
            }
        }
    }
    fn confirm(&self, _sig: &str) -> Result<()> {
        Ok(())
    }
}

// -------------------------------------------------------------------- the scripted host

pub struct TestHost {
    pub out: String,
    pub err: String,
    pub answers: VecDeque<String>,
    pub prompts: Vec<String>,
    pub rpc: LiteRpc,
    pub rpc_urls: Vec<String>,
    /// When set, `rpc()` fails (an "offline" machine).
    pub no_network: bool,
    /// Answer "retype the first 8 characters" prompts from the last `Message SHA-256:` printed.
    pub auto_hash: bool,
}

impl Host for TestHost {
    fn out(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn err(&mut self, s: &str) {
        self.err.push_str(s);
    }
    fn prompt(&mut self, q: &str) -> Result<String> {
        self.prompts.push(q.to_string());
        if self.auto_hash && q.contains("retype the first 8 characters") {
            let at = self.out.rfind("Message SHA-256: ").expect("a hash was printed before the prompt");
            return Ok(self.out[at + 17..at + 25].to_string());
        }
        self.answers.pop_front().ok_or_else(|| Error("refused: no scripted answer (test)".into()))
    }
    fn rpc(&mut self, url: &str) -> Result<Box<dyn Rpc>> {
        self.rpc_urls.push(url.to_string());
        if self.no_network {
            return Err(Error("no network on this machine (test)".into()));
        }
        Ok(Box::new(self.rpc.clone()))
    }
}

impl TestHost {
    pub fn all_output(&self) -> String {
        format!("{}{}{}", self.out, self.err, self.prompts.join("\n"))
    }
}

// -------------------------------------------------------------------- the world

pub struct World {
    pub rpc: LiteRpc,
    pub dir: PathBuf,
    pub sl8: Keypair,
    pub rov: Keypair,
    pub sl8_key: PathBuf,
    pub rov_key: PathBuf,
    pub usdc: Pubkey,
    pub usdt: Pubkey,
    pub keys: Keys,
}

fn so_path() -> PathBuf {
    std::env::var_os("CORE_VAULT_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/test-deploy/core_vault.so"))
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl World {
    /// Sigverify ON, program loaded, mints and SL8's token accounts created, vault NOT initialised.
    pub fn bare() -> World {
        let mut svm = LiteSVM::new().with_sigverify(true);
        let so = so_path();
        let bytes = std::fs::read(&so)
            .unwrap_or_else(|_| panic!("cannot read {} (run scripts/build-test-so.sh)", so.display()));
        let has = |k: &Pubkey| bytes.windows(32).any(|w| w == k.as_ref());
        assert!(
            has(&core_vault::constants::SL8_ADMIN_PUBKEY) && has(&core_vault::constants::ROV_ADMIN_PUBKEY),
            "{} is not the localnet (test-key) build",
            so.display()
        );
        svm.add_program_from_file(core_vault::ID, &so).expect("load program");
        let mut clock: anchor_lang::solana_program::clock::Clock = svm.get_sysvar();
        clock.unix_timestamp = T0;
        svm.set_sysvar(&clock);

        let sl8 = fixture("sl8-admin.json");
        let rov = fixture("rov-admin.json");
        assert_eq!(sl8.pubkey(), core_vault::constants::SL8_ADMIN_PUBKEY, "fixture/const drift");
        assert_eq!(rov.pubkey(), core_vault::constants::ROV_ADMIN_PUBKEY, "fixture/const drift");
        svm.airdrop(&sl8.pubkey(), 100_000_000_000).unwrap();
        svm.airdrop(&rov.pubkey(), 100_000_000_000).unwrap();

        let dir = std::env::temp_dir().join(format!(
            "setl8-admin-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sl8_key = write_key(&dir, "sl8.json", &sl8);
        let rov_key = write_key(&dir, "rov.json", &rov);

        let rpc = LiteRpc {
            svm: Rc::new(RefCell::new(svm)),
            genesis: Rc::new(RefCell::new(LOCAL_GENESIS.to_string())),
            sends: Rc::new(Cell::new(0)),
            simulations: Rc::new(Cell::new(0)),
            last_error: Rc::new(RefCell::new(None)),
            last_sig: Rc::new(RefCell::new(None)),
        };
        let w = World {
            rpc,
            dir,
            sl8,
            rov,
            sl8_key,
            rov_key,
            usdc: Pubkey::new_unique(),
            usdt: Pubkey::new_unique(),
            keys: Keys::compiled(),
        };
        let (usdc, usdt) = (w.usdc, w.usdt);
        w.set_mint(&usdc);
        w.set_mint(&usdt);
        let sl8_pk = w.sl8.pubkey();
        for m in [usdc, usdt] {
            w.set_token(&ata_address(&sl8_pk, &m), &m, &sl8_pk, 0);
        }
        w
    }

    /// The vault initialised by a DIRECT instruction (not through the tool), so a tool bug
    /// cannot hide in the fixture.
    pub fn with_vault() -> World {
        let w = World::bare();
        let ix = AdminIxDirect::init_vault(&w.keys, w.usdc, w.usdt);
        w.send_direct(ix);
        w
    }

    pub fn host(&self) -> TestHost {
        TestHost {
            out: String::new(),
            err: String::new(),
            answers: VecDeque::new(),
            prompts: vec![],
            rpc: self.rpc.clone(),
            rpc_urls: vec![],
            no_network: false,
            auto_hash: false,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn set_raw(&self, addr: &Pubkey, data: Vec<u8>, owner: Pubkey) {
        let mut svm = self.rpc.svm.borrow_mut();
        let lamports = svm.minimum_balance_for_rent_exemption(data.len());
        svm.set_account(*addr, RawAccount { lamports, data, owner, executable: false, rent_epoch: 0 }).unwrap();
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
        self.set_raw(addr, data, spl_token::ID);
    }

    pub fn pool(&self, mint: &Pubkey) -> Pubkey {
        self.keys.pool(mint)
    }

    /// Puts `amount` into a pool token account (state preserved: Initialized).
    pub fn fill_pool(&self, mint: &Pubkey, amount: u64) {
        self.set_token(&self.pool(mint), mint, &self.keys.vault(), amount);
    }

    pub fn token_balance(&self, addr: &Pubkey) -> u64 {
        let a = self.rpc.svm.borrow().get_account(addr).expect("account");
        SplAccount::unpack(&a.data).unwrap().amount
    }

    pub fn vault_state(&self) -> core_vault::state::VaultState {
        let a = self.rpc.svm.borrow().get_account(&self.keys.vault()).expect("vault exists");
        core_vault::state::VaultState::try_deserialize(&mut a.data.as_slice()).unwrap()
    }

    pub fn registry(&self, product: &Pubkey) -> Option<core_vault::state::ProductRegistry> {
        let a = self.rpc.svm.borrow().get_account(&self.keys.registry(product))?;
        Some(core_vault::state::ProductRegistry::try_deserialize(&mut a.data.as_slice()).unwrap())
    }

    pub fn exists(&self, addr: &Pubkey) -> bool {
        self.rpc.svm.borrow().get_account(addr).is_some()
    }

    pub fn expire_blockhash(&self) {
        self.rpc.svm.borrow_mut().expire_blockhash();
    }

    pub fn set_genesis(&self, g: &str) {
        *self.rpc.genesis.borrow_mut() = g.to_string();
    }

    /// Sends an instruction signed by both admins, fee payer SL8, directly to the SVM.
    pub fn send_direct(&self, ix: anchor_lang::solana_program::instruction::Instruction) {
        let mut svm = self.rpc.svm.borrow_mut();
        let bh = svm.latest_blockhash();
        let msg = solana_message::Message::new_with_blockhash(&[ix], Some(&self.sl8.pubkey()), &bh);
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[&self.sl8, &self.rov], bh);
        svm.send_transaction(tx).expect("direct admin instruction");
        svm.expire_blockhash();
    }

    /// Runs the real command line against this world with scripted `answers`.
    pub fn run(&self, answers: &[&str], args: &[&str]) -> (i32, TestHost) {
        let mut h = self.host();
        h.answers = answers.iter().map(|s| s.to_string()).collect();
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = setl8_admin::cli::run_guarded(&mut h, &argv);
        (code, h)
    }

    /// As `run`, but retype-the-hash prompts are answered correctly from the printed hash.
    pub fn run_auto(&self, answers: &[&str], args: &[&str]) -> (i32, TestHost) {
        let mut h = self.host();
        h.auto_hash = true;
        h.answers = answers.iter().map(|s| s.to_string()).collect();
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = setl8_admin::cli::run_guarded(&mut h, &argv);
        (code, h)
    }

    pub fn tx_file(&self, name: &str) -> TxFile {
        TxFile::load(&self.path(name)).unwrap()
    }

    pub fn hash_of(&self, name: &str) -> String {
        self.tx_file(name).message_sha256
    }

    pub fn s(&self, p: &Path) -> String {
        p.to_string_lossy().to_string()
    }

    /// `plan` against the localnet world with a recent blockhash (same session) unless `extra` says otherwise.
    pub fn plan(&self, instruction: &str, out: &str, args: &[&str]) -> (i32, TestHost) {
        let out_path = self.s(&self.path(out));
        let mut v = vec!["plan", instruction, "--cluster", "localnet", "--out", &out_path];
        v.extend_from_slice(args);
        self.run(&[], &v)
    }

    pub fn inspect(&self, name: &str, extra: &[&str]) -> (i32, TestHost) {
        let p = self.s(&self.path(name));
        let mut v = vec!["inspect", p.as_str()];
        v.extend_from_slice(extra);
        self.run(&[], &v)
    }

    /// `sign` as `key` with the correct 8-character confirmation.
    pub fn sign(&self, name: &str, key: &Path, extra: &[&str]) -> (i32, TestHost) {
        let want = self.hash_of(name)[..8].to_string();
        self.sign_with_answer(name, key, &want, extra)
    }

    pub fn sign_with_answer(&self, name: &str, key: &Path, answer: &str, extra: &[&str]) -> (i32, TestHost) {
        let p = self.s(&self.path(name));
        let k = self.s(key);
        let mut v = vec!["sign", p.as_str(), "--keypair", k.as_str()];
        v.extend_from_slice(extra);
        self.run(&[answer], &v)
    }

    pub fn send(&self, name: &str, extra: &[&str]) -> (i32, TestHost) {
        let p = self.s(&self.path(name));
        let mut v = vec!["send", p.as_str(), "--cluster", "localnet"];
        v.extend_from_slice(extra);
        self.run(&[], &v)
    }

    /// Full ceremony: plan, inspect, sign SL8, inspect, sign ROV, send. Returns the send result.
    pub fn ceremony(&self, instruction: &str, args: &[&str]) -> (i32, TestHost) {
        let (c, h) = self.plan(instruction, "tx.json", args);
        assert_eq!(c, 0, "plan failed:\n{}{}", h.out, h.err);
        let (c, h) = self.inspect("tx.json", &[]);
        assert_eq!(c, 0, "inspect failed:\n{}{}", h.out, h.err);
        let (c, h) = self.sign("tx.json", &self.sl8_key, &[]);
        assert_eq!(c, 0, "sign SL8 failed:\n{}{}", h.out, h.err);
        let (c, h) = self.inspect("tx.json", &[]);
        assert_eq!(c, 0, "inspect 2 failed:\n{}{}", h.out, h.err);
        let (c, h) = self.sign("tx.json", &self.rov_key, &[]);
        assert_eq!(c, 0, "sign ROV failed:\n{}{}", h.out, h.err);
        self.send("tx.json", &[])
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Builders written independently of the tool's `AdminIx::build`, the way tests-rs/common
/// builds the same calls (core_vault client types, and shared-interfaces v0.4.0 for the four
/// instructions whose builders did not change in v0.4.1).
pub struct AdminIxDirect;

impl AdminIxDirect {
    pub fn init_vault(k: &Keys, usdc: Pubkey, usdt: Pubkey) -> anchor_lang::solana_program::instruction::Instruction {
        use anchor_lang::{InstructionData, ToAccountMetas};
        anchor_lang::solana_program::instruction::Instruction {
            program_id: k.program_id,
            accounts: core_vault::accounts::InitVault {
                sl8_admin: k.sl8,
                rov_admin: k.rov,
                vault_state: k.vault(),
                usdc_mint: usdc,
                usdt_mint: usdt,
                usdc_pool: k.pool(&usdc),
                usdt_pool: k.pool(&usdt),
                token_program: spl_token::ID,
                system_program: anchor_lang::solana_program::system_program::ID,
            }
            .to_account_metas(None),
            data: core_vault::instruction::InitVault { usdc_mint: usdc, usdt_mint: usdt }.data(),
        }
    }

    pub fn withdraw(
        k: &Keys,
        usdc_side: bool,
        mint: Pubkey,
        amount: u64,
    ) -> anchor_lang::solana_program::instruction::Instruction {
        use anchor_lang::{InstructionData, ToAccountMetas};
        anchor_lang::solana_program::instruction::Instruction {
            program_id: k.program_id,
            accounts: core_vault::accounts::AdminWithdrawMarketingFunds {
                sl8_admin: k.sl8,
                rov_admin: k.rov,
                vault_state: k.vault(),
                mint,
                pool_token_account: k.pool(&mint),
                sl8_token_account: k.sl8_ata(&mint),
                token_program: spl_token::ID,
            }
            .to_account_metas(None),
            data: core_vault::instruction::AdminWithdrawMarketingFunds {
                pool: if usdc_side { core_vault::state::PoolSide::Usdc } else { core_vault::state::PoolSide::Usdt },
                amount,
            }
            .data(),
        }
    }
}

/// A product.json body for `product`.
pub fn product_json(product: &Pubkey, fee_split_bps: u16) -> String {
    format!(
        r#"{{"product_program_id":"{product}","fee_split_bps":{fee_split_bps},"challenge_sizes":[{{"size":10000000000,"cost":100000000}},{{"size":50000000000,"cost":400000000}}],"max_payout_count":5,"reset_price_bps":[100,150]}}"#
    )
}

pub fn write_file(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}
