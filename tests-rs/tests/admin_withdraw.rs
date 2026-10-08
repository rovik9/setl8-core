//! admin_withdraw_marketing_funds: the documented exception to "no admin key on
//! money". Both admins, SL8 wallet only, at most 75% of one pool's live balance.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use core_vault::constants::HEARTBEAT_MIN_GAP_SECS as GAP;
use core_vault::errors::VaultError;
use core_vault::state::BondTerm;
use litesvm::types::TransactionResult;
use solana_signer::Signer;

const M: u64 = 1_000_000;

/// ceil(x * 25%), independently of the program's code.
fn quarter_up(x: u64) -> u64 {
    ((x as u128 * 2_500 + 9_999) / 10_000) as u64
}

fn pool_with(usdc: u64, usdt: u64) -> Env {
    let mut e = Env::new();
    e.set_pool(Coin::Usdc, usdc);
    e.set_pool(Coin::Usdt, usdt);
    e
}

fn assert_unchanged(e: &mut Env, ix: Instruction, check: impl FnOnce(&TransactionResult)) {
    let before = (e.token_snapshot(), e.svm.get_account(&e.vault).unwrap().data);
    let r = e.send(ix);
    check(&r);
    assert_eq!((e.token_snapshot(), e.svm.get_account(&e.vault).unwrap().data), before, "a refused withdrawal changes nothing");
}

// ------------------------------------------------------------- worked examples

#[test]
fn a_thousand_dollar_pool_with_no_floor_yet_releases_exactly_750() {
    let mut e = pool_with(1_000 * M, 0);
    assert_eq!(e.vault_state().usdc_floor, 0, "no finalize yet: the stored floor is 0");
    for (bad, want) in [
        (750 * M + 1, VaultError::WithdrawalExceedsReserve),
        (u64::MAX, VaultError::WithdrawalExceedsReserve),
        (0, VaultError::ZeroAmount),
    ] {
        let ix = withdraw_ix(&e, Coin::Usdc, bad);
        assert_unchanged(&mut e, ix, |r| assert_vault_err(r, want));
    }
    let totals = e.totals();
    let m = e.withdraw(Coin::Usdc, 750 * M);
    println!("admin_withdraw_marketing_funds: {} CU", m.compute_units_consumed);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 250 * M);
    assert_eq!(e.token_balance(&e.sl8_usdc.clone()), 750 * M);
    assert_eq!(e.totals(), totals, "conserved");
    let logs = m.logs.join("\n");
    assert!(logs.contains("withdrawable=750000000") && logs.contains("reserve=250000000") && logs.contains("amount=750000000"), "{logs}");
    // repeatable: 75% of the 250 left (reserve 62.5) is available again, not one unit more
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 187_500_001), VaultError::WithdrawalExceedsReserve);
}

#[test]
fn after_a_finalize_that_stored_the_floor_the_answer_is_the_same() {
    let mut e = pool_with(1_000 * M, 0);
    e.begin();
    e.finalize();
    assert_eq!(e.vault_state().usdc_floor, 250 * M);
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 750 * M + 1), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdc, 750 * M);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 250 * M);
}

#[test]
fn the_reserve_rounds_up_to_the_base_unit() {
    // 1,000.000001: 25% = 250.00000025 -> reserve 250.000001 -> withdrawable 750.000000
    let mut e = pool_with(1_000_000_001, 0);
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 750_000_001), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdc, 750_000_000);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 250_000_001);
    assert_eq!(250_000_001, quarter_up(1_000_000_001));
    // and awkward balances in general: remaining is always exactly ceil(25%)
    for live in [1u64, 2, 3, 4, 5, 7, 999_999_999, 123_456_789, 1_000_000_003] {
        let mut e = pool_with(live, 0);
        let w = live - quarter_up(live);
        if w == 0 {
            assert_vault_err(&e.withdraw_result(Coin::Usdc, 1), VaultError::WithdrawalExceedsReserve);
        } else {
            assert_vault_err(&e.withdraw_result(Coin::Usdc, w + 1), VaultError::WithdrawalExceedsReserve);
            e.withdraw(Coin::Usdc, w);
        }
        assert_eq!(e.token_balance(&e.usdc_pool.clone()), quarter_up(live), "live {live}");
    }
}

