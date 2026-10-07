//! request_payout moves tokens OUT: from exactly one pool (the larger; tie ->
//! USDC) to the trader's own account for that mint.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::token::spl_token;
use common::*;
use core_vault::errors::VaultError;
use litesvm::types::TransactionResult;
use std::collections::BTreeMap;

struct P {
    e: Env,
    s: Sector,
    w: Pubkey,
}

/// Registered product, trader `w` holding challenge 1 (bought with USDC).
/// Pools start with whatever that purchase put in them; tests then set them.
fn rig(max_payout: u64) -> P {
    let (mut e, s) = Env::registered(&Cfg { max_payout, ..Cfg::default() });
    let w = wallet();
    e.deposit(&s, &w, 1);
    P { e, s, w }
}

impl P {
    fn pools(&mut self, usdc: u64, usdt: u64) {
        self.e.set_pool(Coin::Usdc, usdc);
        self.e.set_pool(Coin::Usdt, usdt);
    }
    fn ix(&self, amount: u64, req: u64) -> Instruction {
        payout_ix(&self.e, &self.s, &self.w, 1, amount, req)
    }
    fn send(&mut self, amount: u64, req: u64) -> TransactionResult {
        let ix = self.ix(amount, req);
        self.e.send(ix)
    }
    fn ta(&self, c: Coin) -> Pubkey {
        self.e.wallet_ta(&self.w, c)
    }
    /// Raw bytes of the two accounts a payout may write.
    fn state_bytes(&self) -> (Vec<u8>, Vec<u8>) {
        (
            self.e.svm.get_account(&self.s.trader(&self.w, 1)).unwrap().data,
            self.e.svm.get_account(&self.s.registry()).unwrap().data,
        )
    }
    fn total(&self, snap: &BTreeMap<Pubkey, u64>, mint: &Pubkey) -> u128 {
        snap.iter().filter(|(a, _)| self.e.token_state(a).mint == *mint).map(|(_, v)| *v as u128).sum()
    }
    /// Every tracked balance equals `before` except the listed signed changes;
    /// and each mint's total is conserved.
    fn assert_deltas(&self, before: &BTreeMap<Pubkey, u64>, changes: &[(Pubkey, i128)]) {
        let after = self.e.token_snapshot();
        for (addr, b) in before {
            let delta = changes.iter().filter(|(a, _)| a == addr).map(|(_, d)| *d).sum::<i128>();
            assert_eq!(after[addr] as i128, *b as i128 + delta, "balance of {addr}");
        }
        for m in [self.e.usdc, self.e.usdt] {
            assert_eq!(self.total(before, &m), self.total(&after, &m), "tokens conserved for {m}");
        }
    }
    /// A rejected payout: exact error, no balance moved, trader/registry bytes identical.
    fn assert_rejected_cleanly(&mut self, ix: Instruction, check: impl FnOnce(&TransactionResult)) {
        let (bal, st) = (self.e.token_snapshot(), self.state_bytes());
        let r = self.e.send(ix);
        check(&r);
        assert_eq!(self.e.token_snapshot(), bal, "no token may move");
        assert_eq!(self.state_bytes(), st, "no state may change");
    }
}

// ------------------------------------------------------------ pool selection

fn pays_from_larger(larger: Coin) {
    let mut p = rig(5);
    let (big, small, amount) = (1_000u64, 400u64, 300u64);
    match larger {
        Coin::Usdc => p.pools(big, small),
        Coin::Usdt => p.pools(small, big),
    }
    let other = if larger == Coin::Usdc { Coin::Usdt } else { Coin::Usdc };
    let before = p.e.token_snapshot();
    p.e.advance(DAY);
    let m = assert_ok(p.send(amount, 1));
    assert_payout_outcome(&m, PayoutOutcome::Paid);

    // chosen pool -amount, trader's account for THAT mint +amount, nothing else moves
    p.assert_deltas(&before, &[(p.e.coin(larger).1, -(amount as i128)), (p.ta(larger), amount as i128)]);
    assert_eq!(p.e.token_balance(&p.e.coin(other).1), small, "the other pool is untouched");
    assert_eq!(p.e.token_balance(&p.e.coin(larger).1), big - amount);

    let ts = p.e.trader(&p.s, &p.w, 1);
    assert_eq!((ts.payout_count, ts.status, ts.last_activity_timestamp), (1, TraderStatus::Active, p.e.now()));
    assert_eq!(p.e.registry(&p.s).total_requests_emitted, 1);
}

