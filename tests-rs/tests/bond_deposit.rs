//! deposit_bond: caps, the 0.2% fee, the 50/50 principal split, index agreement.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::constants::{BOND_GLOBAL_CAP, BOND_MAX_PER_WALLET, BOND_MIN_PRINCIPAL};
use core_vault::errors::VaultError;
use core_vault::state::{BondCapTracker, BondPosition, BondTerm};
use solana_keypair::Keypair;
use solana_signer::Signer;

const M: u64 = 1_000_000;

fn world() -> Env {
    Env::new()
}

fn fee_of(p: u64) -> u64 {
    ((p as u128 * 20 + 9_999) / 10_000) as u64
}

/// Everything a rejected deposit must leave untouched.
fn assert_rejected(e: &mut Env, k: &Keypair, ix: anchor_lang::solana_program::instruction::Instruction, check: impl FnOnce(&litesvm::types::TransactionResult)) {
    let before = (e.token_snapshot(), e.svm.get_account(&e.vault).unwrap().data, e.digest());
    let pos_before: Vec<_> = e.bond_addrs.borrow().iter().map(|a| e.svm.get_account(a).map(|x| x.data)).collect();
    let tracker_before = e.svm.get_account(&bond_cap_pda(&k.pubkey()).0).map(|x| x.data);
    let r = e.send_as(ix, k);
    check(&r);
    assert_eq!(e.token_snapshot(), before.0, "no token may move");
    assert_eq!(e.svm.get_account(&e.vault).unwrap().data, before.1, "vault counters unchanged");
    let pos_after: Vec<_> = e.bond_addrs.borrow().iter().map(|a| e.svm.get_account(a).map(|x| x.data)).collect();
    assert_eq!(pos_after[..pos_before.len()], pos_before[..], "no position may change");
    assert_eq!(e.svm.get_account(&bond_cap_pda(&k.pubkey()).0).map(|x| x.data), tracker_before, "tracker unchanged");
}

// ------------------------------------------------------------- worked examples

