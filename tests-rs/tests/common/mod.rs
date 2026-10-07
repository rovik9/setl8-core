//! Shared harness for the core-vault LiteSVM suite.
//!
//! Every test file does `mod common;`. The clock starts at a realistic Unix
//! time (the vault uses `paused_since > 0` as its "is paused" sentinel, so a
//! clock of 0 would silently break pause handling and is not a real-chain
//! state).
#![allow(dead_code, unused_imports)]

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
    state::{ProductRegistry, TraderState},
};
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
}

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
        // CORE_VAULT_SO lets a mutation-testing run point at a different build.
        let so = std::env::var_os("CORE_VAULT_SO")
            .map(PathBuf::from)
            .unwrap_or_else(|| root().join("target/deploy/core_vault.so"));
        svm.add_program_from_file(core_vault::ID, &so)
            .unwrap_or_else(|e| panic!("cannot load {} ({e:?}) -- run `anchor build` first", so.display()));
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
        };
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
        addr
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
    pub fn deposit_tier(&mut self, s: &Sector, w: &Pubkey, id: u64, (size, cost): (u64, u64)) -> TransactionMetadata {
        let ix = deposit_fee_ix(s, w, id, cost, size, &self.payer.pubkey());
        self.ok(ix)
    }
    pub fn record(&mut self, s: &Sector, w: &Pubkey, id: u64) -> TransactionMetadata {
        self.ok(record_ix(s, w, id))
    }
    pub fn flag(&mut self, s: &Sector, w: &Pubkey, id: u64) {
        self.ok(flag_ix(s, w, id));
    }
    pub fn payout(&mut self, s: &Sector, w: &Pubkey, id: u64, amount: u64, req: u64) -> TransactionMetadata {
        self.ok(payout_ix(s, w, id, amount, req))
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

    // ---- decoding
    pub fn vault_state(&self) -> core_vault::state::VaultState {
        let a = self.svm.get_account(&self.vault).expect("vault_state account");
        assert_eq!(a.owner, core_vault::ID);
        core_vault::state::VaultState::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    pub fn registry(&self, s: &Sector) -> ProductRegistry {
        let a = self.svm.get_account(&s.registry()).expect("registry account");
        assert_eq!(a.owner, core_vault::ID);
        ProductRegistry::try_deserialize(&mut a.data.as_slice()).unwrap()
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

pub fn deposit_fee_ix(s: &Sector, w: &Pubkey, id: u64, amount: u64, size: u64, payer: &Pubkey) -> Instruction {
    si::deposit_fee(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[AccountMeta::new(s.trader(w, id), false), AccountMeta::new(*payer, true), sys()],
        si::DepositFeeArgs {
            amount,
            product_program_id: s.id,
            challenge_id: id,
            trader_wallet: *w,
            account_size: size,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn reset_ix(
    s: &Sector,
    w: &Pubkey,
    prev: u64,
    new: u64,
    amount: u64,
    phase: u8,
    payer: &Pubkey,
) -> Instruction {
    si::deposit_reset(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[
            AccountMeta::new(s.trader(w, prev), false),
            AccountMeta::new(s.trader(w, new), false),
            AccountMeta::new(*payer, true),
            sys(),
        ],
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

pub fn payout_ix(s: &Sector, w: &Pubkey, id: u64, amount: u64, req: u64) -> Instruction {
    si::request_payout(
        core_vault::ID,
        s.authority,
        s.registry(),
        &[AccountMeta::new(s.trader(w, id), false)],
        si::RequestPayoutArgs {
            trader_wallet: *w,
            amount,
            product_program_id: s.id,
            challenge_id: id,
            proposed_request_id: req,
        },
    )
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
