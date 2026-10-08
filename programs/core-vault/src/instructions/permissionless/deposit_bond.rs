use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    BOND_CAP_SEED, BOND_GLOBAL_CAP, BOND_MAX_PER_WALLET, BOND_MIN_PRINCIPAL, BOND_SEED, ROV_ADMIN_PUBKEY,
    SL8_ADMIN_PUBKEY, VAULT_STATE_SEED,
};
use crate::errors::VaultError;
use crate::state::{BondCapTracker, BondPosition, BondTerm, VaultState};
use crate::utils::{create_pda_account_with, plan_deposit, write_account, Payment};

#[derive(Accounts)]
#[instruction(deposit_index: u64)]
pub struct DepositBond<'info> {
    /// The bond holder. Pays the principal and the fee from their own token
    /// account, and the rent of the position and (first time) the tracker.
    #[account(mut)]
    pub depositor: Signer<'info>,

    /// Holds the global open-principal counter.
    #[account(
        mut,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// The depositor's own token account for `mint` (source of the payment).
    #[account(
        mut,
        constraint = depositor_token_account.owner == depositor.key() && depositor_token_account.mint == mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub depositor_token_account: Box<Account<'info, TokenAccount>>,

    /// The stablecoin of the bond: exactly one of the vault's two mints.
    #[account(
        constraint = mint.key() == vault_state.usdc_mint || mint.key() == vault_state.usdt_mint
            @ VaultError::InvalidMint,
    )]
    pub mint: Box<Account<'info, Mint>>,

    /// The vault's payout pool for `mint` (validated exactly as `deposit_fee` does).
    #[account(
        mut,
        constraint = Some(pool_token_account.key()) == vault_state.pool_for(&mint.key())
            @ VaultError::InvalidTokenAccount,
    )]
    pub pool_token_account: Box<Account<'info, TokenAccount>>,

    /// SL8's destination: any `mint` token account owned by `vault_state.sl8_wallet`
    /// (validated exactly as `deposit_fee` does).
    #[account(
        mut,
        constraint = sl8_token_account.owner == vault_state.sl8_wallet && sl8_token_account.mint == mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub sl8_token_account: Box<Account<'info, TokenAccount>>,

    /// CHECK: the new position's PDA, created by hand in the handler after every
    /// check has passed (so a rejected deposit leaves nothing behind).
    #[account(
        mut,
        seeds = [BOND_SEED, depositor.key().as_ref(), &deposit_index.to_le_bytes()],
        bump,
    )]
    pub bond_position: UncheckedAccount<'info>,

    /// CHECK: the depositor's `BondCapTracker` PDA. Read and written by hand: it is
    /// created by the wallet's first deposit and must not use `init_if_needed`,
    /// which is not safe against a pre-funded address.
    #[account(
        mut,
        seeds = [BOND_CAP_SEED, depositor.key().as_ref()],
        bump,
    )]
    pub bond_cap_tracker: UncheckedAccount<'info>,

    /// Classic SPL Token only: Token-2022 fails the program-id check.
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Opens a bond. The depositor (who must sign) pays `principal + bond_fee`, where
/// `bond_fee = ceil(principal * 0.2%)`, in USDC or USDT. The principal is split
/// like `deposit_fee` splits a payment, half (rounded down) into the same-mint
/// payout pool and the remainder to the SL8 wallet, and the whole fee goes to the
/// SL8 wallet too.
///
/// Caps are measured on principal: at least `BOND_MIN_PRINCIPAL`, at most
/// `BOND_MAX_PER_WALLET` open per wallet summed across all its positions, and
/// `BOND_GLOBAL_CAP` open across the vault. `deposit_index` must be the wallet's
/// next index (it never repeats). Every check runs before any account is created
/// or any token moves.
pub fn deposit_bond(ctx: Context<DepositBond>, deposit_index: u64, principal: u64, term: BondTerm) -> Result<()> {
    require!(principal >= BOND_MIN_PRINCIPAL, VaultError::BondBelowMinimum);

    let depositor = ctx.accounts.depositor.key();
    let existing = read_tracker(&ctx.accounts.bond_cap_tracker, &depositor)?;
    let first_deposit = existing.is_none();
    let mut tracker = existing.unwrap_or(BondCapTracker {
        depositor,
        open_principal_total: 0,
        next_deposit_index: 0,
        bump: ctx.bumps.bond_cap_tracker,
    });

    require!(deposit_index == tracker.next_deposit_index, VaultError::BondIndexMismatch);
    let wallet_open = tracker.open_principal_total.checked_add(principal).ok_or(VaultError::MathOverflow)?;
    require!(wallet_open <= BOND_MAX_PER_WALLET, VaultError::BondWalletCapExceeded);
    let global_open = ctx
        .accounts
        .vault_state
        .bond_principal_open_total
        .checked_add(principal)
        .ok_or(VaultError::MathOverflow)?;
    require!(global_open <= BOND_GLOBAL_CAP, VaultError::BondGlobalCapExceeded);
    let next_index = deposit_index.checked_add(1).ok_or(VaultError::MathOverflow)?;

    let plan = plan_deposit(principal)?;
    require!(
        ctx.accounts.depositor_token_account.amount >= plan.total_debit,
        VaultError::InsufficientTokenBalance
    );

    // ---- state: tracker, position, global counter
    tracker.open_principal_total = wallet_open;
    tracker.next_deposit_index = next_index;
    let tracker_info = ctx.accounts.bond_cap_tracker.to_account_info();
    if first_deposit {
        create_pda_account_with(
            &ctx.accounts.depositor.to_account_info(),
            &tracker_info,
            &ctx.accounts.system_program.to_account_info(),
            BondCapTracker::SPACE,
            &[BOND_CAP_SEED, depositor.as_ref(), &[tracker.bump]],
            &tracker,
        )?;
    } else {
        write_account(&tracker_info, &tracker)?;
    }

    let position = BondPosition {
        depositor,
        deposit_index,
        mint: ctx.accounts.mint.key(),
        principal,
        term,
        interest_bps: term.interest_bps(),
        created_at: Clock::get()?.unix_timestamp,
        bump: ctx.bumps.bond_position,
    };
    create_pda_account_with(
        &ctx.accounts.depositor.to_account_info(),
        &ctx.accounts.bond_position.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        BondPosition::SPACE,
        &[BOND_SEED, depositor.as_ref(), &deposit_index.to_le_bytes(), &[position.bump]],
        &position,
    )?;
    ctx.accounts.vault_state.bond_principal_open_total = global_open;

    // ---- tokens, last: pool share, then SL8's share plus the whole fee
    Payment {
        trader: &ctx.accounts.depositor,
        trader_token_account: &ctx.accounts.depositor_token_account,
        mint: &ctx.accounts.mint,
        pool_token_account: &ctx.accounts.pool_token_account,
        sl8_token_account: &ctx.accounts.sl8_token_account,
        token_program: &ctx.accounts.token_program,
    }
    .execute(plan.pool, plan.sl8)
}

/// The depositor's tracker, if one exists. A tracker address is a PDA of this
/// program, so it is either ours (a real tracker) or still a system account (never
/// created, possibly dusted with lamports); anything else is rejected.
fn read_tracker(info: &UncheckedAccount, depositor: &Pubkey) -> Result<Option<BondCapTracker>> {
    if info.owner == &crate::ID {
        let data = info.try_borrow_data()?;
        let t = BondCapTracker::try_deserialize(&mut &data[..]).map_err(|_| error!(VaultError::InvalidBondPosition))?;
        require_keys_eq!(t.depositor, *depositor, VaultError::InvalidBondPosition);
        Ok(Some(t))
    } else {
        require_keys_eq!(*info.owner, anchor_lang::system_program::ID, VaultError::InvalidBondPosition);
        require!(info.data_is_empty(), VaultError::InvalidBondPosition);
        Ok(None)
    }
}
