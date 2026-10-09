//! The rehearsal steps (a to m of the module brief), each recording PASS/FAIL rows.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::{system_instruction, system_program};
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token::{self, state::AccountState};
use base64::Engine;
use core_vault::errors::VaultError;
use core_vault::state::{BondTerm, ProductRegistry, VaultState};
use core_vault::utils::{cycle_ratio, plan_settlement_with_frozen};
use serde_json::json;
use setl8_admin::admin_ix::{AdminIx, Side};
use setl8_admin::error::{Error, Result};
use setl8_admin::fmt::{fmt_amount, parse_amount};
use setl8_admin::txfile::TxFile;

use crate::chain::*;
use crate::sector;

type R<T> = Result<T>;

fn vix<A: ToAccountMetas, D: InstructionData>(c: &Ctx, a: A, d: D) -> Instruction {
    Instruction { program_id: c.keys.program_id, accounts: a.to_account_metas(None), data: d.data() }
}

fn code(e: VaultError) -> u64 {
    u32::from(e) as u64
}

pub fn vault_state(c: &Ctx) -> Option<VaultState> {
    let (_, _, d) = c.account(&c.keys.vault()).ok().flatten()?;
    VaultState::try_deserialize(&mut d.as_slice()).ok()
}

fn registry(c: &Ctx) -> Option<ProductRegistry> {
    let (_, _, d) = c.account(&c.keys.registry(&c.sector)).ok().flatten()?;
    ProductRegistry::try_deserialize(&mut d.as_slice()).ok()
}

// ------------------------------------------------------------------ the admin tool, in process

fn admin_args(c: &Ctx) -> Vec<String> {
    vec!["--program-id".into(), c.keys.program_id.to_string()]
}

pub fn cli(c: &Ctx, args: Vec<String>) -> (i32, DrvHost) {
    let mut h = DrvHost { out: String::new(), err: String::new(), rpc_calls: 0 };
    let code = setl8_admin::cli::run_guarded(&mut h, &args);
    let _ = c;
    (code, h)
}

fn s(x: impl ToString) -> String {
    x.to_string()
}

fn keyfile(c: &Ctx, name: &str) -> String {
    c.dir.join(format!("{name}.json")).to_string_lossy().to_string()
}

fn nonce_args(c: &Ctx) -> Vec<String> {
    vec!["--nonce-account".into(), s(c.nonce.expect("nonce created"))]
}

fn onchain_args(c: &Ctx) -> Vec<String> {
    let mut v = admin_args(c);
    v.extend(["--rpc".into(), c.url.clone()]);
    v
}

pub fn status_text(c: &Ctx) -> String {
    let mut a = vec!["status".into(), "--cluster".into(), c.cluster.clone()];
    a.extend(onchain_args(c));
    let (code, h) = cli(c, a);
    if code != 0 {
        format!("STATUS FAILED: {}{}", h.out, h.err)
    } else {
        h.out
    }
}

/// plan -> inspect -> sign (SL8) -> inspect -> sign (ROV) -> send, every stage reading the file from disk.
pub fn ceremony(c: &mut Ctx, label: &str, plan_args: &[&str]) -> R<TxInfo> {
    let file = c.work.join(format!("{label}.tx.json")).to_string_lossy().to_string();
    let mut plan: Vec<String> = vec!["plan".into()];
    plan.extend(plan_args.iter().map(|x| x.to_string()));
    plan.extend(["--cluster".into(), c.cluster.clone(), "--out".into(), file.clone()]);
    plan.extend(onchain_args(c));
    plan.extend(nonce_args(c));
    let (code_, h) = cli(c, plan);
    if code_ != 0 {
        return Err(Error(format!("plan {label} failed ({code_}): {}{}", h.out, h.err)));
    }
    let mut common = admin_args(c);
    common.extend(nonce_args(c));
    let step = |c: &Ctx, name: &str, extra: Vec<String>| -> R<DrvHost> {
        let mut a: Vec<String> = vec![name.to_string(), file.clone()];
        a.extend(extra);
        a.extend(common.clone());
        let (code_, h) = cli(c, a);
        if code_ != 0 {
            return Err(Error(format!("{name} {label} failed ({code_}): {}{}", h.out, h.err)));
        }
        Ok(h)
    };
    let online = vec!["--rpc".to_string(), c.url.clone()];
    step(c, "inspect", online.clone())?;
    step(c, "sign", vec!["--keypair".into(), keyfile(c, "sl8-test")])?;
    step(c, "inspect", online.clone())?;
    step(c, "sign", vec!["--keypair".into(), keyfile(c, "rov-test")])?;
    let t0 = std::time::Instant::now();
    let h = step(c, "send", vec!["--cluster".into(), c.cluster.clone(), "--rpc".into(), c.url.clone()])?;
    let ms = t0.elapsed().as_millis();
    let sig = h
        .out
        .split("Sent. Signature: ")
        .nth(1)
        .and_then(|x| x.split_whitespace().next())
        .map(String::from)
        .ok_or_else(|| Error("no signature printed".into()))?;
    let (cu, fee, logs) = c.meta(&sig);
    let size = TxFile::load(std::path::Path::new(&file))
        .map(|f| 1 + 64 * f.signatures.len() + f.message_bytes().map(|m| m.len()).unwrap_or(0))
        .unwrap_or(0);
    Ok(TxInfo { sig, cu, fee, size, ms, logs })
}

fn note_cu(c: &mut Ctx, name: &str, t: &TxInfo) {
    if let (Some(v), Some(total)) = (program_cu(&t.logs, &c.keys.program_id), t.cu) {
        c.cus.push((name.to_string(), v, total));
    }
}

// ------------------------------------------------------------------ small chain helpers

fn ata_ix(c: &Ctx, payer: &Pubkey, wallet: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(wallet, mint), false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: vec![1],
    }
    .clone_with(c)
}

trait CloneWith {
    fn clone_with(self, c: &Ctx) -> Self;
}
impl CloneWith for Instruction {
    fn clone_with(self, _c: &Ctx) -> Self {
        self
    }
}