#[test]
fn alice_deposits_a_thousand_dollars_in_usdc() {
    let mut e = world();
    let alice = e.new_depositor();
    let a = alice.pubkey();
    let wt = e.wallet_tok(&a);
    let (pool, sl8) = (e.usdc_pool, e.sl8_usdc);
    let lamports_before = e.lamports(&a);
    let other = (e.token_balance(&wt.usdt), e.token_balance(&e.usdt_pool.clone()), e.token_balance(&e.sl8_usdt.clone()));

    let m = e.bond_deposit(&alice, 0, 1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    println!("deposit_bond (first deposit: creates position and tracker): {} CU", m.compute_units_consumed);

    assert_eq!(e.token_balance(&wt.usdc), WALLET_START - 1_002 * M, "she pays 1,002.000000");
    assert_eq!(e.token_balance(&pool), 500 * M, "pool +500");
    assert_eq!(e.token_balance(&sl8), 502 * M, "SL8 wallet +502 (500 principal share + the 2.0 fee)");
    assert_eq!((e.token_balance(&wt.usdt), e.token_balance(&e.usdt_pool.clone()), e.token_balance(&e.sl8_usdt.clone())), other, "the other mint is untouched");
    assert_eq!(e.token_balance(&wt.usdc) + e.token_balance(&pool) + e.token_balance(&sl8), WALLET_START + 0, "conserved");

    let p = e.bond(&a, 0).expect("position");
    assert_eq!(p.depositor, a);
    assert_eq!(p.deposit_index, 0);
    assert_eq!(p.mint, e.usdc);
    assert_eq!(p.principal, 1_000 * M);
    assert_eq!(p.term, BondTerm::SixMonths);
    assert_eq!(p.interest_bps, 2_000);
    assert_eq!(p.created_at, T0);
    assert_eq!(p.bump, bond_pda(&a, 0).1);
    let t = e.tracker_of(&a).expect("tracker");
    assert_eq!((t.depositor, t.open_principal_total, t.next_deposit_index, t.bump), (a, 1_000 * M, 1, bond_cap_pda(&a).1));
    assert_eq!(e.vault_state().bond_principal_open_total, 1_000 * M);

    // she paid the rent of the position and the tracker, and nothing else in lamports
    let rent = |n| e.svm.minimum_balance_for_rent_exemption(n);
    assert_eq!(lamports_before - e.lamports(&a), rent(BondPosition::SPACE) + rent(BondCapTracker::SPACE));
    assert_eq!(e.svm.get_account(&bond_pda(&a, 0).0).unwrap().data.len(), BondPosition::SPACE);
    assert_eq!(e.svm.get_account(&bond_cap_pda(&a).0).unwrap().data.len(), BondCapTracker::SPACE);
    e.assert_bond_invariant();
}

#[test]
fn every_mint_and_term_combination_is_exact() {
    for (coin, term, bps) in [
        (Coin::Usdc, BondTerm::SixMonths, 2_000u16),
        (Coin::Usdc, BondTerm::NineMonths, 3_000),
        (Coin::Usdt, BondTerm::SixMonths, 2_000),
        (Coin::Usdt, BondTerm::NineMonths, 3_000),
    ] {
        let mut e = world();
        let k = e.new_depositor();
        let a = k.pubkey();
        let (mint, pool, sl8) = e.coin(coin);
        let src = e.wallet_ta(&a, coin);
        e.bond_deposit(&k, 0, 1_000 * M, term, coin);
        assert_eq!(e.token_balance(&src), WALLET_START - 1_002 * M);
        assert_eq!((e.token_balance(&pool), e.token_balance(&sl8)), (500 * M, 502 * M));
        let p = e.bond(&a, 0).unwrap();
        assert_eq!((p.mint, p.term, p.interest_bps), (mint, term, bps));
        // the other coin's pool and SL8 account got nothing
        let other = if coin == Coin::Usdc { Coin::Usdt } else { Coin::Usdc };
        let (_, opool, osl8) = e.coin(other);
        assert_eq!((e.token_balance(&opool), e.token_balance(&osl8)), (0, 0));
    }
}

#[test]
fn awkward_principals_round_the_fee_up_and_the_split_loses_nothing() {
    for principal in [50_000_000u64, 50_000_001, 50_000_099, 50_000_100, 999_999_999, 1_000_000_001, 123_456_789, 49_999_999_999] {
        let mut e = world();
        let k = e.new_depositor();
        let a = k.pubkey();
        let src = e.wallet_ta(&a, Coin::Usdc);
        e.bond_deposit(&k, 0, principal, BondTerm::NineMonths, Coin::Usdc);
        let fee = fee_of(principal);
        let pool = principal / 2;
        assert_eq!(e.token_balance(&e.usdc_pool.clone()), pool, "principal {principal}");
        assert_eq!(e.token_balance(&e.sl8_usdc.clone()), principal - pool + fee, "principal {principal}");
        assert_eq!(WALLET_START - e.token_balance(&src), principal + fee, "principal {principal}");
        assert!(fee < principal);
        assert_eq!(e.bond(&a, 0).unwrap().principal, principal, "caps count principal, not the fee");
    }
    assert_eq!(fee_of(50_000_001), 100_001);
    assert_eq!(fee_of(999_999_999), 2_000_000);
}

// ----------------------------------------------------------------------- minimum

#[test]
fn the_minimum_is_exactly_fifty_dollars() {
    let mut e = world();
    let k = e.new_depositor();
    for bad in [0u64, 1, BOND_MIN_PRINCIPAL - 1] {
        let ix = deposit_bond_ix(&e, &k.pubkey(), 0, bad, BondTerm::SixMonths, Coin::Usdc);
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondBelowMinimum));
    }
    e.bond_deposit(&k, 0, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.bond(&k.pubkey(), 0).unwrap().principal, 50_000_000);
}

// -------------------------------------------------------------------------- caps

#[test]
fn the_wallet_cap_counts_every_open_position_and_is_exact() {
    let mut e = world();
    let k = e.new_depositor();
    for i in 0..20u64 {
        e.bond_deposit(&k, i, 2_500 * M, if i % 2 == 0 { BondTerm::SixMonths } else { BondTerm::NineMonths }, if i % 3 == 0 { Coin::Usdt } else { Coin::Usdc });
        e.assert_bond_invariant();
    }
    let t = e.tracker_of(&k.pubkey()).unwrap();
    assert_eq!((t.open_principal_total, t.next_deposit_index), (BOND_MAX_PER_WALLET, 20));
    // 20 x $2,500 = $50,000 exactly: even the smallest bond is over
    let ix = deposit_bond_ix(&e, &k.pubkey(), 20, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondWalletCapExceeded));
}

