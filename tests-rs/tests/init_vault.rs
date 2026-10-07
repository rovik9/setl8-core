//! init_vault: one-time creation of VaultState and the two pool token accounts.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_lang::InstructionData;
use anchor_spl::token::spl_token::{self, state::AccountState};
use common::*;
use core_vault::errors::VaultError;
use core_vault::state::VaultState;
use solana_signer::Signer;

fn init(e: &mut Env) -> litesvm::types::TransactionResult {
    let ix = init_vault_ix(e, e.usdc, e.usdt);
    e.send(ix)
}

#[test]
fn creates_vault_state_and_both_pools() {
    let mut e = Env::new_bare();
    let sl8_before = e.svm.get_balance(&e.sl8.pubkey()).unwrap();
    assert_ok(init(&mut e));

    let vs = e.vault_state();
    assert_eq!(vs.usdc_mint, e.usdc);
    assert_eq!(vs.usdt_mint, e.usdt);
    assert_eq!(vs.usdc_pool, pool_pda(&e.vault, &e.usdc));
    assert_eq!(vs.usdt_pool, pool_pda(&e.vault, &e.usdt));
    assert_eq!(vs.sl8_wallet, core_vault::constants::SL8_ADMIN_PUBKEY);
    assert_eq!((vs.usdc_floor, vs.usdt_floor, vs.floor_updated_at), (0, 0, 0));
    let (_, bump) = Pubkey::find_program_address(
        &[
            b"vault_state",
            core_vault::constants::SL8_ADMIN_PUBKEY.as_ref(),
            core_vault::constants::ROV_ADMIN_PUBKEY.as_ref(),
        ],
        &core_vault::ID,
    );
    assert_eq!(vs.bump, bump);
    let acct = e.svm.get_account(&e.vault).unwrap();
    assert_eq!(acct.owner, core_vault::ID);
    assert_eq!(acct.data.len(), VaultState::SPACE);

    for (pool, mint) in [(e.usdc_pool, e.usdc), (e.usdt_pool, e.usdt)] {
        let a = e.svm.get_account(&pool).unwrap();
        assert_eq!(a.owner, spl_token::ID, "pool is a classic-SPL token account");
        assert_eq!(a.data.len(), spl_token::state::Account::LEN);
        let t = e.token_state(&pool);
        assert_eq!(t.mint, mint);
        assert_eq!(t.owner, e.vault, "token authority is the VaultState PDA");
        assert_eq!(t.amount, 0);
        assert_eq!(t.state, AccountState::Initialized);
        assert!(a.lamports >= e.svm.minimum_balance_for_rent_exemption(a.data.len()));
    }

    // sl8_admin funded all three accounts' rent (the fee payer is a third party)
    let rent = e.svm.minimum_balance_for_rent_exemption(VaultState::SPACE)
        + 2 * e.svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
    assert_eq!(sl8_before - e.svm.get_balance(&e.sl8.pubkey()).unwrap(), rent);
}

#[test]
fn needs_both_exact_admins() {
    let mut e = Env::new_bare();
    let stranger = Pubkey::new_unique();
    e.fund(&stranger); // funded: so `init` can run and the admin constraint is what rejects
    for slot in [0usize, 1] {
        let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
        ix.accounts[slot].pubkey = stranger;
        assert_vault_err(&e.send(ix), VaultError::MissingMultisigSignature);

        let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
        ix.accounts[slot].is_signer = false;
        assert_anchor_err(&e.send(ix), ErrorCode::AccountNotSigner);
    }
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    let (a, b) = (ix.accounts[0].pubkey, ix.accounts[1].pubkey);
    ix.accounts[0].pubkey = b;
    ix.accounts[1].pubkey = a;
    assert_vault_err(&e.send(ix), VaultError::MissingMultisigSignature);
    assert!(e.svm.get_account(&e.vault).is_none(), "every rejected attempt reverted");
    assert!(e.svm.get_account(&e.usdc_pool).is_none());
    // and the genuine pair works afterwards
    assert_ok(init(&mut e));
}

#[test]
fn unfunded_wrong_sl8_key_fails_in_init_before_the_address_check() {
    // Same ordering characteristic as register_product: `init` is paid by the sl8 slot.
    let mut e = Env::new_bare();
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    ix.accounts[0].pubkey = Pubkey::new_unique();
    assert_custom_code(&e.send(ix), 1, "system InsufficientFunds");
    assert!(e.svm.get_account(&e.vault).is_none());
}

#[test]
fn same_mint_twice_is_rejected() {
    let mut e = Env::new_bare();
    let ix = init_vault_ix(&e, e.usdc, e.usdc);
    assert_vault_err(&e.send(ix), VaultError::DuplicateMint);
    assert!(e.svm.get_account(&e.vault).is_none());
    assert!(e.svm.get_account(&e.usdc_pool).is_none());
}

#[test]
fn instruction_args_must_match_the_mint_accounts() {
    let mut e = Env::new_bare();
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    ix.data = core_vault::instruction::InitVault { usdc_mint: Pubkey::new_unique(), usdt_mint: e.usdt }.data();
    assert_vault_err(&e.send(ix), VaultError::InvalidMint);
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    ix.data = core_vault::instruction::InitVault { usdc_mint: e.usdc, usdt_mint: Pubkey::new_unique() }.data();
    assert_vault_err(&e.send(ix), VaultError::InvalidMint);
    assert!(e.svm.get_account(&e.vault).is_none());
}