#[test]
fn a_higher_stored_floor_wins_over_the_live_quarter() {
    let mut e = pool_with(400 * M, 0);
    e.set_vault_state(|v| v.usdc_floor = 250 * M); // the balance fell after the floor was stored
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 150 * M + 1), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdc, 150 * M);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 250 * M);
}

#[test]
fn a_balance_at_or_below_the_stored_floor_releases_nothing() {
    for live in [300 * M, 500 * M, 0] {
        let mut e = pool_with(live, 0);
        e.set_vault_state(|v| v.usdc_floor = 500 * M);
        let ix = withdraw_ix(&e, Coin::Usdc, 1);
        assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::WithdrawalExceedsReserve));
    }
}

#[test]
fn deposits_after_a_finalize_raise_the_withdrawable_amount() {
    let mut e = pool_with(1_000 * M, 0);
    e.begin();
    e.finalize(); // floor 250
    // a bond deposit puts liquidity into the pool: live = 1,000 + 500
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 1_500 * M);
    // the live 25% (375) now beats the stored floor (250): withdrawable 1,125
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 1_125 * M + 1), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdc, 1_125 * M);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 375 * M);
}

#[test]
fn repeated_withdrawals_decay_the_pool_geometrically() {
    let mut e = pool_with(1_000_000_000, 0);
    let mut expected = 1_000_000_000u64;
    let mut seen = vec![];
    for n in 1..=4 {
        let live = e.token_balance(&e.usdc_pool.clone());
        let w = live - quarter_up(live);
        assert_vault_err(&e.withdraw_result(Coin::Usdc, w + 1), VaultError::WithdrawalExceedsReserve);
        e.withdraw(Coin::Usdc, w);
        expected = quarter_up(expected);
        seen.push(e.token_balance(&e.usdc_pool.clone()));
        assert_eq!(*seen.last().unwrap(), expected, "after call {n}");
    }
    assert_eq!(seen, vec![250_000_000, 62_500_000, 15_625_000, 3_906_250]);
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, 1_000_000_000 - 3_906_250);
    assert_eq!(e.token_balance(&e.sl8_usdc.clone()), 1_000_000_000 - 3_906_250);
}

// ------------------------------------------------------------ pool independence

#[test]
fn the_two_pools_are_independent_and_the_counters_follow_the_side() {
    let mut e = pool_with(1_000 * M, 400 * M);
    e.withdraw(Coin::Usdc, 750 * M);
    assert_eq!(e.pools(), (250 * M, 400 * M), "USDT untouched");
    assert_eq!(e.token_balance(&e.sl8_usdt.clone()), 0);
    let vs = e.vault_state();
    assert_eq!((vs.marketing_withdrawn_usdc, vs.marketing_withdrawn_usdt), (750 * M, 0));
    // USDC allows 75% of the 250 left again; USDT has 300 available (its own 25% reserve is 100)
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 187_500_001), VaultError::WithdrawalExceedsReserve);
    assert_vault_err(&e.withdraw_result(Coin::Usdt, 300 * M + 1), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdt, 300 * M);
    assert_eq!(e.pools(), (250 * M, 100 * M));
    let vs = e.vault_state();
    assert_eq!((vs.marketing_withdrawn_usdc, vs.marketing_withdrawn_usdt), (750 * M, 300 * M));
    assert_eq!((e.token_balance(&e.sl8_usdc.clone()), e.token_balance(&e.sl8_usdt.clone())), (750 * M, 300 * M));
}

#[test]
fn the_two_pools_are_never_combined() {
    // USDC is tiny, USDT is big: the combined 75% would allow far more than USDC's own
    let mut e = pool_with(100 * M, 10_000 * M);
    assert_vault_err(&e.withdraw_result(Coin::Usdc, 75 * M + 1), VaultError::WithdrawalExceedsReserve);
    e.withdraw(Coin::Usdc, 75 * M);
    assert_eq!(e.pools(), (25 * M, 10_000 * M));
}

#[test]
fn naming_one_side_with_the_other_sides_accounts_fails() {
    let mut e = pool_with(1_000 * M, 1_000 * M);
    // args say USDT, accounts are the USDC ones (and the reverse)
    for (c_args, c_accts) in [(Coin::Usdt, Coin::Usdc), (Coin::Usdc, Coin::Usdt)] {
        let mut ix = withdraw_ix(&e, c_accts, M);
        ix.data = withdraw_ix(&e, c_args, M).data;
        assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidMint));
    }
    // right mint, but the other side's pool
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.pool].pubkey = e.usdt_pool;
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
    let mut ix = withdraw_ix(&e, Coin::Usdt, M);
    ix.accounts[AW.pool].pubkey = e.usdc_pool;
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
}