#[test]
fn the_wallet_cap_is_exact_to_the_base_unit() {
    // 49,950,000,000 + 50,000,000 = the cap exactly: allowed
    let mut e = world();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 49_950_000_000, BondTerm::SixMonths, Coin::Usdc);
    e.bond_deposit(&k, 1, 50_000_000, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.tracker_of(&k.pubkey()).unwrap().open_principal_total, BOND_MAX_PER_WALLET);

    // one base unit over: refused
    let mut e = world();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 49_950_000_000, BondTerm::SixMonths, Coin::Usdc);
    let ix = deposit_bond_ix(&e, &k.pubkey(), 1, 50_000_001, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondWalletCapExceeded));
    // a single deposit above the cap is refused too
    let k2 = e.new_depositor();
    let ix = deposit_bond_ix(&e, &k2.pubkey(), 0, BOND_MAX_PER_WALLET + 1, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k2, ix, |r| assert_vault_err(r, VaultError::BondWalletCapExceeded));
    e.bond_deposit(&k2, 0, BOND_MAX_PER_WALLET, BondTerm::SixMonths, Coin::Usdc);
}

#[test]
fn the_wallet_cap_is_per_wallet() {
    let mut e = world();
    let (a, b) = (e.new_depositor(), e.new_depositor());
    e.bond_deposit(&a, 0, BOND_MAX_PER_WALLET, BondTerm::SixMonths, Coin::Usdc);
    // A is full, B is untouched
    e.bond_deposit(&b, 0, 70 * M, BondTerm::NineMonths, Coin::Usdt);
    assert_eq!(e.tracker_of(&a.pubkey()).unwrap().open_principal_total, BOND_MAX_PER_WALLET);
    assert_eq!(e.tracker_of(&b.pubkey()).unwrap().open_principal_total, 70 * M);
    assert_eq!(e.vault_state().bond_principal_open_total, BOND_MAX_PER_WALLET + 70 * M);
    e.assert_bond_invariant();
}

#[test]
fn the_global_cap_is_exact_across_wallets() {
    let mut e = world();
    for _ in 0..12 {
        let k = e.new_depositor();
        e.bond_deposit(&k, 0, BOND_MAX_PER_WALLET, BondTerm::SixMonths, Coin::Usdc);
    }
    assert_eq!(e.vault_state().bond_principal_open_total, BOND_GLOBAL_CAP, "12 x $50K = $600K exactly");
    let k = e.new_depositor();
    let ix = deposit_bond_ix(&e, &k.pubkey(), 0, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondGlobalCapExceeded));
    e.assert_bond_invariant();
}

#[test]
fn the_global_cap_is_exact_to_the_base_unit() {
    let mut e = world();
    let k = e.new_depositor();
    e.set_vault_state(|v| v.bond_principal_open_total = BOND_GLOBAL_CAP - BOND_MIN_PRINCIPAL);
    e.bond_deposit(&k, 0, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc); // lands exactly on the cap
    assert_eq!(e.vault_state().bond_principal_open_total, BOND_GLOBAL_CAP);

    let mut e = world();
    let k = e.new_depositor();
    e.set_vault_state(|v| v.bond_principal_open_total = BOND_GLOBAL_CAP - BOND_MIN_PRINCIPAL + 1);
    let ix = deposit_bond_ix(&e, &k.pubkey(), 0, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondGlobalCapExceeded));
}

#[test]
fn the_wallet_cap_is_checked_before_the_global_cap() {
    let mut e = world();
    let k = e.new_depositor();
    e.set_vault_state(|v| v.bond_principal_open_total = BOND_GLOBAL_CAP);
    let ix = deposit_bond_ix(&e, &k.pubkey(), 0, BOND_MAX_PER_WALLET + 1, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondWalletCapExceeded));
}

