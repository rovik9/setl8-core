//! LiteSVM integration tests for the MOCK sector program against the real core-vault `.so`.
//!
//! Needs two prebuilt programs (see README.md):
//!   * the mock sector:  `cargo build-sbf --manifest-path tools/devnet-sector/program/Cargo.toml
//!                          --sbf-out-dir tools/devnet-sector/target/deploy`
//!   * the vault, localnet (public test admin keys) build: `scripts/build-test-so.sh`
//!     -> `target/test-deploy/core_vault.so`
//!
//! Signature verification is ON, so what passes here passes on a real cluster: the `sector_authority` PDA is never
//! a signer of the outer transaction, only the mock's `invoke_signed` signs for it.
#![allow(clippy::result_large_err)] // litesvm's TransactionResult, as in tests-rs

use std::path::PathBuf;

use anchor_lang::solana_program::{
    clock::Clock,
    instruction::error::InstructionError,
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
use borsh::BorshSerialize;
use core_vault::constants::{OPEN_CLAIMS_CEILING, PAUSE_RECONCILIATION_DEFICIT, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY};
use core_vault::errors::VaultError;
use core_vault::state::{PayoutClaim, ProductRegistry, TraderState, TraderStatus, VaultState};
use litesvm::types::{TransactionMetadata, TransactionResult};
use litesvm::LiteSVM;
use mock_sector::SectorIx;
use si::{ChallengeSize, PayoutTally};
use solana_account::Account as RawAccount;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

const T0: i64 = 1_700_000_000;
/// Tier used throughout: size 12_345 costs 77. 77 * 6500 / 10_000 = 50.05, so floor puts 50 in the pool, 27 to SL8.
const TIER: (u64, u64) = (12_345, 77);
const POOL_SHARE: u64 = 50;
const SL8_SHARE: u64 = 27;
const START_BALANCE: u64 = 1_000_000;

fn vault_repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load_keypair(name: &str) -> Keypair {
    let txt = std::fs::read_to_string(vault_repo_root().join("tests/fixtures").join(name)).unwrap();
    let bytes: Vec<u8> = txt
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    Keypair::try_from(bytes.as_slice()).unwrap()
}

fn mock_so() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/deploy/mock_sector.so");
    assert!(
        p.exists(),
        "{} is missing: build it first (cargo build-sbf, see tests/mock_sector.rs header)",
        p.display()
    );
    p
}