fn expect_vault_err(
    c: &mut Ctx,
    step: &str,
    what: &str,
    ixs: &[Instruction],
    payer: &str,
    signers: &[&str],
    want: VaultError,
) -> R<()> {
    let (err, _logs, units, size) = c.simulate(ixs, payer, signers)?;
    let got = err.as_ref().and_then(custom_code);
    let want_c = code(want);
    let actual = match (&err, got) {
        (None, _) => "succeeded (unexpected)".to_string(),
        (Some(_), Some(g)) => format!("Custom({g})"),
        (Some(e), None) => e.to_string(),
    };
    let t = TxInfo { cu: units, size, ..Default::default() };
    c.check(
        step,
        what,
        format!("{:?} = Custom({want_c})", want),
        actual.replace(&format!("Custom({want_c})"), &format!("{:?} = Custom({want_c})", want)),
        Some(&t),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn expect_token_err(
    c: &mut Ctx,
    step: &str,
    what: &str,
    ixs: &[Instruction],
    payer: &str,
    signers: &[&str],
    token_code: u64,
    name: &str,
) -> R<()> {
    let (err, _logs, units, size) = c.simulate(ixs, payer, signers)?;
    let got = err.as_ref().and_then(custom_code);
    let actual = match (&err, got) {
        (None, _) => "succeeded (unexpected)".to_string(),
        (Some(_), Some(g)) => format!("Custom({g})"),
        (Some(e), None) => e.to_string(),
    };
    let t = TxInfo { cu: units, size, ..Default::default() };
    c.check(
        step,
        what,
        format!("{name} = Custom({token_code})"),
        actual.replace(&format!("Custom({token_code})"), &format!("{name} = Custom({token_code})")),
        Some(&t),
    );
    Ok(())
}

// ------------------------------------------------------------------ setup

pub fn setup(c: &mut Ctx) -> R<()> {
    // genesis and programs
    let g = c.rpc.call("getGenesisHash", json!([])).map_err(|e| Error(e.to_string()))?;
    c.check("S00", "node genesis hash is the expected cluster", c.genesis.clone(), g.as_str().unwrap_or(""), None);
    for (name, p) in [("core-vault", c.keys.program_id), ("mock sector", c.sector)] {
        let exec = c
            .rpc
            .call("getAccountInfo", json!([p.to_string(), {"encoding": "base64", "commitment": "confirmed"}]))
            .ok()
            .map(|r| r["value"]["executable"].as_bool().unwrap_or(false))
            .unwrap_or(false);
        c.check_true(
            "S00",
            &format!("{name} program {p} is deployed"),
            "executable",
            if exec { "executable" } else { "NOT deployed" },
            exec,
            None,
        );
        if !exec {
            return Err(Error(format!("{name} program is not deployed")));
        }
    }
    // mints (6 decimals, freeze + mint authority = the throwaway freeze-authority key)
    let rent = c
        .rpc
        .call("getMinimumBalanceForRentExemption", json!([82]))
        .map_err(|e| Error(e.to_string()))?
        .as_u64()
        .unwrap_or(1_461_600);
    for name in ["usdc-mint", "usdt-mint"] {
        let mint = c.pk(name);
        if c.exists(&mint) {
            continue;
        }
        let auth = c.pk("freeze-authority");
        let ixs = vec![
            system_instruction::create_account(&c.pk("keeper"), &mint, rent, 82, &spl_token::ID),
            spl_token::instruction::initialize_mint2(&spl_token::ID, &mint, &auth, Some(&auth), 6)
                .map_err(|e| Error(e.to_string()))?,
        ];
        let t = c.send_ok(&ixs, "keeper", &[name])?;
        c.record(
            "S00",
            &format!("create {name} mint {mint} (6 decimals, freeze authority = throwaway key)"),
            "created",
            "created",
            true,
            Some(&t),
        );
    }
    // token accounts for every wallet (the SL8 wallet's BEFORE any fee: checklist precondition)
    let wallets: Vec<(String, Pubkey)> = ["sl8-test", "trader1", "trader2", "trader3", "bonder1", "bonder2"]
        .iter()
        .map(|n| (n.to_string(), c.pk(n)))
        .collect();
    let payer = c.pk("keeper");
    let mut ixs = vec![];
    for (_, w) in &wallets {
        for m in [c.usdc, c.usdt] {
            if !c.exists(&ata(w, &m)) {
                ixs.push(ata_ix(c, &payer, w, &m));
            }
        }
    }
    for chunk in ixs.chunks(6) {
        let t = c.send_ok(chunk, "keeper", &[])?;
        c.record(
            "S00",
            &format!("create {} associated token accounts (SL8 wallet first in the list)", chunk.len()),
            "created",
            "created",
            true,
            Some(&t),
        );
    }
    for (n, w) in &wallets {
        c.check_true(
            "S00",
            &format!("{n} has both token accounts"),
            "both exist",
            "checked",
            c.exists(&ata(w, &c.usdc)) && c.exists(&ata(w, &c.usdt)),
            None,
        );
    }
    // working balances
    let auth = c.pk("freeze-authority");
    let mut ixs = vec![];
    for (n, w) in &wallets {
        let want = if n.starts_with("trader") {
            1_000_000_000
        } else if n.starts_with("bonder") {
            200_000_000
        } else {
            0
        };
        for m in [c.usdc, c.usdt] {
            if want > 0 && c.balance(&ata(w, &m)) < want / 2 {
                ixs.push(
                    spl_token::instruction::mint_to(&spl_token::ID, &m, &ata(w, &m), &auth, &[], want)
                        .map_err(|e| Error(e.to_string()))?,
                );
            }
        }
    }
    for chunk in ixs.chunks(6) {
        let t = c.send_ok(chunk, "keeper", &["freeze-authority"])?;
        c.record("S00", &format!("mint test tokens to {} accounts", chunk.len()), "minted", "minted", true, Some(&t));
    }
    Ok(())
}

// ------------------------------------------------------------------ a, b, c

pub fn nonce_and_init(c: &mut Ctx) -> R<()> {
    let state = c.work.join("nonce.txt");
    if let Ok(t) = std::fs::read_to_string(&state) {
        if let Ok(p) = t.trim().parse::<Pubkey>() {
            if c.exists(&p) {
                c.nonce = Some(p);
            }
        }
    }
    if c.nonce.is_none() {
        let mut a = vec![
            "nonce-create".into(),
            "--cluster".into(),
            c.cluster.clone(),
            "--keypair".into(),
            keyfile(c, "sl8-test"),
        ];
        a.extend(["--rpc".into(), c.url.clone()]);
        let t0 = std::time::Instant::now();
        let (code_, h) = cli(c, a);
        let ok = code_ == 0;
        let nonce: Option<Pubkey> = h
            .out
            .split("Nonce account: ")
            .nth(1)
            .and_then(|x| x.split_whitespace().next())
            .and_then(|x| x.parse().ok());
        let sig =
            h.out.split("Sent. Signature: ").nth(1).and_then(|x| x.split_whitespace().next()).unwrap_or("").to_string();
        let (cu, fee, logs) = if sig.is_empty() { (None, None, vec![]) } else { c.meta(&sig) };
        let t = TxInfo { sig, cu, fee, size: 0, ms: t0.elapsed().as_millis(), logs };
        c.check_true(
            "S01",
            "nonce-create (SL8 key is the authority)",
            "created",
            &if ok { "created".to_string() } else { format!("{}{}", h.out, h.err) },
            ok && nonce.is_some(),
            Some(&t),
        );
        let n = nonce.ok_or_else(|| Error("nonce-create failed".into()))?;
        std::fs::write(&state, n.to_string())?;
        c.nonce = Some(n);
    } else {
        c.record("S01", "durable nonce account already exists", "reused", "reused", true, None);
    }
    // status of an empty vault
    if vault_state(c).is_none() {
        let st = status_text(c);
        c.check_true(
            "S01",
            "status of an empty vault reports 'not initialised' cleanly",
            "vault does not exist yet",
            if st.contains("does not exist on this cluster yet") { "vault does not exist yet" } else { &st },
            st.contains("does not exist on this cluster yet"),
            None,
        );
        let (usdc, usdt) = (s(c.usdc), s(c.usdt));
        let t = ceremony(c, "init_vault", &["init-vault", "--usdc-mint", &usdc, "--usdt-mint", &usdt])?;
        note_cu(c, "init_vault", &t);
        let vs = vault_state(c);
        let ok = vs
            .as_ref()
            .map(|v| {
                v.usdc_mint == c.usdc
                    && v.usdt_mint == c.usdt
                    && v.sl8_wallet == c.keys.sl8
                    && v.usdc_pool == c.keys.pool(&c.usdc)
            })
            .unwrap_or(false);
        c.check_true(
            "S02",
            "init_vault through the ceremony (plan, inspect, sign, sign, send)",
            "vault with our mints, pools, SL8 wallet",
            if ok { "vault with our mints, pools, SL8 wallet" } else { "mismatch" },
            ok,
            Some(&t),
        );
        let st = status_text(c);
        c.check_true(
            "S02",
            "status after init_vault",
            "mints and both pools listed",
            if st.contains(&usdc) && st.contains(&usdt) { "mints and both pools listed" } else { &st },
            st.contains(&usdc) && st.contains(&usdt),
            None,
        );
    } else {
        c.record("S02", "vault already initialised", "reused", "reused", true, None);
    }
    Ok(())
}

pub fn register(c: &mut Ctx) -> R<()> {
    if registry(c).is_some() {
        c.record("S03", "product already registered", "reused", "reused", true, None);
    } else {
        // 32 tiers (the program's maximum) and 8 reset phases (its maximum)
        let tiers: Vec<_> =
            (1..=32u64).map(|i| json!({"size": i * 1_000_000_000u64, "cost": i * 10_000_000u64})).collect();
        let cfg = json!({
            "product_program_id": s(c.sector), "fee_split_bps": 6500, "challenge_sizes": tiers,
            "max_payout_count": 10, "reset_price_bps": [100, 150, 200, 250, 300, 350, 400, 450]
        });
        let path = c.work.join("product.json");
        std::fs::write(&path, cfg.to_string())?;
        let p = path.to_string_lossy().to_string();
        let t = ceremony(c, "register_product", &["register-product", "--config", &p])?;
        note_cu(c, "register_product (32 tiers, 8 phases)", &t);
        let r = registry(c);
        let ok = r
            .as_ref()
            .map(|r| {
                r.challenge_sizes.len() == 32 && r.reset_price_bps.len() == 8 && r.fee_split_bps == 6500 && r.active
            })
            .unwrap_or(false);
        c.check_true(
            "S03",
            "register_product with 32 tiers and 8 phases through the ceremony",
            "registered, active",
            if ok { "registered, active" } else { "mismatch" },
            ok,
            Some(&t),
        );
    }
    // the sector initialises its tally BEFORE its first request (checklist 5.5)
    let tally = sector::tally(c);
    if !c.exists(&tally) {
        let payer = c.pk("keeper");
        let t = c.send_ok(&[sector::init_tally(c, &payer)], "keeper", &[])?;
        c.record(
            "S03",
            "mock sector initialises its payout tally (0/0) before the first request",
            "created",
            "created",
            true,
            Some(&t),
        );
    }
    let st = status_text(c);
    c.check_true(
        "S03",
        "status lists the product as ACTIVE",
        "ACTIVE",
        if st.contains("ACTIVE") && st.contains(&s(c.sector)) { "ACTIVE" } else { &st },
        st.contains("ACTIVE") && st.contains(&s(c.sector)),
        None,
    );
    Ok(())
}

// ------------------------------------------------------------------ d, e

fn pool_sl8_snapshot(c: &Ctx, mint: &Pubkey) -> (u64, u64) {
    (c.balance(&c.keys.pool(mint)), c.balance(&c.keys.sl8_ata(mint)))
}

pub fn fees(c: &mut Ctx) -> R<()> {
    let (size, cost) = (5_000_000_000u64, 50_000_000u64); // tier 5: $5,000 / $50
    for (who, mint_name, step) in
        [("trader1", "usdc-mint", "S04"), ("trader2", "usdt-mint", "S04"), ("trader3", "usdt-mint", "S04")]
    {
        let mint = c.pk(mint_name);
        let trader = c.pk(who);
        if c.exists(&sector::trader_state(c, &trader, 1)) {
            c.record(step, &format!("{who} challenge 1 already bought"), "reused", "reused", true, None);
            continue;
        }
        let (p0, s0) = pool_sl8_snapshot(c, &mint);
        let t0 = c.balance(&ata(&trader, &mint));
        let payer = c.pk("keeper");
        let t = c.send_ok(&[sector::deposit_fee(c, &trader, &payer, 1, cost, size, &mint)], "keeper", &[who])?;
        note_cu(c, &format!("deposit_fee ({mint_name})"), &t);
        let (p1, s1) = pool_sl8_snapshot(c, &mint);
        let pool_share = cost * 6500 / 10_000;
        c.check(
            step,
            &format!("{who} buys a $50 challenge in {mint_name}: pool gets floor(65%)"),
            fmt_amount(pool_share),
            fmt_amount(p1 - p0),
            Some(&t),
        );
        c.check(
            step,
            &format!("{who}: the SL8 wallet gets the remainder"),
            fmt_amount(cost - pool_share),
            fmt_amount(s1 - s0),
            None,
        );
        c.check(
            step,
            &format!("{who}: the trader pays exactly the cost"),
            fmt_amount(cost),
            fmt_amount(t0 - c.balance(&ata(&trader, &mint))),
            None,
        );
    }
    Ok(())
}

pub fn bonds(c: &mut Ctx) -> R<()> {
    for (who, mint_name) in [("bonder1", "usdc-mint"), ("bonder2", "usdt-mint")] {
        let mint = c.pk(mint_name);
        let dep = c.pk(who);
        let bond = Pubkey::find_program_address(&[b"bond", dep.as_ref(), &0u64.to_le_bytes()], &c.keys.program_id).0;
        let tracker = Pubkey::find_program_address(&[b"bond_cap", dep.as_ref()], &c.keys.program_id).0;
        if !c.exists(&bond) {
            let (p0, s0) = pool_sl8_snapshot(c, &mint);
            let d0 = c.balance(&ata(&dep, &mint));
            let ix = vix(
                c,
                core_vault::accounts::DepositBond {
                    depositor: dep,
                    vault_state: c.keys.vault(),
                    depositor_token_account: ata(&dep, &mint),
                    mint,
                    pool_token_account: c.keys.pool(&mint),
                    sl8_token_account: c.keys.sl8_ata(&mint),
                    bond_position: bond,
                    bond_cap_tracker: tracker,
                    token_program: spl_token::ID,
                    system_program: system_program::ID,
                },
                core_vault::instruction::DepositBond {
                    deposit_index: 0,
                    principal: 50_000_000,
                    term: BondTerm::SixMonths,
                },
            );
            let t = c.send_ok(&[ix], "keeper", &[who])?;
            note_cu(c, "deposit_bond", &t);
            let (p1, s1) = pool_sl8_snapshot(c, &mint);
            c.check(
                "S05",
                &format!("{who} bonds $50 in {mint_name}: half the principal goes to the pool"),
                fmt_amount(25_000_000),
                fmt_amount(p1 - p0),
                Some(&t),
            );
            c.check(
                "S05",
                &format!("{who}: SL8 gets the other half plus the 0.2% fee (rounded up)"),
                fmt_amount(25_100_000),
                fmt_amount(s1 - s0),
                None,
            );
            c.check(
                "S05",
                &format!("{who}: the depositor pays principal + fee"),
                fmt_amount(50_100_000),
                fmt_amount(d0 - c.balance(&ata(&dep, &mint))),
                None,
            );
        }
        let ix = vix(
            c,
            core_vault::accounts::RequestBondPayout {
                depositor: dep,
                vault_state: c.keys.vault(),
                bond_position: bond,
                bond_cap_tracker: tracker,
                payout_claim: Pubkey::find_program_address(
                    &[b"bond_claim", dep.as_ref(), &0u64.to_le_bytes()],
                    &c.keys.program_id,
                )
                .0,
                system_program: system_program::ID,
            },
            core_vault::instruction::RequestBondPayout { deposit_index: 0 },
        );
        expect_vault_err(
            c,
            "S05",
            &format!(
                "{who}: request_bond_payout right away is refused (90-day lock, cannot be waited out on a cluster)"
            ),
            &[ix],
            "keeper",
            &[who],
            VaultError::BondLocked,
        )?;
    }
    let st = status_text(c);
    c.check_true(
        "S05",
        "status shows the bond principal",
        "$100 of bond principal",
        if st.contains("principal open 100.000000") { "$100 of bond principal" } else { "see status" },
        st.contains("principal open 100.000000"),
        None,
    );
    Ok(())
}

// ------------------------------------------------------------------ f, g

pub const CLAIMS: [(&str, u64); 3] = [("trader1", 30_000_000), ("trader2", 70_000_000), ("trader3", 20_000_000)];

pub fn payouts(c: &mut Ctx) -> R<()> {
    let keeper = c.pk("keeper");
    let t1 = c.pk("trader1");
    // refused requests first (they leave nothing behind)
    let over = core_vault::constants::OPEN_CLAIMS_CEILING + 1;
    expect_vault_err(
        c,
        "S06",
        "a payout request of ceiling + 1 base units ($2,500,000.000001) is refused",
        &[sector::request_payout(c, &t1, &keeper, 1, over, 1)],
        "keeper",
        &[],
        VaultError::ClaimsCeilingExceeded,
    )?;
    expect_vault_err(
        c,
        "S06",
        "a payout request with a wrong request id (5, expected 1) is refused",
        &[sector::request_payout(c, &t1, &keeper, 1, 1_000_000, 5)],
        "keeper",
        &[],
        VaultError::RequestIdMismatch,
    )?;
    for (who, amount) in CLAIMS {
        let w = c.pk(who);
        if c.exists(&sector::claim_address(c, &w, 1, 1)) {
            c.record("S06", &format!("{who} claim already queued"), "reused", "reused", true, None);
            continue;
        }
        let t = c.send_ok(&[sector::request_payout(c, &w, &keeper, 1, amount, 1)], "keeper", &[])?;
        note_cu(c, "request_payout", &t);
        c.check_true(
            "S06",
            &format!("mock sector queues a {} claim for {who}", fmt_amount(amount)),
            "claim created",
            "claim created",
            c.exists(&sector::claim_address(c, &w, 1, 1)),
            Some(&t),
        );
    }
    if let Some(vs) = vault_state(c) {
        c.check("S06", "vault counters: open claims", 3, vs.open_claims_count, None);
        c.check(
            "S06",
            "vault counters: open claims total",
            fmt_amount(120_000_000),
            fmt_amount(vs.open_claims_total),
            None,
        );
    }
    let st = status_text(c);
    let headroom = fmt_amount(core_vault::constants::OPEN_CLAIMS_CEILING - 120_000_000);
    c.check_true(
        "S06",
        "status shows the claims and the headroom to the $2,500,000 ceiling",
        &format!("headroom {headroom}"),
        if st.contains(&format!("headroom {headroom}")) { &headroom } else { "see status" },
        st.contains(&format!("headroom {headroom}")),
        None,
    );
    Ok(())
}

fn reconcile_ix(c: &Ctx, caller: &Pubkey) -> Instruction {
    vix(
        c,
        core_vault::accounts::ReconcileProduct {
            caller: *caller,
            product_registry: c.keys.registry(&c.sector),
            payout_tally: sector::tally(c),
        },
        core_vault::instruction::ReconcileProduct { product_program_id: c.sector },
    )
}

pub fn reconcile(c: &mut Ctx) -> R<()> {
    let keeper = c.pk("keeper");
    let t = c.send_ok(&[reconcile_ix(c, &keeper)], "keeper", &[])?;
    note_cu(c, "reconcile_product (match)", &t);
    c.check(
        "S07",
        "reconcile_product with a matching tally leaves the product active",
        true,
        registry(c).map(|r| r.active).unwrap_or(false),
        Some(&t),
    );
    // the deliberately wrong tally
    let r = registry(c).ok_or_else(|| Error("no registry".into()))?;
    let wrong = set_tally_ix(c, r.total_requests_emitted + 1, r.total_requested_amount);
    let t = c.send_ok(&[wrong], "keeper", &[])?;
    let _ = t;
    let t = c.send_ok(&[reconcile_ix(c, &keeper)], "keeper", &[])?;
    note_cu(c, "reconcile_product (mismatch, pauses)", &t);
    let r = registry(c).ok_or_else(|| Error("no registry".into()))?;
    c.check_true(
        "S07",
        "a wrong tally makes reconcile_product auto-pause the product AND return Ok",
        "paused, reason 2 (reconciliation deficit), tx succeeded",
        &format!("active={}, reason={}", r.active, r.pause_reason),
        !r.active && r.pause_reason == core_vault::constants::PAUSE_RECONCILIATION_DEFICIT,
        Some(&t),
    );
    let st = status_text(c);
    c.check_true(
        "S07",
        "status names the reconciliation pause",
        "RECONCILIATION DEFICIT",
        if st.contains("RECONCILIATION DEFICIT") { "RECONCILIATION DEFICIT" } else { "see status" },
        st.contains("RECONCILIATION DEFICIT"),
        None,
    );
    // repair the sector's tally, reactivate through the 2-of-2 ceremony, reconcile again
    let _ = c.send_ok(&[set_tally_ix(c, r.total_requests_emitted, r.total_requested_amount)], "keeper", &[])?;
    let p = s(c.sector);
    let t = ceremony(c, "reactivate_product", &["reactivate-product", "--product", &p])?;
    note_cu(c, "reactivate_product", &t);
    c.check(
        "S07",
        "reactivate_product through the ceremony re-activates the product",
        true,
        registry(c).map(|r| r.active).unwrap_or(false),
        Some(&t),
    );
    let t = c.send_ok(&[reconcile_ix(c, &keeper)], "keeper", &[])?;
    c.check(
        "S07",
        "after the tally is repaired reconcile matches again",
        true,
        registry(c).map(|r| r.active).unwrap_or(false),
        Some(&t),
    );
    Ok(())
}

fn set_tally_ix(c: &Ctx, count: u64, total: u64) -> Instruction {
    sector::set_tally(c, count, total)
}

// ------------------------------------------------------------------ h, i, k

fn settle_ix(c: &Ctx, caller: &Pubkey, claims: &[(&str, u64)]) -> Instruction {
    let mut accounts = core_vault::accounts::SettleClaims {
        caller: *caller,
        vault_state: c.keys.vault(),
        usdc_mint: c.usdc,
        usdt_mint: c.usdt,
        usdc_pool: c.keys.pool(&c.usdc),
        usdt_pool: c.keys.pool(&c.usdt),
        token_program: spl_token::ID,
    }
    .to_account_metas(None);
    for (who, _) in claims {
        let w = c.pk(who);
        accounts.push(AccountMeta::new(sector::claim_address(c, &w, 1, 1), false));
        accounts.push(AccountMeta::new(ata(&w, &c.usdc), false));
        accounts.push(AccountMeta::new(ata(&w, &c.usdt), false));
    }
    Instruction { program_id: c.keys.program_id, accounts, data: core_vault::instruction::SettleClaims {}.data() }
}

fn claim_owed(c: &Ctx, who: &str) -> Option<u64> {
    let w = c.pk(who);
    let (_, _, d) = c.account(&sector::claim_address(c, &w, 1, 1)).ok().flatten()?;
    core_vault::state::PayoutClaim::try_deserialize(&mut d.as_slice()).ok().map(|x| x.owed)
}

fn frozen(c: &Ctx, mint: &Pubkey) -> bool {
    c.token(&c.keys.pool(mint)).map(|t| t.state == AccountState::Frozen).unwrap_or(false)
}

/// Settles one claim and checks the exact payment against the program's own planner.
fn settle_and_check(c: &mut Ctx, step: &str, who: &str, label: &str) -> R<TxInfo> {
    let keeper = c.pk("keeper");
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    let owed = claim_owed(c, who).ok_or_else(|| Error(format!("{who} has no claim")))?;
    let (usdc, usdt) = (c.usdc, c.usdt);
    let w = c.pk(who);
    let (pu0, pt0) = (c.balance(&c.keys.pool(&usdc)), c.balance(&c.keys.pool(&usdt)));
    let (wu0, wt0) = (c.balance(&ata(&w, &usdc)), c.balance(&ata(&w, &usdt)));
    let (fu, ft) = (frozen(c, &usdc), frozen(c, &usdt));
    let (num, den) = cycle_ratio(vs.cycle_available_snapshot, vs.cycle_owed_snapshot);
    let plan =
        plan_settlement_with_frozen(owed, num, den, pu0, fu, pt0, ft).map_err(|_| Error("planner failed".into()))?;
    let t = c.send_ok(&[settle_ix(c, &keeper, &[(who, 1)])], "keeper", &[])?;
    note_cu(c, "settle_claims (1 claim)", &t);
    let (wu1, wt1) = (c.balance(&ata(&w, &usdc)), c.balance(&ata(&w, &usdt)));
    c.check(step, &format!("{label}: USDC paid to {who}"), fmt_amount(plan.from_usdc), fmt_amount(wu1 - wu0), Some(&t));
    c.check(step, &format!("{label}: USDT paid to {who}"), fmt_amount(plan.from_usdt), fmt_amount(wt1 - wt0), None);
    c.check(
        step,
        &format!("{label}: pools lose exactly what was paid"),
        format!("{} / {}", fmt_amount(plan.from_usdc), fmt_amount(plan.from_usdt)),
        format!(
            "{} / {}",
            fmt_amount(pu0 - c.balance(&c.keys.pool(&usdc))),
            fmt_amount(pt0 - c.balance(&c.keys.pool(&usdt)))
        ),
        None,
    );
    let left = owed - plan.total();
    let after = claim_owed(c, who);
    c.check(
        step,
        &format!("{label}: remaining owed on {who}'s claim"),
        if left == 0 { "closed".to_string() } else { fmt_amount(left) },
        after.map(fmt_amount).unwrap_or_else(|| "closed".into()),
        None,
    );
    Ok(t)
}

pub fn heartbeat(c: &mut Ctx) -> R<()> {
    let keeper = c.pk("keeper");
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    if vs.cycle_id > 0 {
        c.record(
            "S08",
            "a heartbeat cycle already ran on this deployment; the 5-day gap cannot be waited out",
            "skipped",
            "skipped (cycle_id > 0)",
            true,
            None,
        );
        return Ok(());
    }
    let (usdc, usdt) = (c.usdc, c.usdt);
    let begin = |c: &Ctx| {
        vix(
            c,
            core_vault::accounts::BeginHeartbeat {
                caller: keeper,
                vault_state: c.keys.vault(),
                usdc_pool: c.keys.pool(&usdc),
                usdt_pool: c.keys.pool(&usdt),
            },
            core_vault::instruction::BeginHeartbeat {},
        )
    };
    let finalize = |c: &Ctx| {
        vix(
            c,
            core_vault::accounts::FinalizeHeartbeat {
                caller: keeper,
                vault_state: c.keys.vault(),
                usdc_pool: c.keys.pool(&usdc),
                usdt_pool: c.keys.pool(&usdt),
            },
            core_vault::instruction::FinalizeHeartbeat {},
        )
    };
    let avail = c.balance(&c.keys.pool(&usdc)) + c.balance(&c.keys.pool(&usdt));
    let t = c.send_ok(&[begin(c)], "keeper", &[])?;
    note_cu(c, "begin_heartbeat", &t);
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    c.check(
        "S08",
        "begin_heartbeat snapshots owed / available / eligible",
        format!("{} / {} / 3", fmt_amount(120_000_000), fmt_amount(avail)),
        format!(
            "{} / {} / {}",
            fmt_amount(vs.cycle_owed_snapshot),
            fmt_amount(vs.cycle_available_snapshot),
            vs.cycle_eligible_count
        ),
        Some(&t),
    );
    expect_vault_err(
        c,
        "S08",
        "a second begin_heartbeat while the cycle is open",
        &[begin(c)],
        "keeper",
        &[],
        VaultError::CycleInProgress,
    )?;

    // claim 1: ordinary settlement
    settle_and_check(c, "S08", "trader1", "ordinary settle")?;

    // i: the issuer freezes the USDC pool inside the open cycle
    let freezer = c.pk("freeze-authority");
    let usdc_pool = c.keys.pool(&usdc);
    let fz = spl_token::instruction::freeze_account(&spl_token::ID, &usdc_pool, &usdc, &freezer, &[])
        .map_err(|e| Error(e.to_string()))?;
    let t = c.send_ok(&[fz], "keeper", &["freeze-authority"])?;
    c.check("S10", "the mint's freeze authority freezes the USDC pool token account", true, frozen(c, &usdc), Some(&t));
    let st = status_text(c);
    c.check_true(
        "S10",
        "status shows the frozen pool",
        "frozen: YES",
        if st.contains("frozen: YES") { "frozen: YES" } else { "see status" },
        st.contains("frozen: YES"),
        None,
    );

    // a deposit into the frozen pool fails cleanly (the other mint works)
    let t3 = c.pk("trader3");
    expect_token_err(
        c,
        "S10",
        "a trader's deposit_fee into the FROZEN USDC pool fails cleanly in the token program",
        &[sector::deposit_fee(c, &t3, &keeper, 2, 50_000_000, 5_000_000_000, &usdc)],
        "keeper",
        &["trader3"],
        17,
        "AccountFrozen",
    )?;
    // admin withdrawal from the frozen pool: the tool's pre-flight refuses ...
    let mut a = vec![
        "plan".to_string(),
        "admin-withdraw".into(),
        "--pool".into(),
        "usdc".into(),
        "--amount".into(),
        "1".into(),
        "--cluster".into(),
        c.cluster.clone(),
        "--out".into(),
        c.work.join("frozen.tx.json").to_string_lossy().to_string(),
    ];
    a.extend(onchain_args(c));
    a.extend(nonce_args(c));
    let (code_, h) = cli(c, a);
    c.check_true(
        "S10",
        "setl8-admin plan refuses a withdrawal from the frozen pool",
        "refused (exit 2, FROZEN)",
        &format!("exit {code_}: {}", h.err.trim()),
        code_ == 2 && h.err.contains("FROZEN"),
        None,
    );
    // ... and a hand-built transaction that bypasses it fails inside the token program
    let wd = AdminIx::Withdraw { pool: Side::Usdc, amount: 1_000_000, mint: usdc }.build(&c.keys);
    let (t, err) = c.send_fail_onchain(&[wd], "sl8-test", &["sl8-test", "rov-test"])?;
    let want = "\"Custom\":17";
    c.check_true(
        "S10",
        "admin_withdraw from the frozen pool (hand-built, no pre-flight) fails in the token program",
        "AccountFrozen = Custom(17)",
        &err,
        err.contains(want),
        Some(&t),
    );

    // k: pause the product through the ceremony (the freeze is still in force); queued claims must still settle
    let p = s(c.sector);
    let t = ceremony(c, "pause_product", &["pause-product", "--product", &p])?;
    note_cu(c, "pause_product", &t);
    let r = registry(c);
    c.check_true(
        "S09",
        "pause_product through the ceremony",
        "paused, reason 1 (planned upgrade)",
        &format!(
            "active={}, reason={}",
            r.as_ref().map(|r| r.active).unwrap_or(true),
            r.as_ref().map(|r| r.pause_reason).unwrap_or(0)
        ),
        r.map(|r| !r.active && r.pause_reason == core_vault::constants::PAUSE_PLANNED_UPGRADE).unwrap_or(false),
        Some(&t),
    );

    // settle the big claim while USDC is frozen: it must not revert and must pay from USDT only
    settle_and_check(c, "S10", "trader2", "settle with USDC frozen")?;
    let carry = claim_owed(c, "trader2");
    c.check_true(
        "S10",
        "the carry-over remains owed after the frozen-pool settlement",
        "claim still open with a remainder",
        &format!("{carry:?}"),
        carry.map(|x| x > 0).unwrap_or(false),
        None,
    );

    // thaw, settle the last claim, finalize
    let th = spl_token::instruction::thaw_account(&spl_token::ID, &usdc_pool, &usdc, &freezer, &[])
        .map_err(|e| Error(e.to_string()))?;
    let t = c.send_ok(&[th], "keeper", &["freeze-authority"])?;
    c.check("S10", "the freeze authority thaws the USDC pool", false, frozen(c, &usdc), Some(&t));
    settle_and_check(c, "S10", "trader3", "settle after the thaw")?;
    let t = c.send_ok(&[finalize(c)], "keeper", &[])?;
    note_cu(c, "finalize_heartbeat", &t);
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    c.check(
        "S08",
        "finalize_heartbeat: USDC floor = floor(25% of the real balance)",
        fmt_amount(c.balance(&c.keys.pool(&usdc)) / 4),
        fmt_amount(vs.usdc_floor),
        Some(&t),
    );
    c.check(
        "S08",
        "finalize_heartbeat: USDT floor = floor(25% of the real balance)",
        fmt_amount(c.balance(&c.keys.pool(&usdt)) / 4),
        fmt_amount(vs.usdt_floor),
        None,
    );
    c.check("S08", "the cycle is closed", false, vs.cycle_active, None);
    c.check("S08", "open claims after the cycle: one carry-over claim", 1, vs.open_claims_count, None);
    expect_vault_err(
        c,
        "S08",
        "begin_heartbeat again right away (432,000 s gap cannot be waited out)",
        &[begin(c)],
        "keeper",
        &[],
        VaultError::HeartbeatTooEarly,
    )?;

    let t = ceremony(c, "reactivate_product_2", &["reactivate-product", "--product", &p])?;
    c.check(
        "S09",
        "reactivate_product through the ceremony",
        true,
        registry(c).map(|r| r.active).unwrap_or(false),
        Some(&t),
    );
    Ok(())
}

// ------------------------------------------------------------------ j, l, m

pub fn withdraw(c: &mut Ctx) -> R<()> {
    let usdc = c.usdc;
    let pool = c.keys.pool(&usdc);
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    let live = c.balance(&pool);
    let max = core_vault::utils::withdrawable(live, vs.usdc_floor).map_err(|_| Error("reserve math".into()))?;
    let reserve = live - max;
    c.record(
        "S11",
        &format!(
            "USDC pool {} , floor {} , reserve {} , withdrawable {}",
            fmt_amount(live),
            fmt_amount(vs.usdc_floor),
            fmt_amount(reserve),
            fmt_amount(max)
        ),
        "-",
        "-",
        true,
        None,
    );
    if max < 4 {
        c.record("S11", "not enough in the pool to test a withdrawal", "skipped", "skipped", true, None);
        return Ok(());
    }
    // the exact boundary through the tool's pre-flight
    let over = fmt_amount(max + 1).replace(',', "");
    let mut a = vec![
        "plan".to_string(),
        "admin-withdraw".into(),
        "--pool".into(),
        "usdc".into(),
        "--amount".into(),
        over,
        "--cluster".into(),
        c.cluster.clone(),
        "--out".into(),
        c.work.join("over.tx.json").to_string_lossy().to_string(),
    ];
    a.extend(onchain_args(c));
    a.extend(nonce_args(c));
    let (code_, h) = cli(c, a);
    c.check_true(
        "S11",
        "plan pre-flight refuses max + 1 base unit",
        "refused (exit 2) naming the maximum",
        &format!("exit {code_}: {}", h.err.trim().chars().take(160).collect::<String>()),
        code_ == 2 && h.err.contains(&format!("at most {} can be withdrawn", fmt_amount(max))),
        None,
    );
    // the program itself refuses it too (hand-built, no pre-flight)
    let ix = AdminIx::Withdraw { pool: Side::Usdc, amount: max + 1, mint: usdc }.build(&c.keys);
    let (t, err) = c.send_fail_onchain(&[ix], "sl8-test", &["sl8-test", "rov-test"])?;
    let want = format!("\"Custom\":{}", code(VaultError::WithdrawalExceedsReserve));
    c.check_true(
        "S11",
        "the program refuses max + 1 base unit even when the pre-flight is bypassed",
        "WithdrawalExceedsReserve",
        &err,
        err.contains(&want),
        Some(&t),
    );
    // a withdrawal that succeeds: exactly the maximum
    let (p0, s0) = pool_sl8_snapshot(c, &usdc);
    let amt = fmt_amount(max).replace(',', "");
    let t = ceremony(c, "admin_withdraw", &["admin-withdraw", "--pool", "usdc", "--amount", &amt])?;
    note_cu(c, "admin_withdraw_marketing_funds", &t);
    let (p1, s1) = pool_sl8_snapshot(c, &usdc);
    c.check(
        "S11",
        "admin_withdraw of exactly the maximum: pool down by the amount",
        fmt_amount(max),
        fmt_amount(p0 - p1),
        Some(&t),
    );
    c.check("S11", "admin_withdraw: SL8's token account up by the amount", fmt_amount(max), fmt_amount(s1 - s0), None);
    c.check("S11", "admin_withdraw: the pool keeps exactly its reserve", fmt_amount(reserve), fmt_amount(p1), None);
    c.check(
        "S11",
        "admin_withdraw: marketing_withdrawn_usdc",
        fmt_amount(vs.marketing_withdrawn_usdc + max),
        fmt_amount(vault_state(c).map(|v| v.marketing_withdrawn_usdc).unwrap_or(0)),
        None,
    );
    let _ = parse_amount;
    Ok(())
}

pub fn revoke(c: &mut Ctx) -> R<()> {
    // a valid, fully signed, UNSENT transaction ...
    let p = s(c.sector);
    let file = c.work.join("revoke.tx.json").to_string_lossy().to_string();
    let common = |c: &Ctx| {
        let mut v = admin_args(c);
        v.extend(nonce_args(c));
        v
    };
    let mut a = vec![
        "plan".to_string(),
        "pause-product".into(),
        "--product".into(),
        p.clone(),
        "--cluster".into(),
        c.cluster.clone(),
        "--out".into(),
        file.clone(),
    ];
    a.extend(onchain_args(c));
    a.extend(nonce_args(c));
    let (code_, h) = cli(c, a);
    if code_ != 0 {
        return Err(Error(format!("plan failed: {}{}", h.out, h.err)));
    }
    for key in ["sl8-test", "rov-test"] {
        let mut a = vec!["sign".to_string(), file.clone(), "--keypair".into(), keyfile(c, key)];
        a.extend(common(c));
        let (code_, h) = cli(c, a);
        if code_ != 0 {
            return Err(Error(format!("sign failed: {}{}", h.out, h.err)));
        }
    }
    // ... is revoked by nonce-advance
    let mut a = vec![
        "nonce-advance".to_string(),
        "--cluster".into(),
        c.cluster.clone(),
        "--keypair".into(),
        keyfile(c, "sl8-test"),
        "--rpc".into(),
        c.url.clone(),
    ];
    a.extend(nonce_args(c));
    let (code_, h) = cli(c, a);
    c.check_true(
        "S12",
        "nonce-advance (SL8 key, the nonce authority)",
        "confirmed",
        if code_ == 0 { "confirmed" } else { &h.err },
        code_ == 0,
        None,
    );
    // the tool refuses to send it
    let mut a =
        vec!["send".to_string(), file.clone(), "--cluster".into(), c.cluster.clone(), "--rpc".into(), c.url.clone()];
    a.extend(common(c));
    let (code_, h) = cli(c, a);
    c.check_true(
        "S12",
        "setl8-admin send of the previously signed tx after nonce-advance",
        "refused (simulation fails)",
        &format!("exit {code_}: {}", h.err.trim().chars().take(200).collect::<String>()),
        code_ == 2 && h.err.contains("simulation failed"),
        None,
    );
    // and the cluster itself rejects the raw bytes
    let f = TxFile::load(std::path::Path::new(&file))?;
    let msg = f.message_bytes()?;
    let sigs: Vec<solana_signature::Signature> = f.signatures.iter().filter_map(|e| e.signature.parse().ok()).collect();
    let wire = setl8_admin::send::wire_transaction(&sigs, &msg);
    let r = c.rpc.call(
        "sendTransaction",
        json!([base64::engine::general_purpose::STANDARD.encode(&wire), {"encoding": "base64"}]),
    );
    let (ok, txt) = match r {
        Err(e) => (true, format!("rejected by the cluster: {} {}", e.message, e.data["err"])),
        Ok(v) => {
            let sig = v.as_str().unwrap_or("").to_string();
            match c.rpc.call("getSignatureStatuses", json!([[sig]])) {
                Ok(st) if st["value"][0].is_null() => (true, "accepted by the RPC but never landed".to_string()),
                _ => (false, "LANDED (the revocation failed!)".to_string()),
            }
        }
    };
    c.check_true("S12", "the same bytes sent raw to the cluster after nonce-advance", "rejected", &txt, ok, None);
    Ok(())
}

pub fn tool_safety(c: &mut Ctx) -> R<()> {
    let p = s(c.sector);
    let file = c.work.join("safety.tx.json").to_string_lossy().to_string();
    let mk = |c: &Ctx| -> Vec<String> {
        let mut a = vec![
            "plan".to_string(),
            "pause-product".into(),
            "--product".into(),
            p.clone(),
            "--cluster".into(),
            c.cluster.clone(),
            "--out".into(),
            file.clone(),
        ];
        a.extend(onchain_args(c));
        a.extend(nonce_args(c));
        a
    };
    let mut common = admin_args(c);
    common.extend(nonce_args(c));
    let (code_, h) = cli(c, mk(c));
    if code_ != 0 {
        return Err(Error(format!("plan failed: {}{}", h.out, h.err)));
    }
    // wrong key
    let mut a = vec!["sign".to_string(), file.clone(), "--keypair".into(), keyfile(c, "trader1")];
    a.extend(common.clone());
    let (code_, h) = cli(c, a);
    c.check_true(
        "S13",
        "sign with a key that is not a required signer",
        "refused (exit 2)",
        &format!("exit {code_}: {}", h.err.trim().chars().take(120).collect::<String>()),
        code_ == 2 && h.err.contains("required signers"),
        None,
    );
    // one signature only
    let mut a = vec!["sign".to_string(), file.clone(), "--keypair".into(), keyfile(c, "sl8-test")];
    a.extend(common.clone());
    let (code_, _) = cli(c, a);
    let mut a =
        vec!["send".to_string(), file.clone(), "--cluster".into(), c.cluster.clone(), "--rpc".into(), c.url.clone()];
    a.extend(common.clone());
    let (code2, h) = cli(c, a);
    c.check_true(
        "S13",
        "send with only one of the two signatures",
        "refused (exit 2)",
        &format!("sign exit {code_}; send exit {code2}: {}", h.err.trim().chars().take(120).collect::<String>()),
        code_ == 0 && code2 == 2 && h.err.contains("missing or invalid"),
        None,
    );
    // tamper one byte after the first signature
    let mut f = TxFile::load(std::path::Path::new(&file))?;
    let mut bytes = f.message_bytes()?;
    *bytes.last_mut().unwrap() ^= 1;
    f.message_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    f.message_sha256 = setl8_admin::txfile::sha256_hex(&bytes);
    let tampered = c.work.join("tampered.tx.json");
    f.save(&tampered)?;
    let mut a =
        vec!["sign".to_string(), tampered.to_string_lossy().to_string(), "--keypair".into(), keyfile(c, "rov-test")];
    a.extend(common.clone());
    let (code_, h) = cli(c, a);
    c.check_true(
        "S13",
        "the second signer refuses a message changed after the first signature",
        "refused (exit 2, signature no longer verifies)",
        &format!("exit {code_}"),
        code_ == 2 && h.out.contains("does not verify against this message"),
        None,
    );
    // a transaction built for another cluster
    let other = c.work.join("othercluster.tx.json").to_string_lossy().to_string();
    let bh = c.latest_blockhash()?.to_string();
    let mint = s(c.usdc);
    let mut a: Vec<String> = [
        "plan",
        "admin-withdraw",
        "--pool",
        "usdc",
        "--amount",
        "1",
        "--mint",
        &mint,
        "--cluster",
        "localnet",
        "--genesis-hash",
        "NotThisCluster1111111111111111111111111111",
        "--offline",
        "--recent-blockhash",
        &bh,
        "--out",
        &other,
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    a.extend(admin_args(c));
    let (code_, h) = cli(c, a);
    if code_ != 0 {
        return Err(Error(format!("offline plan failed: {}{}", h.out, h.err)));
    }
    for key in ["sl8-test", "rov-test"] {
        let mut a = vec!["sign".to_string(), other.clone(), "--keypair".into(), keyfile(c, key)];
        a.extend(admin_args(c));
        let (code_, h) = cli(c, a);
        if code_ != 0 {
            return Err(Error(format!("sign failed: {}{}", h.out, h.err)));
        }
    }
    let mut a = vec!["send".to_string(), other, "--cluster".into(), c.cluster.clone(), "--rpc".into(), c.url.clone()];
    a.extend(admin_args(c));
    let (code_, h) = cli(c, a);
    c.check_true(
        "S13",
        "a fully signed transaction bound to another cluster is refused by genesis hash",
        "refused (exit 2) naming both clusters",
        &format!("exit {code_}: {}", h.err.trim().chars().take(160).collect::<String>()),
        code_ == 2 && h.err.contains("bound to"),
        None,
    );
    Ok(())
}

// ------------------------------------------------------------------ the end

/// Documented figures (docs/SECURITY-REVIEW.md section 4); measured = the vault program's own consumption.
pub fn cu_table(c: &Ctx) -> String {
    let doc: &[(&str, u64)] = &[
        ("init_vault", 33_834),
        ("register_product (32 tiers, 8 phases)", 12_002),
        ("pause_product", 4_716),
        ("reactivate_product", 4_718),
        ("deposit_fee (usdc-mint)", 35_188),
        ("deposit_fee (usdt-mint)", 35_188),
        ("deposit_bond", 36_371),
        ("request_payout", 18_213),
        ("reconcile_product (match)", 9_149),
        ("reconcile_product (mismatch, pauses)", 10_293),
        ("begin_heartbeat", 5_670),
        ("settle_claims (1 claim)", 24_129),
        ("finalize_heartbeat", 5_645),
        ("admin_withdraw_marketing_funds", 16_953),
    ];
    let mut out = String::from("| instruction | documented CU (LiteSVM) | measured on the cluster (vault program only) | whole-tx CU | difference |\n|---|---:|---:|---:|---|\n");
    let mut seen = std::collections::BTreeSet::new();
    for (name, v, total) in &c.cus {
        if !seen.insert(name.clone()) {
            continue;
        }
        let d = doc.iter().find(|(n, _)| n == name).map(|x| x.1);
        let diff = d
            .map(|d| {
                format!(
                    "{:+.1}%{}",
                    (*v as f64 - d as f64) * 100.0 / d as f64,
                    if ((*v as f64 - d as f64).abs() * 100.0 / d as f64) > 10.0 { " **>10%**" } else { "" }
                )
            })
            .unwrap_or_else(|| "n/a".into());
        out.push_str(&format!(
            "| {name} | {} | {v} | {total} | {diff} |\n",
            d.map(|d| d.to_string()).unwrap_or_else(|| "-".into())
        ));
    }
    out
}

pub fn results_markdown(c: &Ctx) -> String {
    let mut o = String::from("| step | check | expected | actual | result | signature | CU | fee (lamports) | tx bytes | ms |\n|---|---|---|---|---|---|---:|---:|---:|---:|\n");
    for r in &c.rows {
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.step,
            r.what.replace('|', "/"),
            r.expected.replace('|', "/"),
            r.actual.replace('|', "/").chars().take(160).collect::<String>(),
            if r.ok { "PASS" } else { "**FAIL**" },
            if r.sig.is_empty() { String::new() } else { format!("`{}`", &r.sig[..r.sig.len().min(20)]) },
            r.cu.map(|x| x.to_string()).unwrap_or_default(),
            r.fee.map(|x| x.to_string()).unwrap_or_default(),
            if r.size == 0 { String::new() } else { r.size.to_string() },
            if r.ms == 0 { String::new() } else { r.ms.to_string() }
        ));
    }
    o
}

/// `--extra-claims N`: N more traders, generated in memory (their keys are never written anywhere),
/// each with token accounts, a $50 challenge (alternating USDC / USDT) and one queued claim. Makes a queue
/// long enough for more than one settlement batch.
pub fn extra_claims(c: &mut Ctx) -> R<()> {
    let n = c.extra_claims;
    if n == 0 {
        return Ok(());
    }
    let auth = c.pk("freeze-authority");
    let payer = c.pk("keeper");
    for i in 0..n {
        let name = format!("extra{i}");
        let kp = solana_keypair::Keypair::new();
        let w = solana_signer::Signer::pubkey(&kp);
        if c.exists(&sector::trader_state(c, &w, 1)) {
            continue;
        }
        c.kp.insert(name.clone(), kp);
        let (usdc, usdt) = (c.usdc, c.usdt);
        let mint = if i % 2 == 0 { usdc } else { usdt };
        let mut ixs = vec![ata_ix(c, &payer, &w, &usdc), ata_ix(c, &payer, &w, &usdt)];
        ixs.push(
            spl_token::instruction::mint_to(&spl_token::ID, &mint, &ata(&w, &mint), &auth, &[], 100_000_000)
                .map_err(|e| Error(e.to_string()))?,
        );
        c.send_ok(&ixs, "keeper", &["freeze-authority"])?;
        let (size, cost) = (5_000_000_000u64, 50_000_000u64);
        let t = c.send_ok(&[sector::deposit_fee(c, &w, &payer, 1, cost, size, &mint)], "keeper", &[name.as_str()])?;
        let amount = (5 + i as u64) * 1_000_000;
        let t2 = c.send_ok(&[sector::request_payout(c, &w, &payer, 1, amount, 1)], "keeper", &[])?;
        c.record(
            "S06x",
            &format!("extra trader {i}: bought a challenge and a {} claim is queued", fmt_amount(amount)),
            "queued",
            "queued",
            true,
            Some(&t2),
        );
        let _ = t;
    }
    let vs = vault_state(c).ok_or_else(|| Error("no vault".into()))?;
    c.record("S06x", &format!("claims in the queue: {}", vs.open_claims_count), "-", "-", true, None);
    Ok(())
}