#[test]
fn pays_from_usdc_when_usdc_pool_is_larger() {
    pays_from_larger(Coin::Usdc);
}
#[test]
fn pays_from_usdt_when_usdt_pool_is_larger() {
    pays_from_larger(Coin::Usdt);
}

#[test]
fn a_tie_pays_from_usdc() {
    let mut p = rig(5);
    p.pools(700, 700);
    let before = p.e.token_snapshot();
    assert_payout_outcome(&assert_ok(p.send(100, 1)), PayoutOutcome::Paid);
    p.assert_deltas(&before, &[(p.e.usdc_pool, -100), (p.ta(Coin::Usdc), 100)]);
    assert_eq!(p.e.token_balance(&p.e.usdt_pool), 700);
}

#[test]
fn the_smaller_pool_is_never_used_even_if_it_alone_could_pay() {
    let mut p = rig(5);
    p.pools(10_000, 500);
    let before = p.e.token_snapshot();
    assert_ok(p.send(100, 1));
    p.assert_deltas(&before, &[(p.e.usdc_pool, -100), (p.ta(Coin::Usdc), 100)]);
    assert_eq!(p.e.token_balance(&p.e.usdt_pool), 500);
}

#[test]
fn amount_equal_to_the_whole_pool_succeeds_and_drains_it() {
    for larger in [Coin::Usdc, Coin::Usdt] {
        let mut p = rig(5);
        match larger {
            Coin::Usdc => p.pools(1_000, 10),
            Coin::Usdt => p.pools(10, 1_000),
        }
        let before = p.e.token_snapshot();
        assert_ok(p.send(1_000, 1));
        assert_eq!(p.e.token_balance(&p.e.coin(larger).1), 0);
        p.assert_deltas(&before, &[(p.e.coin(larger).1, -1_000), (p.ta(larger), 1_000)]);
    }
}

#[test]
fn amount_one_above_the_pool_fails_insufficient_pool_balance() {
    let mut p = rig(5);
    p.pools(1_000, 10);
    let ix = p.ix(1_001, 1);
    p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::InsufficientPoolBalance));
    // and the boundary just below still works afterwards (nothing was consumed)
    assert_ok(p.send(1_000, 1));
}

#[test]
fn no_fallback_and_no_summing_when_the_larger_pool_is_too_small() {
    // 600 + 500 = 1_100 would cover 700, but the single larger pool (600) does not.
    for (usdc, usdt) in [(600u64, 500u64), (500, 600), (650, 650), (0, 0)] {
        let mut p = rig(5);
        p.pools(usdc, usdt);
        let ix = p.ix(700, 1);
        p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::InsufficientPoolBalance));
    }
}

#[test]
fn a_failed_payout_consumes_nothing() {
    let mut p = rig(1);
    p.pools(100, 50);
    p.e.advance(2 * DAY);
    let before_ts = p.e.trader(&p.s, &p.w, 1);
    let ix = p.ix(101, 1);
    p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::InsufficientPoolBalance));
    let ts = p.e.trader(&p.s, &p.w, 1);
    assert_eq!(ts.payout_count, 0, "payout_count not incremented");
    assert_eq!(ts.last_activity_timestamp, before_ts.last_activity_timestamp, "last_activity untouched");
    assert_eq!(ts.status, TraderStatus::Active, "not graduated (max_payout is 1)");
    assert_eq!(p.e.registry(&p.s).total_requests_emitted, 0, "no request emitted");
    // request id 1 is still the next id: a smaller payout now works and graduates
    assert_ok(p.send(100, 1));
    assert_eq!(p.e.trader(&p.s, &p.w, 1).status, TraderStatus::Graduated);
}

// ------------------------------------------------------- destination attacks

/// `corrupt` edits the instruction; the payout must fail with `want` and
/// change nothing. Pools: USDC 1_000 (picked), USDT 400.
fn attack(corrupt: impl FnOnce(&mut P, &mut Instruction), want: impl FnOnce(&TransactionResult)) {
    let mut p = rig(5);
    p.pools(1_000, 400);
    let mut ix = p.ix(100, 1);
    corrupt(&mut p, &mut ix);
    p.assert_rejected_cleanly(ix, want);
}
fn inv_ta(r: &TransactionResult) {
    assert_vault_err(r, VaultError::InvalidTokenAccount)
}

#[test]
fn rejects_a_usdc_destination_owned_by_someone_else() {
    attack(
        |p, ix| {
            let usdc = p.e.usdc;
            ix.accounts[PO.trader_usdc].pubkey = p.e.new_token_account(&usdc, &Pubkey::new_unique(), 0);
        },
        inv_ta,
    );
}