// --------------------------------------------------------------------- 2-of-2

#[test]
fn both_admins_must_sign_and_be_the_exact_keys() {
    let mut e = pool_with(1_000 * M, 0);
    let stranger = Pubkey::new_unique();
    for slot in [AW.sl8_admin, AW.rov_admin] {
        // a wrong key in the slot
        let mut ix = withdraw_ix(&e, Coin::Usdc, M);
        ix.accounts[slot].pubkey = stranger;
        assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::MissingMultisigSignature));
        // the right key, present but not signing
        let mut ix = withdraw_ix(&e, Coin::Usdc, M);
        ix.accounts[slot].is_signer = false;
        assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
    }
    // neither signs
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.sl8_admin].is_signer = false;
    ix.accounts[AW.rov_admin].is_signer = false;
    assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
    // swapped keys
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts.swap(AW.sl8_admin, AW.rov_admin);
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::MissingMultisigSignature));
    // the same key twice
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.rov_admin].pubkey = e.sl8.pubkey();
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::MissingMultisigSignature));
    // both together work
    e.withdraw(Coin::Usdc, M);
}

#[test]
fn real_signatures_are_required_when_signatures_are_verified() {
    // sigverify ON: leaving a required signature out is a transaction-level failure
    let mut e = Env::new_sigverify_on();
    e.init_vault();
    e.set_pool(Coin::Usdc, 1_000 * M);
    let (sl8, rov, payer) = (dup(&e.sl8), dup(&e.rov), dup(&e.payer));
    let ix = withdraw_ix(&e, Coin::Usdc, M);
    assert!(e.send_with(&[ix.clone()], &payer, &[&sl8]).is_err(), "SL8 alone");
    assert!(e.send_with(&[ix.clone()], &payer, &[&rov]).is_err(), "ROV alone");
    assert!(e.send_with(&[ix.clone()], &payer, &[]).is_err(), "neither");
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 1_000 * M);
    assert_ok(e.send_with(&[ix], &payer, &[&sl8, &rov]));
    assert_eq!(e.token_balance(&e.sl8_usdc.clone()), M);
}

// ----------------------------------------------------------- destination safety

#[test]
fn the_destination_can_only_be_the_sl8_wallets_account_for_that_mint() {
    let mut e = pool_with(1_000 * M, 1_000 * M);
    let trader = Pubkey::new_unique();
    let t = e.fund_wallet(&trader);
    let stranger_usdc = e.new_token_account(&e.usdc.clone(), &Pubkey::new_unique(), 0);
    let rov_usdc = e.new_token_account(&e.usdc.clone(), &e.rov.pubkey(), 0); // even Rov's own account
    let cases: Vec<(&str, Pubkey)> = vec![
        ("a trader's account", t.usdc),
        ("an account owned by someone else", stranger_usdc),
        ("the other admin's account", rov_usdc),
        ("SL8's account for the OTHER mint", e.sl8_usdt),
        ("the pool itself", e.usdc_pool),
        ("the other pool", e.usdt_pool),
    ];
    for (label, bad) in cases {
        let mut ix = withdraw_ix(&e, Coin::Usdc, M);
        ix.accounts[AW.sl8_ta].pubkey = bad;
        assert_unchanged(&mut e, ix, |r| {
            let _ = label;
            assert_vault_err(r, VaultError::InvalidTokenAccount)
        });
    }
}

#[test]
fn a_frozen_sl8_account_fails_inside_the_token_program() {
    let mut e = pool_with(1_000 * M, 0);
    let sl8 = e.sl8_usdc;
    e.edit_token_account(&sl8, |a| a.state = AccountState::Frozen);
    let ix = withdraw_ix(&e, Coin::Usdc, M);
    assert_unchanged(&mut e, ix, |r| assert_custom_code(r, 17, "SPL Token AccountFrozen"));
}

