use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::{PoolSide, VaultState};
use crate::utils::{transfer_from_pool, withdrawable};

#[derive(Accounts)]
#[instruction(pool: PoolSide)]
pub struct AdminWithdrawMarketingFunds<'info> {
    #[account(address = SL8_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub sl8_admin: Signer<'info>,

    #[account(address = ROV_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub rov_admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// The mint of the chosen side.
    #[account(
        constraint = mint.key() == vault_state.mint_of(pool) @ VaultError::InvalidMint,
    )]
    pub mint: Box<Account<'info, Mint>>,

    /// The payout pool of the chosen side.
    #[account(
        mut,
        constraint = pool_token_account.key() == vault_state.pool_of(pool) @ VaultError::InvalidTokenAccount,
    )]
    pub pool_token_account: Box<Account<'info, TokenAccount>>,

    /// The ONLY possible destination: a `mint` token account owned by
    /// `vault_state.sl8_wallet` (validated exactly as `deposit_fee` validates SL8's
    /// destination; a frozen account fails inside the token program). There is no
    /// destination argument and no remaining account.
    #[account(
        mut,
        constraint = sl8_token_account.owner == vault_state.sl8_wallet && sl8_token_account.mint == mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub sl8_token_account: Box<Account<'info, TokenAccount>>,

    /// Classic SPL Token only: Token-2022 fails the program-id check.
    pub token_program: Program<'info, Token>,
}

/// **DOCUMENTED EXCEPTION to "no admin key on money".** Both admins together (SL8 and
/// Rov, a 2-of-2) may move `amount` of one pool's token to the SL8 wallet's token
/// account for that mint.
///
/// * The destination is fixed: the SL8 wallet's token account. Nothing else can be
///   named.
/// * Per pool, using the LIVE balance:
///   `reserve = max(stored_floor, ceil(live * 25%))` and
///   `withdrawable = live - reserve`. `amount` must be `> 0` (`ZeroAmount`) and
///   `<= withdrawable` (`WithdrawalExceedsReserve`). The live-25% term matters: the
///   stored floor is 0 until the first `finalize_heartbeat`.
/// * There is NO deduction for open claims, bond liabilities or the current cycle,
///   by the founder's explicit choice: it is plain "75% of the pool balance, 25%
///   floor". It can be called repeatedly and at any time (no cycle gating), so
///   repeated withdrawals shrink a pool geometrically (balance * 0.25^n), and queued
///   claims then settle pro rata against what is left (`settle_claims` already caps
///   every payment at the live balance). A product pause does not block it: it is a
///   vault-level instruction.
///
/// All checks run before the transfer; the transfer is signed by the `VaultState`
/// PDA (the pool's token authority).
pub fn admin_withdraw_marketing_funds(ctx: Context<AdminWithdrawMarketingFunds>, pool: PoolSide, amount: u64) -> Result<()> {
    ctx.accounts.pool_token_account.reload()?;
    let live = ctx.accounts.pool_token_account.amount;
    let floor = ctx.accounts.vault_state.floor_of(pool);
    let reserve = crate::utils::reserve(live, floor)?;
    let available = withdrawable(live, floor)?;
    msg!("admin_withdraw_marketing_funds: pool={:?} amount={} withdrawable={} reserve={}", pool, amount, available, reserve);

    require!(amount > 0, VaultError::ZeroAmount);
    require!(amount <= available, VaultError::WithdrawalExceedsReserve);

    let vs = &mut ctx.accounts.vault_state;
    match pool {
        PoolSide::Usdc => {
            vs.marketing_withdrawn_usdc = vs.marketing_withdrawn_usdc.checked_add(amount).ok_or(VaultError::MathOverflow)?
        }
        PoolSide::Usdt => {
            vs.marketing_withdrawn_usdt = vs.marketing_withdrawn_usdt.checked_add(amount).ok_or(VaultError::MathOverflow)?
        }
    }

    transfer_from_pool(
        &ctx.accounts.vault_state,
        &ctx.accounts.token_program,
        &ctx.accounts.pool_token_account,
        &ctx.accounts.mint,
        &ctx.accounts.sl8_token_account.to_account_info(),
        amount,
    )
}