#[test]
fn rejects_a_usdt_destination_owned_by_someone_else_even_though_usdc_is_paid() {
    attack(
        |p, ix| {
            let usdt = p.e.usdt;
            ix.accounts[PO.trader_usdt].pubkey = p.e.new_token_account(&usdt, &Pubkey::new_unique(), 0);
        },
        inv_ta,
    );
}

#[test]
fn rejects_a_wrong_mint_account_in_either_slot() {
    attack(|p, ix| ix.accounts[PO.trader_usdc].pubkey = p.ta(Coin::Usdt), inv_ta);
    attack(|p, ix| ix.accounts[PO.trader_usdt].pubkey = p.ta(Coin::Usdc), inv_ta);
}

#[test]
fn rejects_accounts_owned_by_an_attacker_when_the_wallet_arg_is_the_victim() {
    attack(
        |p, ix| {
            let atk = Pubkey::new_unique();
            let (usdc, usdt) = (p.e.usdc, p.e.usdt);
            ix.accounts[PO.trader_usdc].pubkey = p.e.new_token_account(&usdc, &atk, 0);
            ix.accounts[PO.trader_usdt].pubkey = p.e.new_token_account(&usdt, &atk, 0);
        },
        inv_ta,
    );
}

#[test]
fn rejects_swapped_pools() {
    attack(|p, ix| ix.accounts[PO.usdt_pool].pubkey = p.e.usdc_pool, inv_ta);
    attack(|p, ix| ix.accounts[PO.usdc_pool].pubkey = p.e.usdt_pool, inv_ta);
    attack(
        |p, ix| {
            ix.accounts[PO.usdc_pool].pubkey = p.e.usdt_pool;
            ix.accounts[PO.usdt_pool].pubkey = p.e.usdc_pool;
        },
        inv_ta,
    );
}

#[test]
fn rejects_a_non_pool_account_in_a_pool_slot() {
    attack(
        |p, ix| {
            let usdc = p.e.usdc;
            // a USDC account somebody filled, posing as the pool
            ix.accounts[PO.usdc_pool].pubkey = p.e.new_token_account(&usdc, &Pubkey::new_unique(), 1_000_000);
        },
        inv_ta,
    );
}

#[test]
fn rejects_a_mint_that_is_not_the_vaults() {
    for slot in [PO.usdc_mint, PO.usdt_mint] {
        attack(
            |p, ix| {
                let m3 = Pubkey::new_unique();
                p.e.set_mint(&m3, 6, spl_token::ID);
                ix.accounts[slot].pubkey = m3;
            },
            |r| assert_vault_err(r, VaultError::InvalidMint),
        );
    }
    attack(
        |p, ix| {
            let m22 = Pubkey::new_unique();
            p.e.set_mint(&m22, 6, TOKEN_2022_ID);
            ix.accounts[PO.usdc_mint].pubkey = m22;
        },
        |r| assert_anchor_err(r, ErrorCode::AccountOwnedByWrongProgram),
    );
}

#[test]
fn rejects_the_wrong_token_program() {
    attack(
        |_, ix| ix.accounts[PO.token_program].pubkey = TOKEN_2022_ID,
        |r| assert_anchor_err(r, ErrorCode::InvalidProgramId),
    );
}

#[test]
fn rejects_a_vault_state_that_does_not_exist() {
    attack(
        |_, ix| ix.accounts[PO.vault].pubkey = Pubkey::new_unique(),
        |r| assert_anchor_err(r, ErrorCode::AccountNotInitialized),
    );
}

#[test]
fn a_frozen_destination_fails_inside_the_transfer_and_rolls_everything_back() {
    attack(
        |p, _| {
            let ta = p.ta(Coin::Usdc);
            p.e.edit_token_account(&ta, |a| a.state = anchor_spl::token::spl_token::state::AccountState::Frozen);
        },
        |r| assert_custom_code(r, 17, "SPL Token AccountFrozen"),
    );
}

// ----------------------------------------------------------------- stale path

