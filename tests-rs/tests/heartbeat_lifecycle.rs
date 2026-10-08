//! The whole money path with real instructions, from a purchase to a carried-over
//! payout, checking balances, conservation and the claim invariant at every step.
mod common;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::constants::{BOND_6M_LOCK_SECS, BOND_6M_TERM_SECS, BOND_9M_LOCK_SECS, BOND_9M_TERM_SECS, BOND_MAX_PER_WALLET};
use core_vault::state::BondTerm;
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::collections::BTreeMap;

use anchor_lang::solana_program::pubkey::Pubkey;

const M: u64 = 1_000_000;

#[test]
fn deposit_request_begin_settle_finalize_then_a_second_cycle_clears_the_carry_over() {
    let cfg = Cfg {
        fee_split_bps: 6500,
        tiers: vec![ChallengeSize { size: 100 * M, cost: 20 * M }],
        max_payout: 5,
        reset_bps: vec![],
    };
    let (mut e, s) = Env::registered(&cfg);
    let (a, b, c) = (wallet(), wallet(), wallet());
    for w in [&a, &b, &c] {
        e.fund_wallet(w);
        e.make_atas(w);
    }
    let (usdc_pool, usdt_pool, sl8_usdc, sl8_usdt) = (e.usdc_pool, e.usdt_pool, e.sl8_usdc, e.sl8_usdt);
    let totals = e.totals();

    // model of every tracked balance; `step` applies signed changes, then compares with the chain
    let mut model: BTreeMap<Pubkey, u64> = e.token_snapshot();
    let mut step = |e: &mut Env, label: &str, changes: &[(Pubkey, i128)]| {
        for (addr, d) in changes {
            let v = model.get_mut(addr).unwrap_or_else(|| panic!("{label}: untracked {addr}"));
            *v = (*v as i128 + d) as u64;
        }
        assert_eq!(&e.token_snapshot(), &model, "balances after: {label}");
        assert_eq!(e.totals(), totals, "conservation after: {label}");
        e.assert_claim_invariant();
    };
    let wa = e.wallet_tok(&a);
    let wb = e.wallet_tok(&b);
    let (a_usdc, a_usdt) = (ata(&a, &e.usdc), ata(&a, &e.usdt));
    let (b_usdc, b_usdt) = (ata(&b, &e.usdc), ata(&b, &e.usdt));

    // 1. A buys with USDC, B buys with USDT: 13 to each pool, 7 to SL8
    e.deposit_coin(&s, &a, 1, (100 * M, 20 * M), Coin::Usdc);
    step(&mut e, "A buys", &[(wa.usdc, -(20 * M as i128)), (usdc_pool, 13 * M as i128), (sl8_usdc, 7 * M as i128)]);
    e.deposit_coin(&s, &b, 1, (100 * M, 20 * M), Coin::Usdt);
    step(&mut e, "B buys", &[(wb.usdt, -(20 * M as i128)), (usdt_pool, 13 * M as i128), (sl8_usdt, 7 * M as i128)]);
    assert_eq!(e.pools(), (13 * M, 13 * M));

    // 2. three requests: A 12 and 8, B 12. NO tokens move.
    e.payout(&s, &a, 1, 12 * M, 1);
    step(&mut e, "A requests 12", &[]);
    e.payout(&s, &a, 1, 8 * M, 2);
    step(&mut e, "A requests 8", &[]);
    e.payout(&s, &b, 1, 12 * M, 1);
    step(&mut e, "B requests 12", &[]);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (3, 32 * M));

    // 3. begin: 32 owed, 26 available => ratio 26/32
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_owed_snapshot, vs.cycle_available_snapshot, vs.cycle_eligible_count), (32 * M, 26 * M, 3));
    step(&mut e, "begin", &[]);

    // 4. settle all three in one call:
    //  A#1 9.75 : pools tie 13/13 -> USDC first          -> USDC 3.25 / USDT 13
    //  A#2 6.5  : USDT larger                            -> USDC 3.25 / USDT 6.5
    //  B#1 9.75 : USDT larger (6.5), USDC tops up (3.25) -> 0 / 0
    let t = [triple(&e, &s, &a, 1, 1), triple(&e, &s, &a, 1, 2), triple(&e, &s, &b, 1, 1)];
    e.settle(&t);
    step(
        &mut e,
        "settle",
        &[
            (usdc_pool, -(13 * M as i128)),
            (usdt_pool, -(13 * M as i128)),
            (a_usdc, 9_750_000),
            (a_usdt, 6_500_000),
            (b_usdc, 3_250_000),
            (b_usdt, 6_500_000),
        ],
    );
    assert_eq!(e.pools(), (0, 0));
    assert_eq!(e.claim(&s, &a, 1, 1).owed, 2_250_000);
    assert_eq!(e.claim(&s, &a, 1, 2).owed, 1_500_000);
    assert_eq!(e.claim(&s, &b, 1, 1).owed, 2_250_000);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (3, 6 * M));

    // 5. finalize: both pools empty => both floors 0
    e.finalize();
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor, vs.cycle_active), (0, 0, false));
    step(&mut e, "finalize", &[]);

    // 6. a third trader buys with USDC (13 into the pool), 5 days later: cycle two clears the carry-over
    e.advance(GAP);
    e.deposit_coin(&s, &c, 1, (100 * M, 20 * M), Coin::Usdc);
    let wc = e.wallet_tok(&c);
    step(&mut e, "C buys", &[(wc.usdc, -(20 * M as i128)), (usdc_pool, 13 * M as i128), (sl8_usdc, 7 * M as i128)]);
    e.begin();
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_owed_snapshot, vs.cycle_available_snapshot), (2, 6 * M, 13 * M));
    e.settle(&t); // ratio 1: everything still owed is paid, all from USDC (the only funded pool)
    step(
        &mut e,
        "settle 2",
        &[(usdc_pool, -(6 * M as i128)), (a_usdc, 3_750_000), (b_usdc, 2_250_000)],
    );
    for (w, req) in [(&a, 1), (&a, 2), (&b, 1)] {
        assert!(e.claim_opt(&s, w, 1, req).is_none(), "claim fully paid and closed");
    }
    assert_eq!((e.vault_state().open_claims_count, e.vault_state().open_claims_total), (0, 0));
    e.finalize();
    // 13 - 6 = 7 left in USDC; floor 25%
    let vs = e.vault_state();
    assert_eq!((vs.usdc_floor, vs.usdt_floor), (1_750_000, 0));

    // each trader received exactly what was requested, in total
    assert_eq!(e.token_balance(&a_usdc) + e.token_balance(&a_usdt), 20 * M);
    assert_eq!(e.token_balance(&b_usdc) + e.token_balance(&b_usdt), 12 * M);
    // SL8 only ever received its 35% fee shares
    assert_eq!((e.token_balance(&sl8_usdc), e.token_balance(&sl8_usdt)), (14 * M, 7 * M));
}