/// Unusable destinations must fail with EXACTLY the error `deposit_fee` gives for the
/// same corruption of ITS SL8 account.
#[test]
fn unusable_destinations_fail_exactly_as_deposit_fee_does() {
    type Corrupt = fn(&mut Env, &Pubkey);
    let corruptions: Vec<(&str, Corrupt)> = vec![
        ("uninitialised token account", |e, a| e.set_raw(a, vec![0u8; 165], anchor_spl::token::spl_token::ID)),
        ("Token-2022 owned", |e, a| {
            let data = e.svm.get_account(a).unwrap().data;
            e.set_raw(a, data, TOKEN_2022_ID)
        }),
        ("no such account", |e, a| e.svm.set_account(*a, solana_account::Account::default()).unwrap()),
        ("system-owned dust", |e, a| {
            e.svm
                .set_account(*a, solana_account::Account { lamports: 5_000_000, data: vec![], owner: anchor_lang::system_program::ID, executable: false, rent_epoch: 0 })
                .unwrap()
        }),
    ];
    for (label, corrupt) in corruptions {
        // deposit_fee with the same corruption of its SL8 account
        let (mut f, s) = Env::registered(&Cfg::default());
        let w = wallet();
        f.fund_wallet(&w);
        let sl8 = f.sl8_usdc;
        corrupt(&mut f, &sl8);
        let ix = deposit_fee_ix(&f, &s, &w, 1, TIER_A.1, TIER_A.0);
        let want = match f.send(ix) {
            Err(m) => format!("{:?}", m.err),
            Ok(_) => panic!("{label}: deposit_fee unexpectedly succeeded"),
        };
        // the withdrawal
        let mut e = pool_with(1_000 * M, 0);
        let sl8 = e.sl8_usdc;
        corrupt(&mut e, &sl8);
        let before = e.token_snapshot();
        let ix = withdraw_ix(&e, Coin::Usdc, M);
        let got = match e.send(ix) {
            Err(m) => format!("{:?}", m.err),
            Ok(_) => panic!("{label}: the withdrawal unexpectedly succeeded"),
        };
        assert_eq!(got, want, "{label}");
        assert_eq!(e.token_snapshot(), before);
    }
}

#[test]
fn the_token_program_and_the_mint_must_be_the_real_ones() {
    let mut e = pool_with(1_000 * M, 1_000 * M);
    for bad in [TOKEN_2022_ID, Pubkey::new_unique(), anchor_lang::system_program::ID] {
        let mut ix = withdraw_ix(&e, Coin::Usdc, M);
        ix.accounts[AW.token_program].pubkey = bad;
        assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::InvalidProgramId));
    }
    // a mint that is not the vault's, a Token-2022 mint, and an account that does not exist
    let rogue = Pubkey::new_unique();
    e.set_mint(&rogue, 6, anchor_spl::token::spl_token::ID);
    let m22 = Pubkey::new_unique();
    e.set_mint(&m22, 6, TOKEN_2022_ID);
    for (bad, check) in [
        (rogue, VaultError::InvalidMint as u32),
        (e.usdt, VaultError::InvalidMint as u32),
    ] {
        let mut ix = withdraw_ix(&e, Coin::Usdc, M);
        ix.accounts[AW.mint].pubkey = bad;
        assert_unchanged(&mut e, ix, |r| assert_custom_code(r, 6000 + check, "wrong mint"));
    }
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.mint].pubkey = m22;
    assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountOwnedByWrongProgram));
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.mint].pubkey = Pubkey::new_unique();
    assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotInitialized));
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    ix.accounts[AW.vault].pubkey = Pubkey::new_unique();
    assert_unchanged(&mut e, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotInitialized));
}

#[test]
fn the_instruction_has_no_room_for_another_destination() {
    let e = pool_with(1_000 * M, 0);
    let ix = withdraw_ix(&e, Coin::Usdc, M);
    assert_eq!(ix.accounts.len(), 7, "no destination argument and no remaining accounts are expected");
    let mut e = e;
    // extra accounts appended by an attacker are simply ignored: the destination stays SL8's
    let mut ix = withdraw_ix(&e, Coin::Usdc, M);
    let attacker = e.new_token_account(&e.usdc.clone(), &Pubkey::new_unique(), 0);
    ix.accounts.push(anchor_lang::solana_program::instruction::AccountMeta::new(attacker, false));
    e.send(ix).unwrap();
    assert_eq!(e.token_balance(&attacker), 0);
    assert_eq!(e.token_balance(&e.sl8_usdc.clone()), M);
}

// ----------------------------------------------------- conservation and counters