fn vault_so() -> PathBuf {
    let p = std::env::var_os("CORE_VAULT_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|| vault_repo_root().join("target/test-deploy/core_vault.so"));
    let bytes =
        std::fs::read(&p).unwrap_or_else(|e| panic!("cannot read {} ({e}): run scripts/build-test-so.sh", p.display()));
    for (name, k) in [("SL8_ADMIN_PUBKEY", SL8_ADMIN_PUBKEY), ("ROV_ADMIN_PUBKEY", ROV_ADMIN_PUBKEY)] {
        assert!(
            bytes.windows(32).any(|w| w == k.as_ref()),
            "{} does not embed {name}: it is not the localnet (public test key) build",
            p.display()
        );
    }
    p
}

struct Env {
    svm: LiteSVM,
    sl8: Keypair,
    rov: Keypair,
    /// Fee payer of every transaction and the rent payer inside the vault CPIs.
    payer: Keypair,
    mock: Pubkey,
    usdc: Pubkey,
    usdt: Pubkey,
    vault: Pubkey,
    usdc_pool: Pubkey,
    sl8_usdc: Pubkey,
}

struct Trader {
    key: Keypair,
    usdc: Pubkey,
}

impl Trader {
    fn wallet(&self) -> Pubkey {
        self.key.pubkey()
    }
}

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

impl Env {
    /// Sigverify ON, vault initialised, mock loaded at a fresh program id. The mock is NOT registered yet.
    fn new() -> Self {
        let mut svm = LiteSVM::new().with_sigverify(true);
        svm.add_program_from_file(core_vault::ID, vault_so()).expect("load core_vault.so");
        let mock = Pubkey::new_unique();
        svm.add_program_from_file(mock, mock_so()).expect("load mock_sector.so");
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = T0;
        svm.set_sysvar(&clock);

        let (sl8, rov) = (load_keypair("sl8-admin.json"), load_keypair("rov-admin.json"));
        assert_eq!(sl8.pubkey(), SL8_ADMIN_PUBKEY, "fixture/const drift");
        assert_eq!(rov.pubkey(), ROV_ADMIN_PUBKEY, "fixture/const drift");
        let payer = Keypair::new();
        for k in [sl8.pubkey(), rov.pubkey(), payer.pubkey()] {
            svm.airdrop(&k, 10_000_000_000).unwrap();
        }
        let vault = pda(&[b"vault_state", SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()], &core_vault::ID);
        let (usdc, usdt) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut e = Self {
            svm,
            sl8,
            rov,
            payer,
            mock,
            usdc,
            usdt,
            vault,
            usdc_pool: pda(&[b"pool", vault.as_ref(), usdc.as_ref()], &core_vault::ID),
            sl8_usdc: Pubkey::default(),
        };
        e.set_mint(&usdc);
        e.set_mint(&usdt);
        e.sl8_usdc = Pubkey::new_unique();
        e.set_token(&e.sl8_usdc.clone(), &usdc, &SL8_ADMIN_PUBKEY, 0);

        let ix = e.init_vault_ix();
        e.ok(&[ix], &[]);
        e
    }

    /// Vault initialised, mock registered as a sector (one tier, 65% to the pool), tally initialised.
    fn ready() -> Self {
        let mut e = Self::new();
        let ix = e.register_ix();
        e.ok(&[ix], &[]);
        let ix = e.init_tally_ix();
        e.ok(&[ix], &[]);
        e
    }

    /// `ready()` plus a funded trader who has bought one challenge (id 1) through the mock.
    fn with_trader() -> (Self, Trader) {
        let mut e = Self::ready();
        let t = e.new_trader();
        let ix = e.deposit_ix(&t, 1);
        e.ok(&[ix], &[&t.key]);
        (e, t)
    }

    // ---------------------------------------------------------------- raw state
    fn set_raw(&mut self, addr: &Pubkey, data: Vec<u8>, owner: Pubkey) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(*addr, RawAccount { lamports, data, owner, executable: false, rent_epoch: 0 }).unwrap();
    }
    fn set_mint(&mut self, addr: &Pubkey) {
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
    fn set_token(&mut self, addr: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
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
    fn new_trader(&mut self) -> Trader {
        let key = Keypair::new();
        self.svm.airdrop(&key.pubkey(), 1_000_000_000).unwrap();
        let usdc = Pubkey::new_unique();
        let mint = self.usdc;
        self.set_token(&usdc, &mint, &key.pubkey(), START_BALANCE);
        Trader { key, usdc }
    }
    fn token_balance(&self, addr: &Pubkey) -> u64 {
        SplAccount::unpack(&self.svm.get_account(addr).expect("token account").data).unwrap().amount
    }

    // ---------------------------------------------------------------- decoding
    fn registry_addr(&self) -> Pubkey {
        pda(&[b"product_registry", self.mock.as_ref()], &core_vault::ID)
    }
    fn trader_state_addr(&self, t: &Trader, challenge_id: u64) -> Pubkey {
        pda(&[b"trader_state", self.mock.as_ref(), t.wallet().as_ref(), &challenge_id.to_le_bytes()], &core_vault::ID)
    }
    fn claim_addr(&self, t: &Trader, challenge_id: u64, request_id: u64) -> Pubkey {
        pda(
            &[b"payout_claim", self.trader_state_addr(t, challenge_id).as_ref(), &request_id.to_le_bytes()],
            &core_vault::ID,
        )
    }
    fn authority(&self) -> Pubkey {
        si::derive_sector_authority(&self.mock).0
    }
    fn tally_addr(&self) -> Pubkey {
        si::derive_payout_tally(&self.mock).0
    }
    fn vault_account<T: AccountDeserialize>(&self, addr: &Pubkey) -> T {
        let a = self.svm.get_account(addr).unwrap_or_else(|| panic!("no account at {addr}"));
        assert_eq!(a.owner, core_vault::ID, "{addr} is not owned by the vault");
        T::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn registry(&self) -> ProductRegistry {
        self.vault_account(&self.registry_addr())
    }
    fn vault_state(&self) -> VaultState {
        self.vault_account(&self.vault)
    }
    fn tally_bytes(&self) -> Vec<u8> {
        self.svm.get_account(&self.tally_addr()).expect("tally account").data
    }
    fn tally(&self) -> (u64, u64) {
        let t = PayoutTally::parse(&self.tally_bytes()).expect("valid tally");
        (t.requested_count, t.requested_total)
    }
    /// The vault's own request counters, the numbers `reconcile_product` compares the tally with.
    fn vault_books(&self) -> (u64, u64) {
        let r = self.registry();
        (r.total_requests_emitted, r.total_requested_amount)
    }

    // ---------------------------------------------------------------- sending
    /// Fee payer = `self.payer`. The SL8/ROV admins and any `extra` keypairs sign only if the message requires them.
    fn send(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> TransactionResult {
        let bh = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&self.payer.pubkey()), &bh);
        let required = msg.account_keys[..msg.header.num_required_signatures as usize].to_vec();
        let mut tx = Transaction::new_unsigned(msg);
        let mut signers: Vec<&Keypair> = vec![&self.payer, &self.sl8, &self.rov];
        signers.extend(extra.iter().copied());
        signers.retain(|k| required.contains(&k.pubkey()));
        tx.partial_sign(&signers, bh);
        let r = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        r
    }
    fn ok(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> TransactionMetadata {
        match self.send(ixs, extra) {
            Ok(m) => m,
            Err(f) => panic!("expected success, got {:?}\nlogs:\n{}", f.err, f.meta.logs.join("\n")),
        }
    }

    // ---------------------------------------------------------------- vault instructions
    fn init_vault_ix(&self) -> Instruction {
        let accounts = core_vault::accounts::InitVault {
            sl8_admin: self.sl8.pubkey(),
            rov_admin: self.rov.pubkey(),
            vault_state: self.vault,
            usdc_mint: self.usdc,
            usdt_mint: self.usdt,
            usdc_pool: self.usdc_pool,
            usdt_pool: pda(&[b"pool", self.vault.as_ref(), self.usdt.as_ref()], &core_vault::ID),
            token_program: spl_token::ID,
            system_program: system_program::ID,
        };
        Instruction {
            program_id: core_vault::ID,
            accounts: accounts.to_account_metas(None),
            data: core_vault::instruction::InitVault { usdc_mint: self.usdc, usdt_mint: self.usdt }.data(),
        }
    }
    /// Registers the mock as a sector: 65% of fees to the pool, one tier, up to 5 payouts per challenge.
    fn register_ix(&self) -> Instruction {
        si::register_product(
            core_vault::ID,
            self.sl8.pubkey(),
            self.rov.pubkey(),
            self.registry_addr(),
            &[AccountMeta::new_readonly(system_program::ID, false)],
            si::RegisterProductArgs {
                product_program_id: self.mock,
                fee_split_bps: 6500,
                challenge_sizes: vec![ChallengeSize { size: TIER.0, cost: TIER.1 }],
                max_payout_count: 5,
                reset_price_bps: vec![100, 150, 450],
            },
        )
    }
    fn reconcile_ix(&self) -> Instruction {
        Instruction {
            program_id: core_vault::ID,
            accounts: core_vault::accounts::ReconcileProduct {
                caller: self.payer.pubkey(),
                product_registry: self.registry_addr(),
                payout_tally: self.tally_addr(),
            }
            .to_account_metas(None),
            data: core_vault::instruction::ReconcileProduct { product_program_id: self.mock }.data(),
        }
    }

    // ---------------------------------------------------------------- mock instructions
    fn mock_ix(&self, accounts: Vec<AccountMeta>, ix: SectorIx) -> Instruction {
        Instruction { program_id: self.mock, accounts, data: borsh_bytes(&ix) }
    }
    /// InitTally: `[0] payer (signer, w), [1] tally (w), [2] system program`.
    fn init_tally_ix(&self) -> Instruction {
        self.init_tally_ix_at(self.tally_addr())
    }
    fn init_tally_ix_at(&self, tally: Pubkey) -> Instruction {
        self.mock_ix(
            vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new(tally, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            SectorIx::InitTally,
        )
    }
    /// SetTally: `[0] tally (w)`.
    fn set_tally_ix(&self, count: u64, total: u64) -> Instruction {
        self.mock_ix(vec![AccountMeta::new(self.tally_addr(), false)], SectorIx::SetTally { count, total })
    }
    /// DepositFee: the 12 vault accounts in the shared-interfaces builder order, then the vault program.
    fn deposit_ix(&self, t: &Trader, challenge_id: u64) -> Instruction {
        let pool = self.usdc_pool;
        self.mock_ix(
            vec![
                AccountMeta::new_readonly(self.authority(), false), // 0 sector_authority (NOT a signer)
                AccountMeta::new(self.registry_addr(), false),      // 1 product_registry
                AccountMeta::new(self.trader_state_addr(t, challenge_id), false), // 2 trader_state
                AccountMeta::new(self.payer.pubkey(), true),        // 3 payer
                AccountMeta::new_readonly(system_program::ID, false), // 4 system_program
                AccountMeta::new_readonly(self.vault, false),       // 5 vault_state
                AccountMeta::new_readonly(t.wallet(), true),        // 6 trader (signer)
                AccountMeta::new(t.usdc, false),                    // 7 trader_token_account
                AccountMeta::new_readonly(self.usdc, false),        // 8 mint
                AccountMeta::new(pool, false),                      // 9 pool_token_account
                AccountMeta::new(self.sl8_usdc, false),             // 10 sl8_token_account
                AccountMeta::new_readonly(spl_token::ID, false),    // 11 token_program
                AccountMeta::new_readonly(core_vault::ID, false),   // 12 vault program
            ],
            SectorIx::DepositFee { amount: TIER.1, challenge_id, account_size: TIER.0 },
        )
    }
    /// RequestPayout: the 7 vault accounts in the builder order, then the tally, then the vault program.
    fn payout_ix(&self, t: &Trader, challenge_id: u64, amount: u64, request_id: u64) -> Instruction {
        self.mock_ix(
            vec![
                AccountMeta::new_readonly(self.authority(), false), // 0 sector_authority (NOT a signer)
                AccountMeta::new(self.registry_addr(), false),      // 1 product_registry
                AccountMeta::new(self.trader_state_addr(t, challenge_id), false), // 2 trader_state
                AccountMeta::new(self.vault, false),                // 3 vault_state
                AccountMeta::new(self.claim_addr(t, challenge_id, request_id), false), // 4 payout_claim
                AccountMeta::new(self.payer.pubkey(), true),        // 5 payer
                AccountMeta::new_readonly(system_program::ID, false), // 6 system_program
                AccountMeta::new(self.tally_addr(), false),         // 7 tally
                AccountMeta::new_readonly(core_vault::ID, false),   // 8 vault program
            ],
            SectorIx::RequestPayout { amount, challenge_id, proposed_request_id: request_id },
        )
    }
}

fn borsh_bytes(ix: &SectorIx) -> Vec<u8> {
    ix.try_to_vec().unwrap()
}

fn instruction_error(r: &TransactionResult) -> (u8, InstructionError) {
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

/// The transaction failed on instruction 0 with exactly this vault error (the mock passes the CPI error through).
fn assert_vault_err(r: &TransactionResult, want: VaultError) {
    let name = format!("{want:?}");
    let want = u32::from(want);
    match instruction_error(r) {
        (0, InstructionError::Custom(code)) => {
            assert_eq!(code, want, "wrong error code for VaultError::{name}\nlogs:\n{}", logs(r))
        }
        other => panic!("expected Custom({want}) for VaultError::{name}, got {other:?}\nlogs:\n{}", logs(r)),
    }
}

// ================================================================== 1. InitTally

#[test]
fn init_tally_creates_the_tally_and_a_second_init_fails() {
    let mut e = Env::new();
    let ix = e.init_tally_ix();
    e.ok(&[ix], &[]);

    let acct = e.svm.get_account(&e.tally_addr()).expect("tally exists");
    assert_eq!(acct.owner, e.mock, "owned by the sector program");
    assert_eq!(acct.data.len(), si::PAYOUT_TALLY_MIN_LEN);
    assert!(acct.lamports >= e.svm.minimum_balance_for_rent_exemption(acct.data.len()), "rent exempt");
    assert_eq!(e.tally(), (0, 0));
    assert_eq!(&acct.data[..8], b"SL8TALLY");

    // a second InitTally must not reset anything: bump the tally first, then try again
    let ix = e.set_tally_ix(7, 700);
    e.ok(&[ix], &[]);
    let ix = e.init_tally_ix();
    let r = e.send(&[ix], &[]);
    assert_eq!(instruction_error(&r), (0, InstructionError::AccountAlreadyInitialized), "{}", logs(&r));
    assert_eq!(e.tally(), (7, 700), "double init left the tally untouched");
}

#[test]
fn init_tally_rejects_a_wrong_tally_address() {
    let mut e = Env::new();
    let wrong = Pubkey::new_unique();
    let ix = e.init_tally_ix_at(wrong);
    let r = e.send(&[ix], &[]);
    assert_eq!(
        instruction_error(&r),
        (0, InstructionError::Custom(mock_sector::MockError::WrongTallyAddress as u32)),
        "{}",
        logs(&r)
    );
    assert!(e.svm.get_account(&wrong).is_none());
}

#[test]
fn init_tally_works_on_a_prefunded_address() {
    let rent = Env::new().svm.minimum_balance_for_rent_exemption(si::PAYOUT_TALLY_MIN_LEN);
    // dust below rent exemption (needs a top-up), exactly rent, and far above rent (kept, not refunded)
    for dust in [1u64, rent - 1, rent, rent + 5_000_000] {
        let mut e = Env::new();
        e.svm
            .set_account(
                e.tally_addr(),
                RawAccount {
                    lamports: dust,
                    data: vec![],
                    owner: system_program::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let ix = e.init_tally_ix();
        e.ok(&[ix], &[]);

        let acct = e.svm.get_account(&e.tally_addr()).unwrap();
        assert_eq!(acct.owner, e.mock, "dust={dust}");
        assert_eq!(acct.data.len(), si::PAYOUT_TALLY_MIN_LEN, "dust={dust}");
        assert_eq!(acct.lamports, dust.max(rent), "dust={dust}: topped up to rent exemption, never reduced");
        assert_eq!(e.tally(), (0, 0), "dust={dust}");
    }
}

#[test]
fn a_missing_tally_counts_as_zero_zero() {
    // missing tally + vault books 0/0 => match (documents the vault's rule the mock relies on)
    let mut e = Env::new();
    let ix = e.register_ix();
    e.ok(&[ix], &[]);
    let ix = e.reconcile_ix();
    e.ok(&[ix], &[]);
    assert!(e.registry().active);
}

// ================================================================== 2. full path

#[test]
fn full_path_deposit_payout_and_reconcile_match() {
    let mut e = Env::ready();
    let t = e.new_trader();
    let before = (e.token_balance(&t.usdc), e.token_balance(&e.usdc_pool), e.token_balance(&e.sl8_usdc));
    assert_eq!(before, (START_BALANCE, 0, 0));

    // ---- DepositFee through the mock: the trader signs, the mock signs for the sector authority
    let ix = e.deposit_ix(&t, 1);
    e.ok(&[ix], &[&t.key]);

    let ts: TraderState = e.vault_account(&e.trader_state_addr(&t, 1));
    assert_eq!(ts.trader_wallet, t.wallet());
    assert_eq!(ts.product_program_id, e.mock);
    assert_eq!((ts.challenge_id, ts.account_size, ts.payout_count), (1, TIER.0, 0));
    assert_eq!(ts.status, TraderStatus::Active);

    assert_eq!(e.token_balance(&t.usdc), START_BALANCE - TIER.1, "trader paid the full fee");
    assert_eq!(e.token_balance(&e.usdc_pool), POOL_SHARE, "pool got floor(77 * 65%)");
    assert_eq!(e.token_balance(&e.sl8_usdc), SL8_SHARE, "SL8 got the exact remainder");
    assert_eq!(POOL_SHARE + SL8_SHARE, TIER.1);

    // ---- RequestPayout through the mock: claim + vault counters + tally
    assert_eq!(e.tally(), (0, 0));
    let ix = e.payout_ix(&t, 1, 1_000, 1);
    let m = e.ok(&[ix], &[]);
    assert_eq!(m.return_data.program_id, core_vault::ID, "the vault's Paid outcome is the transaction's return data");
    assert_eq!(m.return_data.data, vec![si::PayoutOutcome::Paid as u8]);

    let claim: PayoutClaim = e.vault_account(&e.claim_addr(&t, 1, 1));
    assert_eq!((claim.owed, claim.request_id, claim.trader_wallet), (1_000, 1, t.wallet()));
    assert_eq!(claim.product_program_id, e.mock);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (1, 1_000));
    assert_eq!(e.vault_books(), (1, 1_000));
    assert_eq!(e.tally(), (1, 1_000), "the mock bumped the tally in step");

    // a second request on the same challenge
    let ix = e.payout_ix(&t, 1, 250, 2);
    e.ok(&[ix], &[]);
    assert_eq!(e.vault_books(), (2, 1_250));
    assert_eq!(e.tally(), (2, 1_250));
    assert_eq!(e.vault_state().open_claims_total, 1_250);
    // no tokens moved by payout requests
    assert_eq!(e.token_balance(&e.usdc_pool), POOL_SHARE);

    // ---- permissionless reconcile: MATCH, product stays active, registry untouched
    let reg_before = e.svm.get_account(&e.registry_addr()).unwrap().data;
    let ix = e.reconcile_ix();
    let m = e.ok(&[ix], &[]);
    assert!(!m.logs.join("\n").contains("MISMATCH"), "{}", m.logs.join("\n"));
    let r = e.registry();
    assert!(r.active);
    assert_eq!((r.pause_reason, r.paused_since), (0, 0));
    assert_eq!(e.svm.get_account(&e.registry_addr()).unwrap().data, reg_before, "a match changes nothing");
}

// ================================================================== 3. wrong tally

#[test]
fn a_wrong_tally_makes_reconcile_pause_the_product_and_return_ok() {
    let (mut e, t) = Env::with_trader();
    let ix = e.payout_ix(&t, 1, 1_000, 1);
    e.ok(&[ix], &[]);
    assert_eq!((e.vault_books(), e.tally()), ((1, 1_000), (1, 1_000)));

    // the deliberately-wrong-tally switch (anyone may call it)
    let ix = e.set_tally_ix(5, 999);
    e.ok(&[ix], &[]);
    assert_eq!(e.tally(), (5, 999));

    let mut clock: Clock = e.svm.get_sysvar();
    clock.unix_timestamp = T0 + 4_321;
    e.svm.set_sysvar(&clock);

    let ix = e.reconcile_ix();
    let m = e.ok(&[ix], &[]); // Ok: an error would revert the pause
    let log = m.logs.join("\n");
    assert!(log.contains("MISMATCH tally_count=5 tally_total=999 vault_count=1 vault_total=1000"), "{log}");
    let r = e.registry();
    assert!(!r.active, "paused");
    assert_eq!(r.pause_reason, PAUSE_RECONCILIATION_DEFICIT);
    assert_eq!(r.paused_since, T0 + 4_321);

    // and a paused product refuses new business
    let t2 = e.new_trader();
    let ix = e.deposit_ix(&t2, 9);
    let r = e.send(&[ix], &[&t2.key]);
    assert_vault_err(&r, VaultError::ProductNotActive);

    // fixing the tally and reconciling again does not un-pause (that stays 2-of-2) and is refused
    let ix = e.set_tally_ix(1, 1_000);
    e.ok(&[ix], &[]);
    let ix = e.reconcile_ix();
    let r = e.send(&[ix], &[]);
    assert_vault_err(&r, VaultError::ProductAlreadyPaused);
}

// ================================================================== 4. request id mismatch

#[test]
fn a_wrong_proposed_request_id_fails_and_leaves_the_tally_untouched() {
    let (mut e, t) = Env::with_trader();

    // fresh challenge: the vault expects request id 1
    let tally_before = e.tally_bytes();
    for wrong in [2u64, 0, 7] {
        let ix = e.payout_ix(&t, 1, 500, wrong);
        let r = e.send(&[ix], &[]);
        assert_vault_err(&r, VaultError::RequestIdMismatch);
        assert_eq!(e.tally_bytes(), tally_before, "proposed id {wrong}: tally untouched");
        assert_eq!(e.vault_books(), (0, 0));
    }

    // after one accepted request the vault expects 2: replaying 1 fails, tally stays at the accepted value
    let ix = e.payout_ix(&t, 1, 500, 1);
    e.ok(&[ix], &[]);
    let tally_after_one = e.tally_bytes();
    assert_eq!(e.tally(), (1, 500));
    let ix = e.payout_ix(&t, 1, 500, 1);
    let r = e.send(&[ix], &[]);
    // The claim address for id 1 already exists, but the vault's id check runs before it is touched.
    assert_vault_err(&r, VaultError::RequestIdMismatch);
    assert_eq!(e.tally_bytes(), tally_after_one);
    assert_eq!(e.vault_books(), (1, 500));
}

// ================================================================== 5. claims ceiling

#[test]
fn a_request_over_the_claims_ceiling_fails_and_leaves_the_tally_untouched() {
    let (mut e, t) = Env::with_trader();

    // one past the ceiling is refused, tally and books untouched
    let tally_before = e.tally_bytes();
    let vs_before = e.vault_state();
    let ix = e.payout_ix(&t, 1, OPEN_CLAIMS_CEILING + 1, 1);
    let r = e.send(&[ix], &[]);
    assert_vault_err(&r, VaultError::ClaimsCeilingExceeded);
    assert_eq!(e.tally_bytes(), tally_before);
    assert_eq!(e.vault_books(), (0, 0));
    assert_eq!(e.vault_state().open_claims_total, vs_before.open_claims_total);
    assert!(e.svm.get_account(&e.claim_addr(&t, 1, 1)).is_none(), "no claim was created");

    // exactly the ceiling is accepted (boundary), and the tally follows
    let ix = e.payout_ix(&t, 1, OPEN_CLAIMS_CEILING, 1);
    e.ok(&[ix], &[]);
    assert_eq!(e.tally(), (1, OPEN_CLAIMS_CEILING));
    assert_eq!(e.vault_books(), (1, OPEN_CLAIMS_CEILING));
    let ix = e.reconcile_ix();
    e.ok(&[ix], &[]);
    assert!(e.registry().active, "still a match at the ceiling");

    // the ceiling is now full: even 1 more is refused
    let tally_full = e.tally_bytes();
    let ix = e.payout_ix(&t, 1, 1, 2);
    let r = e.send(&[ix], &[]);
    assert_vault_err(&r, VaultError::ClaimsCeilingExceeded);
    assert_eq!(e.tally_bytes(), tally_full);
    assert_eq!(e.tally(), (1, OPEN_CLAIMS_CEILING));
}

// ================================================================== 6. the mock is what makes it work

#[test]
fn calling_the_vault_directly_with_a_forged_sector_authority_fails() {
    let (mut e, t) = Env::with_trader();
    let tally_before = e.tally_bytes();
    let books_before = e.vault_books();
    let reg_before = e.svm.get_account(&e.registry_addr()).unwrap().data;

    let direct = |authority: Pubkey, e: &Env| {
        si::request_payout(
            core_vault::ID,
            authority,
            e.registry_addr(),
            &[
                AccountMeta::new(e.trader_state_addr(&t, 1), false),
                AccountMeta::new(e.vault, false),
                AccountMeta::new(e.claim_addr(&t, 1, 1), false),
                AccountMeta::new(e.payer.pubkey(), true),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            si::RequestPayoutArgs {
                trader_wallet: t.wallet(),
                amount: 1_000,
                product_program_id: e.mock,
                challenge_id: 1,
                proposed_request_id: 1,
            },
        )
    };

    // (a) an attacker keypair signs in the sector_authority slot: the vault rejects it as not the sector's PDA
    let forged = Keypair::new();
    let r = e.send(&[direct(forged.pubkey(), &e)], &[&forged]);
    assert_vault_err(&r, VaultError::Unauthorized);

    // (b) the REAL authority PDA address marked as a signer: nobody can produce its signature outside the mock's
    // invoke_signed, so the transaction cannot even be signed
    let r = e.send(&[direct(e.authority(), &e)], &[]);
    assert!(r.is_err(), "an unsigned PDA must not pass as a signer");
    assert!(
        !matches!(&r, Err(f) if matches!(f.err, TransactionError::InstructionError(..))),
        "rejected before execution"
    );

    assert_eq!(e.tally_bytes(), tally_before);
    assert_eq!(e.vault_books(), books_before);
    assert_eq!(e.svm.get_account(&e.registry_addr()).unwrap().data, reg_before);
    assert!(e.svm.get_account(&e.claim_addr(&t, 1, 1)).is_none());

    // sanity: the same request through the mock works
    let ix = e.payout_ix(&t, 1, 1_000, 1);
    e.ok(&[ix], &[]);
    assert_eq!(e.tally(), (1, 1_000));
}

// ================================================================== extras

#[test]
fn abandoned_outcome_does_not_move_the_tally() {
    let (mut e, t) = Env::with_trader();
    let ix = e.payout_ix(&t, 1, 100, 1);
    e.ok(&[ix], &[]);
    assert_eq!(e.tally(), (1, 100));

    // idle past the inactivity window: the vault returns Ok with Abandoned and creates no claim
    let mut clock: Clock = e.svm.get_sysvar();
    clock.unix_timestamp = T0 + core_vault::constants::INACTIVITY_LIMIT_SECS + 10;
    e.svm.set_sysvar(&clock);

    let ix = e.payout_ix(&t, 1, 100, 2);
    let m = e.ok(&[ix], &[]);
    assert_eq!(m.return_data.data, vec![si::PayoutOutcome::Abandoned as u8]);
    assert_eq!(e.tally(), (1, 100), "Abandoned is not counted");
    assert_eq!(e.vault_books(), (1, 100));
    assert!(e.svm.get_account(&e.claim_addr(&t, 1, 2)).is_none());
    let ix = e.reconcile_ix();
    e.ok(&[ix], &[]);
    assert!(e.registry().active, "books and tally still agree");
}

#[test]
fn set_tally_needs_no_authorisation_by_design() {
    let mut e = Env::ready();
    // a random stranger is the only signer-ish account (the fee payer); SetTally still works
    let ix = e.set_tally_ix(42, 4_200);
    e.ok(&[ix], &[]);
    assert_eq!(e.tally(), (42, 4_200));
    // ... and it cannot be pointed at any other account
    let ix = e.mock_ix(vec![AccountMeta::new(e.payer.pubkey(), false)], SectorIx::SetTally { count: 1, total: 1 });
    let r = e.send(&[ix], &[]);
    assert_eq!(
        instruction_error(&r),
        (0, InstructionError::Custom(mock_sector::MockError::WrongTallyAddress as u32)),
        "{}",
        logs(&r)
    );
}
