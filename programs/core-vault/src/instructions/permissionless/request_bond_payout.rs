use anchor_lang::prelude::*;

use crate::constants::{BOND_CAP_SEED, BOND_CLAIM_SEED, BOND_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::{BondCapTracker, BondPosition, PayoutClaim, VaultState, CLAIM_KIND_BOND};
use crate::utils::{close_pda_account, create_pda_account_with, plan_withdrawal, write_account};

#[derive(Accounts)]
#[instruction(deposit_index: u64)]
pub struct RequestBondPayout<'info> {
    /// The bond holder. Must sign. Receives the closed position's rent and pays
    /// the new claim's rent.
    #[account(mut)]
    pub depositor: Signer<'info>,

    /// Holds the global counters and the current cycle id.
    #[account(
        mut,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// CHECK: the position being withdrawn. Read by hand so that EVERY problem
    /// (missing, closed, someone else's, wrong index, forged) is one clean
    /// `InvalidBondPosition`. Closed by the handler.
    #[account(mut)]
    pub bond_position: UncheckedAccount<'info>,

    /// CHECK: the depositor's `BondCapTracker`, read and written by hand for the
    /// same reason.
    #[account(mut)]
    pub bond_cap_tracker: UncheckedAccount<'info>,

    /// CHECK: the new claim's PDA (claim kind 1), created by hand after every
    /// check, safe against a pre-funded address.
    #[account(
        mut,
        seeds = [BOND_CLAIM_SEED, depositor.key().as_ref(), &deposit_index.to_le_bytes()],
        bump,
    )]
    pub payout_claim: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

/// Withdraws a bond: closes the position and queues what it is worth as an
/// ordinary `PayoutClaim` (kind 1) that the permissionless heartbeat settles with
/// the same single cycle ratio as every trader claim.
///
/// Only the depositor may call it. Before the hard lock ends it fails with
/// `BondLocked`; from the lock until maturity the gross amount is the principal;
/// at or after maturity it is principal plus the interest. The 0.2% withdrawal fee
/// (rounded up) comes off the gross amount: no tokens move now, so the fee is
/// simply not owed and stays in the pool (`bond_withdrawal_fees_retained` tracks
/// it). The position is closed here, so it can never be withdrawn twice, and the
/// wallet's cap room is freed.
pub fn request_bond_payout(ctx: Context<RequestBondPayout>, deposit_index: u64) -> Result<()> {
    let depositor = ctx.accounts.depositor.key();
    let position = read_position(&ctx.accounts.bond_position, &depositor, deposit_index)?;
    let mut tracker = read_tracker(&ctx.accounts.bond_cap_tracker, &depositor)?;

    // Interest and the lock come from the position's own copy, never from constants.
    let now = Clock::get()?.unix_timestamp;
    let plan = plan_withdrawal(position.principal, position.interest_bps, position.term, position.created_at, now)?;
    require!(plan.net > 0, VaultError::ZeroAmount);

    let vs = &mut ctx.accounts.vault_state;
    tracker.open_principal_total =
        tracker.open_principal_total.checked_sub(position.principal).ok_or(VaultError::MathOverflow)?;
    vs.bond_principal_open_total =
        vs.bond_principal_open_total.checked_sub(position.principal).ok_or(VaultError::MathOverflow)?;
    vs.bond_withdrawal_fees_retained =
        vs.bond_withdrawal_fees_retained.checked_add(plan.fee).ok_or(VaultError::MathOverflow)?;
    vs.open_claims_count = vs.open_claims_count.checked_add(1).ok_or(VaultError::MathOverflow)?;
    vs.open_claims_total = vs.open_claims_total.checked_add(plan.net).ok_or(VaultError::MathOverflow)?;

    let claim = PayoutClaim {
        trader_wallet: depositor,
        trader_state: ctx.accounts.bond_position.key(),
        product_program_id: Pubkey::default(),
        request_id: deposit_index,
        owed: plan.net,
        created_in_cycle: vs.cycle_id,
        last_settled_cycle: 0,
        kind: CLAIM_KIND_BOND,
        bump: ctx.bumps.payout_claim,
    };
    create_pda_account_with(
        &ctx.accounts.depositor.to_account_info(),
        &ctx.accounts.payout_claim.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        PayoutClaim::SPACE,
        &[BOND_CLAIM_SEED, depositor.as_ref(), &deposit_index.to_le_bytes(), &[claim.bump]],
        &claim,
    )?;

    write_account(&ctx.accounts.bond_cap_tracker.to_account_info(), &tracker)?;
    close_pda_account(&ctx.accounts.bond_position.to_account_info(), &ctx.accounts.depositor.to_account_info())
}

/// The position, if it is a genuine open one belonging to `depositor` at `index`.
/// Missing, closed (system-owned), foreign, forged or mismatched: all
/// `InvalidBondPosition`.
fn read_position(info: &UncheckedAccount, depositor: &Pubkey, index: u64) -> Result<BondPosition> {
    require_keys_eq!(*info.owner, crate::ID, VaultError::InvalidBondPosition);
    let data = info.try_borrow_data()?;
    let p = BondPosition::try_deserialize(&mut &data[..]).map_err(|_| error!(VaultError::InvalidBondPosition))?;
    require_keys_eq!(p.depositor, *depositor, VaultError::InvalidBondPosition);
    require!(p.deposit_index == index, VaultError::InvalidBondPosition);
    let canonical = Pubkey::create_program_address(
        &[BOND_SEED, depositor.as_ref(), &index.to_le_bytes(), &[p.bump]],
        &crate::ID,
    )
    .map_err(|_| error!(VaultError::InvalidBondPosition))?;
    require_keys_eq!(info.key(), canonical, VaultError::InvalidBondPosition);
    Ok(p)
}

/// The depositor's tracker, which must exist (every position implies one).
fn read_tracker(info: &UncheckedAccount, depositor: &Pubkey) -> Result<BondCapTracker> {
    require_keys_eq!(*info.owner, crate::ID, VaultError::InvalidBondPosition);
    let data = info.try_borrow_data()?;
    let t = BondCapTracker::try_deserialize(&mut &data[..]).map_err(|_| error!(VaultError::InvalidBondPosition))?;
    require_keys_eq!(t.depositor, *depositor, VaultError::InvalidBondPosition);
    let canonical = Pubkey::create_program_address(&[BOND_CAP_SEED, depositor.as_ref(), &[t.bump]], &crate::ID)
        .map_err(|_| error!(VaultError::InvalidBondPosition))?;
    require_keys_eq!(info.key(), canonical, VaultError::InvalidBondPosition);
    Ok(t)
}