#[test]
fn a_withdrawal_moves_exactly_the_amount_and_nothing_else() {
    let mut e = pool_with(1_234_567_891, 777_777_777);
    let (before, totals) = (e.token_snapshot(), e.totals());
    let w = 1_234_567_891 - quarter_up(1_234_567_891);
    e.withdraw(Coin::Usdc, w);
    let after = e.token_snapshot();
    for (addr, b) in &before {
        let delta = if *addr == e.usdc_pool { -(w as i128) } else if *addr == e.sl8_usdc { w as i128 } else { 0 };
        assert_eq!(after[addr] as i128, *b as i128 + delta, "balance of {addr}");
    }
    assert_eq!(e.totals(), totals);
    let vs = e.vault_state();
    assert_eq!((vs.marketing_withdrawn_usdc, vs.marketing_withdrawn_usdt), (w, 0));
    // a second call on the same side accumulates
    let live = e.token_balance(&e.usdc_pool.clone());
    let w2 = live - quarter_up(live);
    e.withdraw(Coin::Usdc, w2);
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, w + w2);
}

#[test]
fn the_counter_cannot_overflow() {
    let mut e = pool_with(1_000 * M, 0);
    e.set_vault_state(|v| v.marketing_withdrawn_usdc = u64::MAX - 5);
    let ix = withdraw_ix(&e, Coin::Usdc, 6);
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::MathOverflow));
    e.withdraw(Coin::Usdc, 5);
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, u64::MAX);

    // the USDT counter has its own check
    let mut e = pool_with(0, 1_000 * M);
    e.set_vault_state(|v| v.marketing_withdrawn_usdt = u64::MAX - 5);
    let ix = withdraw_ix(&e, Coin::Usdt, 6);
    assert_unchanged(&mut e, ix, |r| assert_vault_err(r, VaultError::MathOverflow));
    e.withdraw(Coin::Usdt, 5);
    assert_eq!(e.vault_state().marketing_withdrawn_usdt, u64::MAX);
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, 0);
}

// ------------------------------------------------ the documented-exception tests

#[test]
fn documented_exception_a_mid_cycle_withdrawal_shrinks_what_claims_get() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let (w, claim) = e.queue_claim(&s, 1_000 * M);
    e.make_atas(&w);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 0);
    e.begin(); // snapshot: owed 1,000, available 1,000 => ratio 1
    assert_eq!(e.vault_state().cycle_available_snapshot, 1_000 * M);
    e.withdraw(Coin::Usdc, 750 * M); // allowed mid-cycle; the cycle state does not gate it
    assert!(e.vault_state().cycle_active);
    let totals = e.totals();

    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    // the snapshot ratio says 1,000, but only the live 250 exists
    assert_eq!(e.token_balance(&ata(&w, &e.usdc.clone())), 250 * M);
    assert_eq!(e.claim_at(&claim).unwrap().owed, 750 * M, "the unpaid remainder stays owed");
    assert_eq!(e.vault_state().open_claims_total, 750 * M);
    assert_eq!(e.totals(), totals);
    e.assert_claim_invariant();
    e.finalize(); // still works; floors come from the post-settlement balance
    assert_eq!(e.pools(), (0, 0));
    assert_eq!((e.vault_state().usdc_floor, e.vault_state().usdt_floor), (0, 0));
}

#[test]
fn documented_exception_bond_principal_in_the_pool_is_withdrawable() {
    let mut e = Env::new();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 500 * M);
    // the admins can take 75% of it although the pool holds a depositor's bond
    e.withdraw(Coin::Usdc, 375 * M);
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 125 * M);
    assert_eq!(e.vault_state().bond_principal_open_total, 1_000 * M, "the bond liability is untouched and unprotected");
    // the bond's claim later settles pro rata against what is left
    e.advance(core_vault::constants::BOND_6M_LOCK_SECS);
    e.make_atas(&k.pubkey());
    e.bond_request(&k, 0);
    e.begin();
    e.settle(&[(bond_claim_pda(&k.pubkey(), 0).0, ata(&k.pubkey(), &e.usdc), ata(&k.pubkey(), &e.usdt))]);
    assert_eq!(e.token_balance(&ata(&k.pubkey(), &e.usdc.clone())), 125 * M, "everything left, nothing more");
    assert_eq!(e.bond_claim_opt(&k.pubkey(), 0).unwrap().owed, 998 * M - 125 * M);
}