// ------------------------------------------------------------------ with bonds

#[test]
fn a_bond_and_a_trader_payout_share_one_cycle_end_to_end() {
    let cfg = Cfg {
        fee_split_bps: 6500,
        tiers: vec![ChallengeSize { size: 100 * M, cost: 20 * M }],
        max_payout: 5,
        reset_bps: vec![],
    };
    let (mut e, s) = Env::registered(&cfg);
    let trader = wallet();
    e.fund_wallet(&trader);
    e.make_atas(&trader);
    let alice = e.new_depositor();
    let a = alice.pubkey();
    e.make_atas(&a);
    let (usdc_pool, sl8_usdc) = (e.usdc_pool, e.sl8_usdc);
    let totals = e.totals();

    // The trader buys a $20 challenge (pool +13, SL8 +7) and asks for 12.
    e.deposit_coin(&s, &trader, 1, (100 * M, 20 * M), Coin::Usdc);
    e.payout(&s, &trader, 1, 12 * M, 1);
    // Alice locks $1,000 for six months: pays 1,002.000000; pool +500; SL8 +502.
    e.bond_deposit(&alice, 0, 1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.token_balance(&usdc_pool), 13 * M + 500 * M);
    assert_eq!(e.token_balance(&sl8_usdc), 7 * M + 502 * M);
    assert_eq!(e.vault_state().bond_principal_open_total, 1_000 * M);

    // At maturity she withdraws: gross 1,200, fee 2.4, claim 1,197.6. No tokens move.
    e.set_time(T0 + BOND_6M_TERM_SECS);
    e.bond_request(&alice, 0);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (2, 12 * M + 1_197_600_000));
    assert_eq!(vs.bond_withdrawal_fees_retained, 2_400_000);
    assert_eq!(vs.bond_principal_open_total, 0);
    assert_eq!(e.totals(), totals);
    e.assert_bond_invariant();
    e.assert_claim_invariant();

    // One cycle pays both from a pool of 513: ratio 513 / 1,209.6, same for each.
    e.begin();
    let owed = 12 * M + 1_197_600_000;
    assert_eq!(e.vault_state().cycle_owed_snapshot, owed);
    assert_eq!(e.vault_state().cycle_available_snapshot, 513 * M);
    let (tt, at) = (triple(&e, &s, &trader, 1, 1), (bond_claim_pda(&a, 0).0, ata(&a, &e.usdc), ata(&a, &e.usdt)));
    e.settle(&[tt, at]);
    let pay_t = (12 * M as u128 * (513 * M) as u128 / owed as u128) as u64;
    let pay_a = (1_197_600_000u128 * (513 * M) as u128 / owed as u128) as u64;
    assert_eq!(e.token_balance(&tt.1), pay_t);
    assert_eq!(e.token_balance(&at.1), pay_a);
    assert_eq!(e.claim(&s, &trader, 1, 1).owed, 12 * M - pay_t);
    assert_eq!(e.bond_claim_opt(&a, 0).unwrap().owed, 1_197_600_000 - pay_a);
    assert_eq!(e.totals(), totals);
    e.assert_claim_invariant();
    e.finalize();
    let vs = e.vault_state();
    assert_eq!(vs.usdc_floor, e.token_balance(&usdc_pool) * 2_500 / 10_000);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn fee_of(x: u64) -> u64 {
    ((x as u128 * 20 + 9_999) / 10_000) as u64
}