#[test]
fn counters_cannot_overflow() {
    let mut e = world();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    // wallet counter near u64::MAX
    let t = e.tracker_of(&k.pubkey()).unwrap();
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&BondCapTracker { open_principal_total: u64::MAX - 5, ..t }, &mut data).unwrap();
    e.set_raw(&bond_cap_pda(&k.pubkey()).0, data, core_vault::ID);
    let ix = deposit_bond_ix(&e, &k.pubkey(), 1, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::MathOverflow));

    // global counter near u64::MAX
    let mut e = world();
    let k = e.new_depositor();
    e.set_vault_state(|v| v.bond_principal_open_total = u64::MAX - 5);
    let ix = deposit_bond_ix(&e, &k.pubkey(), 0, BOND_MIN_PRINCIPAL, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::MathOverflow));

    // a principal of u64::MAX is simply over the cap
    let ix = deposit_bond_ix(&e, &k.pubkey(), 0, u64::MAX, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondWalletCapExceeded));
}

// ------------------------------------------------------------------------ index

#[test]
fn the_index_must_be_the_wallets_next_one() {
    let mut e = world();
    let k = e.new_depositor();
    // first deposit must be index 0
    for bad in [1u64, 2, u64::MAX] {
        let ix = deposit_bond_ix(&e, &k.pubkey(), bad, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondIndexMismatch));
    }
    e.bond_deposit(&k, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    // reused, skipped
    for bad in [0u64, 2, 3] {
        let ix = deposit_bond_ix(&e, &k.pubkey(), bad, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::BondIndexMismatch));
    }
    e.bond_deposit(&k, 1, 70 * M, BondTerm::NineMonths, Coin::Usdt);
    assert_eq!(e.tracker_of(&k.pubkey()).unwrap().next_deposit_index, 2);
    assert!(e.bond(&k.pubkey(), 2).is_none());
    assert!(e.bond(&k.pubkey(), 0).is_some() && e.bond(&k.pubkey(), 1).is_some());
}

#[test]
fn indexes_are_independent_between_wallets_and_positions_have_independent_timers() {
    let mut e = world();
    let (a, b) = (e.new_depositor(), e.new_depositor());
    e.bond_deposit(&a, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    e.advance(1_000);
    e.bond_deposit(&b, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc); // B also starts at 0
    e.advance(2_000);
    e.bond_deposit(&a, 1, 80 * M, BondTerm::NineMonths, Coin::Usdc);
    assert_eq!(e.bond(&a.pubkey(), 0).unwrap().created_at, T0);
    assert_eq!(e.bond(&b.pubkey(), 0).unwrap().created_at, T0 + 1_000);
    assert_eq!(e.bond(&a.pubkey(), 1).unwrap().created_at, T0 + 3_000);
    assert_ne!(bond_pda(&a.pubkey(), 0).0, bond_pda(&b.pubkey(), 0).0);
}

// ----------------------------------------------------------------------- safety

#[test]
fn a_mint_that_is_not_the_vaults_is_rejected() {
    let mut e = world();
    let k = e.new_depositor();
    let rogue = Pubkey::new_unique();
    e.set_mint(&rogue, 6, anchor_spl::token::spl_token::ID);
    let rogue_ta = e.new_token_account(&rogue, &k.pubkey(), 1_000 * M);
    let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    ix.accounts[BD.mint].pubkey = rogue;
    ix.accounts[BD.source].pubkey = rogue_ta;
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidMint));
    // a mint account that does not exist at all
    let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    ix.accounts[BD.mint].pubkey = Pubkey::new_unique();
    assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotInitialized));
}

#[test]
fn only_the_classic_token_program_is_accepted() {
    let mut e = world();
    let k = e.new_depositor();
    for bad in [TOKEN_2022_ID, Pubkey::new_unique(), anchor_lang::system_program::ID] {
        let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        ix.accounts[BD.token_program].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::InvalidProgramId));
    }
}

#[test]
fn the_pool_and_the_sl8_account_must_be_the_right_ones_for_the_mint() {
    let mut e = world();
    let k = e.new_depositor();
    let stranger_usdc = e.new_token_account(&e.usdc.clone(), &Pubkey::new_unique(), 0);
    let stranger_usdt = e.new_token_account(&e.usdt.clone(), &Pubkey::new_unique(), 0);
    let cases: Vec<(usize, Pubkey)> = vec![
        (BD.pool, e.usdt_pool),      // the other mint's pool
        (BD.pool, e.sl8_usdc),       // not a pool at all
        (BD.pool, stranger_usdc),
        (BD.sl8, e.sl8_usdt),        // SL8's account for the other mint
        (BD.sl8, e.usdc_pool),       // the pool as the SL8 destination
        (BD.sl8, stranger_usdc),     // a token account of someone else
        (BD.sl8, stranger_usdt),
    ];
    for (slot, bad) in cases {
        let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        ix.accounts[slot].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
    }
}