#[test]
fn stale_path_abandons_ok_and_moves_no_tokens_even_with_empty_pools() {
    for (usdc, usdt) in [(0u64, 0u64), (5_000, 9_000)] {
        let mut p = rig(5);
        p.pools(usdc, usdt);
        p.e.advance(INACTIVITY_LIMIT_SECS + 1);
        let before = p.e.token_snapshot();
        let ix = p.ix(1_000_000, 1); // far more than any pool holds
        let m = assert_ok(p.e.send(ix));
        assert_payout_outcome(&m, PayoutOutcome::Abandoned);
        assert_eq!(p.e.token_snapshot(), before, "ZERO tokens move");
        let ts = p.e.trader(&p.s, &p.w, 1);
        assert_eq!((ts.status, ts.payout_count), (TraderStatus::Abandoned, 0));
        assert_eq!(p.e.registry(&p.s).total_requests_emitted, 0);
    }
}

#[test]
fn stale_path_still_needs_valid_destination_accounts() {
    // Account constraints run before the handler, so a bad destination fails
    // even a stale call, and then the Abandoned write does NOT persist.
    let mut p = rig(5);
    p.pools(0, 0);
    p.e.advance(INACTIVITY_LIMIT_SECS + 1);
    let mut ix = p.ix(10, 1);
    let usdc = p.e.usdc;
    ix.accounts[PO.trader_usdc].pubkey = p.e.new_token_account(&usdc, &Pubkey::new_unique(), 0);
    p.assert_rejected_cleanly(ix, inv_ta);
    assert_eq!(p.e.trader(&p.s, &p.w, 1).status, TraderStatus::Active);
    // the same stale call with valid accounts then abandons
    assert_payout_outcome(&assert_ok(p.send(10, 1)), PayoutOutcome::Abandoned);
}

#[test]
fn exactly_at_the_limit_is_not_stale_and_pays() {
    let mut p = rig(5);
    p.pools(1_000, 0);
    p.e.advance(INACTIVITY_LIMIT_SECS);
    let before = p.e.token_snapshot();
    assert_payout_outcome(&assert_ok(p.send(250, 1)), PayoutOutcome::Paid);
    p.assert_deltas(&before, &[(p.e.usdc_pool, -250), (p.ta(Coin::Usdc), 250)]);
}

// ----------------------------------------------------------------- graduation

#[test]
fn the_final_payout_moves_tokens_and_graduates() {
    let mut p = rig(2);
    p.pools(1_000, 0);
    let before = p.e.token_snapshot();
    assert_payout_outcome(&assert_ok(p.send(100, 1)), PayoutOutcome::Paid);
    assert_eq!(p.e.trader(&p.s, &p.w, 1).status, TraderStatus::Active);

    assert_payout_outcome(&assert_ok(p.send(200, 2)), PayoutOutcome::Paid);
    let ts = p.e.trader(&p.s, &p.w, 1);
    assert_eq!((ts.status, ts.payout_count), (TraderStatus::Graduated, 2));
    p.assert_deltas(&before, &[(p.e.usdc_pool, -300), (p.ta(Coin::Usdc), 300)]);

    // the cap-reached request after graduation is rejected as before, and moves nothing
    let ix = p.ix(1, 3);
    p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::InvalidTraderStatus));
}

#[test]
fn payout_cap_reached_still_rejects_before_any_pool_logic() {
    let mut p = rig(5);
    p.pools(1_000, 0);
    p.e.payout(&p.s.clone(), &p.w.clone(), 1, 10, 1);
    p.e.payout(&p.s.clone(), &p.w.clone(), 1, 10, 2);
    p.e.update(&p.s.clone(), &Cfg { max_payout: 2, ..Cfg::default() });
    p.pools(0, 0); // empty pools: the cap error must still win
    let ix = p.ix(10, 3);
    p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::PayoutCapReached));
}

#[test]
fn request_id_mismatch_is_checked_before_the_pool_balance() {
    let mut p = rig(5);
    p.pools(0, 0);
    let ix = p.ix(10, 9);
    p.assert_rejected_cleanly(ix, |r| assert_vault_err(r, VaultError::RequestIdMismatch));
}

// ------------------------------------------------------------------ lifecycle

