//! The whole money path with real instructions, from a purchase to a carried-over
//! payout, checking balances, conservation and the claim invariant at every step.
mod common;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
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