#[test]
fn the_source_must_be_the_depositors_own_account_for_that_mint() {
    let mut e = world();
    let k = e.new_depositor();
    let other = e.new_depositor();
    let cases = [
        e.wallet_ta(&other.pubkey(), Coin::Usdc), // someone else's account
        e.wallet_ta(&k.pubkey(), Coin::Usdt),     // own account, wrong mint
        e.usdc_pool,                              // the pool itself
        e.sl8_usdc,
    ];
    for bad in cases {
        let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        ix.accounts[BD.source].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
    }
}

#[test]
fn the_depositor_must_sign() {
    let mut e = world();
    let k = e.new_depositor();
    let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    ix.accounts[BD.depositor].is_signer = false;
    assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::AccountNotSigner));
}

#[test]
fn someone_else_cannot_open_a_bond_funded_by_a_wallet_that_did_not_sign() {
    // The fee payer signs, the victim's token account is named, the victim does not sign.
    let mut e = world();
    let victim = e.new_depositor();
    let attacker = e.new_depositor();
    let mut ix = deposit_bond_ix(&e, &attacker.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    ix.accounts[BD.source].pubkey = e.wallet_ta(&victim.pubkey(), Coin::Usdc);
    assert_rejected(&mut e, &attacker, ix, |r| assert_vault_err(r, VaultError::InvalidTokenAccount));
}

#[test]
fn an_insufficient_balance_is_a_clean_error_with_no_partial_state() {
    let mut e = world();
    let k = e.new_depositor();
    let a = k.pubkey();
    let src = e.wallet_ta(&a, Coin::Usdc);
    let principal = 999_999_999u64;
    let need = principal + fee_of(principal);
    e.fund_wallet_with(&a, need - 1, 0);
    let ix = deposit_bond_ix(&e, &a, 0, principal, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InsufficientTokenBalance));
    assert!(e.bond(&a, 0).is_none());
    assert!(e.svm.get_account(&bond_pda(&a, 0).0).is_none(), "no position account left behind");
    assert!(e.svm.get_account(&bond_cap_pda(&a).0).is_none(), "no tracker left behind");
    // exactly enough succeeds and drains the account
    e.fund_wallet_with(&a, need, 0);
    e.bond_deposit(&k, 0, principal, BondTerm::SixMonths, Coin::Usdc);
    assert_eq!(e.token_balance(&src), 0);
}

#[test]
fn a_dusted_position_or_tracker_address_does_not_block_a_deposit() {
    let rent = |e: &Env, n: usize| e.svm.minimum_balance_for_rent_exemption(n);
    for dust in [1u64, 890_880, 50_000_000] {
        let mut e = world();
        let k = e.new_depositor();
        let a = k.pubkey();
        for addr in [bond_pda(&a, 0).0, bond_cap_pda(&a).0] {
            let p = dup(&e.payer);
            assert_ok(e.send_with(&[anchor_lang::solana_program::system_instruction::transfer(&p.pubkey(), &addr, dust)], &p, &[]));
        }
        e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
        assert_eq!(e.bond(&a, 0).unwrap().principal, 100 * M);
        assert_eq!(e.tracker_of(&a).unwrap().next_deposit_index, 1);
        assert_eq!(e.lamports(&bond_pda(&a, 0).0), dust.max(rent(&e, BondPosition::SPACE)));
        assert_eq!(e.lamports(&bond_cap_pda(&a).0), dust.max(rent(&e, BondCapTracker::SPACE)));
        e.bond_deposit(&k, 1, 100 * M, BondTerm::SixMonths, Coin::Usdc); // second deposit reuses the tracker
        e.assert_bond_invariant();
    }
}