#[test]
fn documented_exception_open_claims_are_ignored_by_the_cap() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    e.queue_claim(&s, 50_000 * M);
    e.set_pool(Coin::Usdc, 1_000 * M);
    assert_eq!(e.vault_state().open_claims_total, 50_000 * M);
    e.withdraw(Coin::Usdc, 750 * M); // 50x the pool is owed, and the admins still take 75%
    assert_eq!(e.token_balance(&e.usdc_pool.clone()), 250 * M);
}

#[test]
fn documented_exception_repeated_withdrawals_across_cycles_decay_geometrically_while_claims_settle_pro_rata() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    e.set_pool(Coin::Usdc, 100_000 * M);
    e.set_pool(Coin::Usdt, 0);
    let mut model_pool = e.pools().0;
    let mut open: Vec<(Pubkey, u64)> = vec![]; // (wallet, still owed)
    let mut everyone: Vec<Pubkey> = vec![];
    for round in 1..=6 {
        // a new 100-dollar claim joins the queue (its purchase put 65 into the pool)
        let (w, _) = e.queue_claim(&s, 100 * M);
        e.make_atas(&w);
        let totals = e.totals();
        model_pool += 65;
        open.push((w, 100 * M));
        everyone.push(w);
        assert_eq!(e.pools().0, model_pool);

        // the admins take their maximum
        let live = e.pools().0;
        e.withdraw(Coin::Usdc, live - quarter_up(live));
        model_pool = quarter_up(live);
        assert_eq!(e.pools().0, model_pool, "round {round}: ceil(25%) is left");

        // a full heartbeat
        e.advance(GAP);
        e.begin();
        let owed: Vec<u64> = open.iter().map(|c| c.1).collect();
        let mut pools = (model_pool, 0);
        let expect = oracle_settle(&owed, &mut pools);
        let triples: Vec<Triple> = open
            .iter()
            .map(|(w, _)| {
                let claim = e.open_claims().iter().find(|(_, c)| c.trader_wallet == *w).unwrap().0;
                (claim, ata(w, &e.usdc), ata(w, &e.usdt))
            })
            .collect();
        for chunk in triples.chunks(6) {
            e.settle(chunk);
        }
        for (i, c) in open.iter_mut().enumerate() {
            c.1 -= expect[i].0 + expect[i].1;
        }
        open.retain(|c| c.1 > 0);
        model_pool = pools.0;
        assert_eq!(e.pools().0, model_pool, "round {round}: claims were paid pro rata from what was left");
        e.finalize();
        assert_eq!(e.vault_state().usdc_floor, model_pool * 2_500 / 10_000, "floors come from the post-settlement balance");

        // nothing created or lost, nobody overpaid, the counters agree
        assert_eq!(e.totals(), totals, "round {round}: conserved");
        let owed_now: u64 = open.iter().map(|c| c.1).sum();
        assert_eq!(e.vault_state().open_claims_total, owed_now);
        for w in &everyone {
            let still = open.iter().find(|c| c.0 == *w).map(|c| c.1).unwrap_or(0);
            assert_eq!(e.token_balance(&ata(w, &e.usdc.clone())), 100 * M - still, "paid + owed = requested, never more");
        }
        e.assert_claim_invariant();
    }
    // the last rounds were short of liquidity: some claims are carried over, still owed
    assert!(!open.is_empty(), "the geometric decay left claims unpaid, carried over");
}

#[test]
fn the_withdrawal_is_vault_level_and_ignores_product_pauses() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.pause(&s); // a manual product pause
    e.withdraw(Coin::Usdc, M);
    e.resume(&s);
    // a reconciliation auto-pause (the sector has no tally but the books say otherwise)
    let (w, _) = e.queue_claim(&s, 10 * M);
    let _ = w;
    e.reconcile(&s);
    assert!(!e.registry(&s).active);
    e.withdraw(Coin::Usdc, M);
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, 2 * M);
}

#[test]
fn no_cycle_state_gates_the_withdrawal() {
    let mut e = pool_with(1_000 * M, 0);
    e.withdraw(Coin::Usdc, M); // no cycle ever started
    e.begin();
    e.withdraw(Coin::Usdc, M); // cycle open
    e.finalize();
    e.withdraw(Coin::Usdc, M); // cycle closed
    assert_eq!(e.vault_state().marketing_withdrawn_usdc, 3 * M);
}