/// Runs init_vault with `bad` standing in (in turn) as the USDC and USDT mint.
fn bad_mint_in_either_slot(e: &mut Env, bad: Pubkey, want: VaultError) {
    let (good_usdc, good_usdt) = (e.usdc, e.usdt);
    let ix = init_vault_ix(e, bad, good_usdt);
    let name = format!("{want:?}");
    let r = e.send(ix);
    assert_custom_code(&r, u32::from(want.clone()), &format!("bad USDC-slot mint => {name}"));
    let ix = init_vault_ix(e, good_usdc, bad);
    let r = e.send(ix);
    assert_custom_code(&r, u32::from(want), &format!("bad USDT-slot mint => {name}"));
    assert!(e.svm.get_account(&e.vault).is_none(), "nothing may be created on rejection");
}

#[test]
fn a_token_account_is_not_a_mint() {
    let mut e = Env::new_bare();
    let owner = Pubkey::new_unique();
    let ta = e.new_token_account(&e.usdc.clone(), &owner, 0);
    bad_mint_in_either_slot(&mut e, ta, VaultError::InvalidMint);
}

#[test]
fn an_uninitialised_mint_is_invalid() {
    let mut e = Env::new_bare();
    let addr = Pubkey::new_unique();
    e.set_raw(&addr, vec![0u8; spl_token::state::Mint::LEN], spl_token::ID);
    bad_mint_in_either_slot(&mut e, addr, VaultError::InvalidMint);
}

#[test]
fn a_system_owned_or_missing_account_is_not_owned_by_the_token_program() {
    let mut e = Env::new_bare();
    let wallet_acct = Pubkey::new_unique();
    e.fund(&wallet_acct); // system-owned, has lamports
    bad_mint_in_either_slot(&mut e, wallet_acct, VaultError::WrongTokenProgram);
    bad_mint_in_either_slot(&mut e, Pubkey::new_unique(), VaultError::WrongTokenProgram); // never created
}

#[test]
fn a_token_2022_mint_is_rejected_even_when_its_layout_is_valid() {
    let mut e = Env::new_bare();
    let addr = Pubkey::new_unique();
    e.set_mint(&addr, 6, TOKEN_2022_ID);
    bad_mint_in_either_slot(&mut e, addr, VaultError::WrongTokenProgram);
}

#[test]
fn only_six_decimals_are_accepted() {
    for decimals in [0u8, 5, 7, 9] {
        let mut e = Env::new_bare();
        let addr = Pubkey::new_unique();
        e.set_mint(&addr, decimals, spl_token::ID);
        bad_mint_in_either_slot(&mut e, addr, VaultError::InvalidDecimals);
    }
}

#[test]
fn a_second_init_is_rejected_and_changes_nothing() {
    let mut e = Env::new_bare();
    assert_ok(init(&mut e));
    let before = e.svm.get_account(&e.vault).unwrap().data;

    let r = init(&mut e);
    assert_already_in_use(&r);

    // even with a different, perfectly valid mint pair
    let (m1, m2) = (Pubkey::new_unique(), Pubkey::new_unique());
    e.set_mint(&m1, 6, spl_token::ID);
    e.set_mint(&m2, 6, spl_token::ID);
    let ix = init_vault_ix(&e, m1, m2);
    assert_already_in_use(&e.send(ix));
    assert_eq!(e.svm.get_account(&e.vault).unwrap().data, before);
    assert_eq!(e.vault_state().usdc_mint, e.usdc);
}

#[test]
fn prefunded_pool_and_vault_addresses_cannot_block_init() {
    // Anyone can send lamports to a predictable PDA address. `create_account`
    // refuses an address holding lamports, so a naive init would be bricked
    // forever by one dust transfer.
    let mut e = Env::new_bare();
    e.fund(&e.usdc_pool.clone());
    let dust = e.vault;
    e.svm.airdrop(&dust, 1).unwrap();
    e.svm.airdrop(&e.usdt_pool.clone(), 1).unwrap();
    assert_ok(init(&mut e));
    for (pool, mint) in [(e.usdc_pool, e.usdc), (e.usdt_pool, e.usdt)] {
        let a = e.svm.get_account(&pool).unwrap();
        assert_eq!(a.owner, spl_token::ID);
        assert_eq!(e.token_state(&pool).mint, mint);
        assert_eq!(e.token_state(&pool).owner, e.vault);
        assert!(a.lamports >= e.svm.minimum_balance_for_rent_exemption(a.data.len()));
    }
    assert_eq!(e.vault_state().usdc_pool, e.usdc_pool);
}

#[test]
fn wrong_token_program_is_rejected() {
    let mut e = Env::new_bare();
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    ix.accounts[7].pubkey = TOKEN_2022_ID;
    assert_anchor_err(&e.send(ix), ErrorCode::InvalidProgramId);
    assert!(e.svm.get_account(&e.vault).is_none());
}

#[test]
fn pool_addresses_must_be_the_canonical_pdas() {
    let mut e = Env::new_bare();
    for slot in [5usize, 6] {
        let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
        ix.accounts[slot].pubkey = Pubkey::new_unique();
        assert_anchor_err(&e.send(ix), ErrorCode::ConstraintSeeds);
    }
    // the USDT pool address in the USDC slot (right PDA family, wrong mint)
    let mut ix = init_vault_ix(&e, e.usdc, e.usdt);
    ix.accounts[5].pubkey = e.usdt_pool;
    assert_anchor_err(&e.send(ix), ErrorCode::ConstraintSeeds);
    assert!(e.svm.get_account(&e.vault).is_none());
}
