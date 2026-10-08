//! Compute-unit table: every instruction, measured on the real program in its
//! heaviest ordinary configuration, with a ceiling asserted for each (a regression
//! guard for the figures in docs/SECURITY-REVIEW.md).
//!
//! The ceilings are the 200,000 CU default per-instruction limit, except
//! `settle_claims` (a full batch needs the 400,000 limit the keeper documents).
//! Every PDA found with `find_program_address` costs ~1,500 CU per bump tried: a
//! caller that grinds a key to need more tries only makes its OWN transaction dearer,
//! except for `settle_claims`, where a claimant's wallet decides the ATA derivation
//! cost (see settle_batch.rs for that worst case).
mod common;
use common::*;
use core_vault::state::BondTerm;
use solana_signer::Signer;

const DEFAULT_LIMIT: u64 = 200_000;
const M: u64 = 1_000_000;

fn big_cfg() -> Cfg {
    // the largest registry the account can hold: 32 tiers, 8 reset phases
    Cfg {
        fee_split_bps: 6_500,
        tiers: (0..32u64).map(|i| ChallengeSize { size: 10_000 + i, cost: 100 + i }).collect(),
        max_payout: 5,
        reset_bps: vec![100; 8],
    }
}

#[test]
fn every_instruction_fits_the_compute_budget() {
    let mut rows: Vec<(&str, u64, u64)> = vec![];
    let mut add = |name: &'static str, cu: u64, limit: u64| {
        assert!(cu < limit, "{name}: {cu} CU is not below its ceiling {limit}");
        rows.push((name, cu, limit));
    };

    // init_vault
    let mut e = Env::new_bare();
    let ix = init_vault_ix(&e, e.usdc, e.usdt);
    add("init_vault", e.ok(ix).compute_units_consumed, DEFAULT_LIMIT);

    // register_product / update_product_config with the largest registry
    let s = Sector::new();
    let m = e.ok(register_ix(&e, &s, &big_cfg()));
    add("register_product (32 tiers, 8 phases)", m.compute_units_consumed, DEFAULT_LIMIT);
    let m = e.ok(update_ix(&e, &s, &big_cfg()));
    add("update_product_config (32 tiers, 8 phases)", m.compute_units_consumed, DEFAULT_LIMIT);
    e.update(&s, &Cfg::default());

    // pause / reactivate
    let m = e.ok(pause_ix(&e, &s));
    add("pause_product", m.compute_units_consumed, DEFAULT_LIMIT);
    let m = e.ok(reactivate_ix(&e, &s));
    add("reactivate_product", m.compute_units_consumed, DEFAULT_LIMIT);

    // deposit_fee (both legs), record_activity, request_payout, flag, reset
    let (w1, w2, w3) = (wallet(), wallet(), wallet());
    let m = e.deposit_tier(&s, &w1, 1, TIER_A);
    add("deposit_fee", m.compute_units_consumed, DEFAULT_LIMIT);
    e.advance(2 * DAY);
    let m = e.record(&s, &w1, 1);
    add("record_activity (refresh)", m.compute_units_consumed, DEFAULT_LIMIT);
    let m = e.payout(&s, &w1, 1, 1_000, 1);
    add("request_payout (creates the claim)", m.compute_units_consumed, DEFAULT_LIMIT);
    e.deposit(&s, &w2, 1);
    let ix = flag_ix(&s, &w2, 1);
    add("flag_trader_failed", e.ok(ix).compute_units_consumed, DEFAULT_LIMIT);
    let fund = e.fund_wallet(&w2);
    let _ = fund;
    let ix = reset_ix(&e, &s, &w2, 1, 2, 100, 0);
    add("deposit_reset", e.ok(ix).compute_units_consumed, DEFAULT_LIMIT);
    e.deposit(&s, &w3, 1);
    e.advance(8 * DAY);
    let caller = e.new_caller();
    let ix = abandon_ix(&caller.pubkey(), &s, &w3, 1);
    let fp = dup(&e.payer);
    let m = assert_ok(e.send_with(&[ix], &fp, &[&caller]));
    add("mark_abandoned", m.compute_units_consumed, DEFAULT_LIMIT);

    // reconcile (matching tally, then a mismatch that pauses)
    let reg = e.registry(&s);
    e.set_tally(&s, reg.total_requests_emitted, reg.total_requested_amount);
    add("reconcile_product (match)", e.reconcile(&s).compute_units_consumed, DEFAULT_LIMIT);
    e.set_tally(&s, 99, 99);
    add("reconcile_product (mismatch, pauses)", e.reconcile(&s).compute_units_consumed, DEFAULT_LIMIT);
    e.resume(&s);

    // heartbeat: begin, settle (1 and a full batch of 6), finalize
    let mut ts = vec![];
    for _ in 0..6 {
        let w = wallet();
        e.deposit(&s, &w, 1);
        e.payout(&s, &w, 1, 10 * M, 1);
        e.make_atas(&w);
        ts.push(triple(&e, &s, &w, 1, 1));
    }
    e.make_atas(&w1);
    ts.insert(0, triple(&e, &s, &w1, 1, 1));
    e.set_pool(Coin::Usdc, 70 * M);
    e.set_pool(Coin::Usdt, 20 * M);
    add("begin_heartbeat", e.begin().compute_units_consumed, DEFAULT_LIMIT);
    let m = e.settle(&ts[..1]);
    add("settle_claims (1 claim)", m.compute_units_consumed, DEFAULT_LIMIT);
    let caller = e.new_caller();
    let ixs = [compute_limit_ix(400_000), settle_ix(&caller.pubkey(), &e, &ts[1..])];
    let fp = dup(&e.payer);
    let m = assert_ok(e.send_with(&ixs, &fp, &[&caller]));
    add("settle_claims (6 claims, both pools)", m.compute_units_consumed, 400_000);
    add("finalize_heartbeat", e.finalize().compute_units_consumed, DEFAULT_LIMIT);

    // bonds: first deposit (creates position + tracker), second, withdrawal
    let k = e.new_depositor();
    let m = e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    add("deposit_bond (first: creates tracker + position)", m.compute_units_consumed, DEFAULT_LIMIT);
    let m = e.bond_deposit(&k, 1, 100 * M, BondTerm::NineMonths, Coin::Usdt);
    add("deposit_bond (later)", m.compute_units_consumed, DEFAULT_LIMIT);
    e.advance(181 * DAY);
    let m = e.bond_request(&k, 0);
    add("request_bond_payout (matured)", m.compute_units_consumed, DEFAULT_LIMIT);

    // admin withdrawal
    let m = e.withdraw(Coin::Usdc, 1);
    add("admin_withdraw_marketing_funds", m.compute_units_consumed, DEFAULT_LIMIT);

    println!("\n{:<52} {:>9}  ceiling", "instruction (measured case)", "CU");
    for (n, cu, lim) in &rows {
        println!("{n:<52} {cu:>9}  {lim}");
    }
    assert_eq!(rows.len(), 21, "one row per measured case");
}