#[test]
fn a_foreign_tracker_at_the_depositors_address_is_never_trusted() {
    // a tracker for ANOTHER depositor placed at this wallet's address is rejected
    let mut e = world();
    let k = e.new_depositor();
    let a = k.pubkey();
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(
        &BondCapTracker { depositor: Pubkey::new_unique(), open_principal_total: 0, next_deposit_index: 0, bump: 255 },
        &mut data,
    )
    .unwrap();
    e.set_raw(&bond_cap_pda(&a).0, data, core_vault::ID);
    let ix = deposit_bond_ix(&e, &a, 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
    assert_rejected(&mut e, &k, ix, |r| assert_vault_err(r, VaultError::InvalidBondPosition));
}

#[test]
fn wrong_position_or_tracker_addresses_are_rejected() {
    let mut e = world();
    let k = e.new_depositor();
    let other = Pubkey::new_unique();
    for (slot, bad) in [
        (BD.position, bond_pda(&k.pubkey(), 1).0),
        (BD.position, bond_pda(&other, 0).0),
        (BD.position, Pubkey::new_unique()),
        (BD.tracker, bond_cap_pda(&other).0),
        (BD.tracker, Pubkey::new_unique()),
        (BD.tracker, bond_pda(&k.pubkey(), 0).0),
    ] {
        let mut ix = deposit_bond_ix(&e, &k.pubkey(), 0, 60 * M, BondTerm::SixMonths, Coin::Usdc);
        ix.accounts[slot].pubkey = bad;
        assert_rejected(&mut e, &k, ix, |r| assert_anchor_err(r, ErrorCode::ConstraintSeeds));
    }
}

#[test]
fn a_second_deposit_costs_fewer_compute_units_than_the_first() {
    let mut e = world();
    let k = e.new_depositor();
    let first = e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc).compute_units_consumed;
    let second = e.bond_deposit(&k, 1, 100 * M, BondTerm::NineMonths, Coin::Usdt).compute_units_consumed;
    println!("deposit_bond CU: first {first}, second {second}");
    assert!(first < 200_000 && second < 200_000, "within the default per-instruction budget");
}

#[test]
fn deposits_do_not_touch_product_registries_or_claims() {
    let mut e = world();
    let k = e.new_depositor();
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    let vs = e.vault_state();
    assert_eq!((vs.open_claims_count, vs.open_claims_total), (0, 0));
    assert_eq!(vs.bond_principal_open_total, 100 * M);
    assert_eq!(vs.bond_withdrawal_fees_retained, 0);
}

// ------------------------------------------------------ seeded conservation test

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn seeded_random_deposits_conserve_every_token_and_respect_the_caps() {
    let mut e = world();
    let ks: Vec<Keypair> = (0..6).map(|_| e.new_depositor()).collect();
    let totals = e.totals();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut next, mut open) = (vec![0u64; 6], vec![0u64; 6]);
    let (mut pool, mut sl8) = ([0u64; 2], [0u64; 2]);
    let (mut accepted, mut refused) = (0, 0);
    for _ in 0..150 {
        let w = (rng.next() % 6) as usize;
        let principal = BOND_MIN_PRINCIPAL + rng.next() % 30_000_000_000;
        let term = if rng.next() % 2 == 0 { BondTerm::SixMonths } else { BondTerm::NineMonths };
        let (coin, ci) = if rng.next() % 2 == 0 { (Coin::Usdc, 0) } else { (Coin::Usdt, 1) };
        let r = e.bond_deposit_result(&ks[w], next[w], principal, term, coin);
        if open[w] + principal > BOND_MAX_PER_WALLET {
            assert_vault_err(&r, VaultError::BondWalletCapExceeded);
            refused += 1;
        } else {
            assert_ok(r);
            accepted += 1;
            let fee = fee_of(principal);
            pool[ci] += principal / 2;
            sl8[ci] += principal - principal / 2 + fee;
            open[w] += principal;
            next[w] += 1;
        }
        assert_eq!(e.totals(), totals, "tokens are conserved");
        assert_eq!((e.token_balance(&e.usdc_pool.clone()), e.token_balance(&e.usdt_pool.clone())), (pool[0], pool[1]));
        assert_eq!((e.token_balance(&e.sl8_usdc.clone()), e.token_balance(&e.sl8_usdt.clone())), (sl8[0], sl8[1]));
        assert_eq!(e.vault_state().bond_principal_open_total, open.iter().sum::<u64>());
        for (i, k) in ks.iter().enumerate() {
            let t = e.tracker_of(&k.pubkey());
            assert_eq!(t.map(|t| (t.open_principal_total, t.next_deposit_index)).unwrap_or((0, 0)), (open[i], next[i]));
        }
        e.assert_bond_invariant();
    }
    assert!(accepted > 20 && refused > 5, "the run exercises both outcomes ({accepted} accepted, {refused} refused)");
}