/// A seeded random run of deposits, withdrawal requests and full heartbeats. After
/// EVERY step: all tokens are conserved, the pools equal what deposits put in minus
/// what settlement paid out, retained fees and claims reconcile to the unit, and the
/// vault's counters equal what is recomputed from the accounts. Payments are checked
/// against an independent model of the cycle.
#[test]
fn seeded_bonds_requests_and_heartbeats_conserve_every_token_and_reconcile() {
    let mut e = Env::new();
    let n = 10usize;
    let ks: Vec<Keypair> = (0..n).map(|_| e.new_depositor()).collect();
    for k in &ks {
        e.make_atas(&k.pubkey());
    }
    let totals = e.totals();
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);

    struct Open {
        w: usize,
        idx: u64,
        principal: u64,
        term: BondTerm,
        created: i64,
    }
    let (mut open, mut next, mut wallet_open): (Vec<Open>, Vec<u64>, Vec<u64>) = (vec![], vec![0; n], vec![0; n]);
    let (mut pool_credit, mut sl8_credit) = (0u128, 0u128);
    let (mut fees, mut nets) = (0u128, 0u128);
    let mut last_start: i64 = i64::MIN / 2;
    let (mut deposits, mut requests, mut cycles) = (0, 0, 0);

    for _ in 0..80 {
        e.advance((rng.next() % 25) as i64 * 86_400 + (rng.next() % 86_400) as i64);
        match rng.next() % 4 {
            0 | 1 => {
                let w = (rng.next() % n as u64) as usize;
                let principal = 50 * M + rng.next() % (9_000 * M);
                if wallet_open[w] + principal > BOND_MAX_PER_WALLET {
                    continue;
                }
                let term = if rng.next() % 2 == 0 { BondTerm::SixMonths } else { BondTerm::NineMonths };
                let coin = if rng.next() % 2 == 0 { Coin::Usdc } else { Coin::Usdt };
                e.bond_deposit(&ks[w], next[w], principal, term, coin);
                open.push(Open { w, idx: next[w], principal, term, created: e.now() });
                next[w] += 1;
                wallet_open[w] += principal;
                pool_credit += (principal / 2) as u128;
                sl8_credit += (principal - principal / 2 + fee_of(principal)) as u128;
                deposits += 1;
            }
            2 => {
                let ready: Vec<usize> = (0..open.len())
                    .filter(|&i| e.now() >= open[i].created + if matches!(open[i].term, BondTerm::SixMonths) { BOND_6M_LOCK_SECS } else { BOND_9M_LOCK_SECS })
                    .collect();
                if ready.is_empty() {
                    continue;
                }
                let o = open.remove(ready[(rng.next() % ready.len() as u64) as usize]);
                let (full, bps) = match o.term {
                    BondTerm::SixMonths => (BOND_6M_TERM_SECS, 2_000u128),
                    BondTerm::NineMonths => (BOND_9M_TERM_SECS, 3_000),
                };
                let gross = if e.now() >= o.created + full { o.principal + (o.principal as u128 * bps / 10_000) as u64 } else { o.principal };
                fees += fee_of(gross) as u128;
                nets += (gross - fee_of(gross)) as u128;
                e.bond_request(&ks[o.w], o.idx);
                wallet_open[o.w] -= o.principal;
                requests += 1;
            }
            _ => {
                // a full heartbeat over every open claim
                if e.now() < last_start + GAP {
                    let t = last_start + GAP;
                    e.set_time(t);
                }
                let claims = e.open_claims();
                if claims.is_empty() {
                    continue;
                }
                last_start = e.now();
                let owed: Vec<u64> = claims.iter().map(|(_, c)| c.owed).collect();
                let mut pools = e.pools();
                let expect = oracle_settle(&owed, &mut pools);
                e.begin();
                let triples: Vec<Triple> = claims.iter().map(|(addr, c)| (*addr, ata(&c.trader_wallet, &e.usdc), ata(&c.trader_wallet, &e.usdt))).collect();
                for chunk in triples.chunks(6) {
                    e.settle(chunk);
                }
                for (i, (addr, c)) in claims.iter().enumerate() {
                    let pay = expect[i].0 + expect[i].1;
                    let after = e.claim_at(addr).map(|x| x.owed).unwrap_or(0);
                    assert_eq!(after, c.owed - pay, "claim {i} remainder");
                }
                assert_eq!(e.pools(), pools, "pools after the cycle match the model");
                e.finalize();
                cycles += 1;
            }
        }
        // ---- invariants after every step
        assert_eq!(e.totals(), totals, "tokens are conserved");
        let paid_out: u128 = ks
            .iter()
            .map(|k| (e.token_balance(&ata(&k.pubkey(), &e.usdc.clone())) + e.token_balance(&ata(&k.pubkey(), &e.usdt.clone()))) as u128)
            .sum();
        let (pu, pt) = e.pools();
        assert_eq!(pu as u128 + pt as u128, pool_credit - paid_out, "pools = deposits' pool shares - everything paid out");
        assert_eq!(e.token_balance(&e.sl8_usdc.clone()) as u128 + e.token_balance(&e.sl8_usdt.clone()) as u128, sl8_credit);
        let vs = e.vault_state();
        assert_eq!(vs.bond_withdrawal_fees_retained as u128, fees, "retained fees");
        assert_eq!(vs.open_claims_total as u128 + paid_out, nets, "claims requested = paid + still owed");
        e.assert_bond_invariant();
        e.assert_claim_invariant();
    }
    assert!(deposits > 10 && requests > 3 && cycles > 2, "the run exercised everything ({deposits} deposits, {requests} requests, {cycles} cycles)");
}