#[test]
fn full_lifecycle_with_real_instructions_conserves_every_token() {
    let cfg = Cfg {
        fee_split_bps: 6500,
        tiers: vec![ChallengeSize { size: 100_000_000, cost: 20_000_000 }],
        max_payout: 5,
        reset_bps: vec![],
    };
    let (mut e, s) = Env::registered(&cfg);
    let (a, b) = (wallet(), wallet());
    let (ta_a, ta_b) = (e.fund_wallet(&a), e.fund_wallet(&b));
    let (usdc_pool, usdt_pool, sl8_usdc, sl8_usdt) = (e.usdc_pool, e.usdt_pool, e.sl8_usdc, e.sl8_usdt);

    let mut expect = e.token_snapshot();
    let t0 = (e.total_of(&e.usdc.clone()), e.total_of(&e.usdt.clone()));
    // applies signed changes to the model, then checks the chain against it, plus conservation
    let mut step = |e: &mut Env, label: &str, changes: &[(Pubkey, i128)], expect: &mut BTreeMap<Pubkey, u64>| {
        for (addr, d) in changes {
            let v = expect.get_mut(addr).unwrap();
            *v = (*v as i128 + d) as u64;
        }
        assert_eq!(&e.token_snapshot(), expect, "balances after: {label}");
        assert_eq!((e.total_of(&e.usdc.clone()), e.total_of(&e.usdt.clone())), t0, "conservation after: {label}");
    };

    // 1. A buys with USDC (20 coins): 13 to the USDC pool, 7 to SL8
    e.deposit_coin(&s, &a, 1, (100_000_000, 20_000_000), Coin::Usdc);
    step(&mut e, "A buys (USDC)", &[(ta_a.usdc, -20_000_000), (usdc_pool, 13_000_000), (sl8_usdc, 7_000_000)], &mut expect);
    // 2. B buys with USDT
    e.deposit_coin(&s, &b, 1, (100_000_000, 20_000_000), Coin::Usdt);
    step(&mut e, "B buys (USDT)", &[(ta_b.usdt, -20_000_000), (usdt_pool, 13_000_000), (sl8_usdt, 7_000_000)], &mut expect);

    let pay = |e: &mut Env, w: &Pubkey, amount: u64, req: u64| {
        let ix = payout_ix(e, &s, w, 1, amount, req);
        e.send(ix)
    };

    // 3. pools tie 13M/13M -> USDC pays A 5M
    assert_payout_outcome(&assert_ok(pay(&mut e, &a, 5_000_000, 1)), PayoutOutcome::Paid);
    step(&mut e, "A #1 (tie -> USDC)", &[(usdc_pool, -5_000_000), (ta_a.usdc, 5_000_000)], &mut expect);
    // 4. USDC 8M vs USDT 13M -> USDT pays A 4M (A is paid in USDT this time)
    assert_ok(pay(&mut e, &a, 4_000_000, 2));
    step(&mut e, "A #2 (USDT larger)", &[(usdt_pool, -4_000_000), (ta_a.usdt, 4_000_000)], &mut expect);
    // 5. USDT 9M vs USDC 8M -> USDT pays B 6M
    assert_ok(pay(&mut e, &b, 6_000_000, 1));
    step(&mut e, "B #1 (USDT larger)", &[(usdt_pool, -6_000_000), (ta_b.usdt, 6_000_000)], &mut expect);
    // 6. USDC 8M vs USDT 3M -> USDC pays A 5M
    assert_ok(pay(&mut e, &a, 5_000_000, 3));
    step(&mut e, "A #3 (USDC larger)", &[(usdc_pool, -5_000_000), (ta_a.usdc, 5_000_000)], &mut expect);
    // 7. both pools 3M; 3_000_001 fails (sum 6M would cover it) and changes nothing
    let r = pay(&mut e, &a, 3_000_001, 4);
    assert_vault_err(&r, VaultError::InsufficientPoolBalance);
    step(&mut e, "A over-ask rejected", &[], &mut expect);
    assert_eq!(e.trader(&s, &a, 1).payout_count, 3);
    // 8. 3_000_000 on the tie -> USDC drains to 0
    assert_ok(pay(&mut e, &a, 3_000_000, 4));
    step(&mut e, "A #4 (tie, drains USDC)", &[(usdc_pool, -3_000_000), (ta_a.usdc, 3_000_000)], &mut expect);
    assert_eq!(e.token_balance(&usdc_pool), 0);
    // 9. USDC empty, USDT 3M -> USDT pays B 1
    assert_ok(pay(&mut e, &b, 1, 2));
    step(&mut e, "B #2 (USDT)", &[(usdt_pool, -1), (ta_b.usdt, 1)], &mut expect);

    // SL8 only ever received fee shares: 7M each, never touched by payouts
    assert_eq!((e.token_balance(&sl8_usdc), e.token_balance(&sl8_usdt)), (7_000_000, 7_000_000));
    assert_eq!(e.registry(&s).total_requests_emitted, 6);
    assert_eq!((e.trader(&s, &a, 1).payout_count, e.trader(&s, &b, 1).payout_count), (4, 2));
}
