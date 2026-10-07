use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};
use setl8_shared_interfaces::PayoutOutcome;

use crate::constants::{
    PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, TRADER_STATE_SEED, VAULT_STATE_SEED,
};
use crate::errors::VaultError;
use crate::instructions::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus, VaultState};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, amount: u64, product_program_id: Pubkey, challenge_id: u64)]
pub struct RequestPayout<'info> {
    /// CPI-auth identity: the calling sector program's own PDA. See
    /// `assert_sector_authority` for what a valid signature here proves.
    pub sector_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Box<Account<'info, ProductRegistry>>,

    #[account(
        mut,
        seeds = [
            TRADER_STATE_SEED,
            product_program_id.as_ref(),
            trader_wallet.as_ref(),
            &challenge_id.to_le_bytes(),
        ],
        bump = trader_state.bump,
    )]
    pub trader_state: Box<Account<'info, TraderState>>,

    // ---- token movement (appended; everything above keeps its position) ----
    // Large accounts are boxed: an unboxed `try_accounts` frame past 4,096
    // bytes silently corrupts memory on SBF.
    #[account(
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    #[account(address = vault_state.usdc_mint @ VaultError::InvalidMint)]
    pub usdc_mint: Box<Account<'info, Mint>>,

    #[account(address = vault_state.usdt_mint @ VaultError::InvalidMint)]
    pub usdt_mint: Box<Account<'info, Mint>>,

    #[account(mut, address = vault_state.usdc_pool @ VaultError::InvalidTokenAccount)]
    pub usdc_pool: Box<Account<'info, TokenAccount>>,

    #[account(mut, address = vault_state.usdt_pool @ VaultError::InvalidTokenAccount)]
    pub usdt_pool: Box<Account<'info, TokenAccount>>,

    /// The trader's own USDC account. Checked even when USDT is the pool paid
    /// from: a trader without BOTH accounts cannot be paid.
    #[account(
        mut,
        constraint = trader_usdc_account.owner == trader_wallet && trader_usdc_account.mint == usdc_mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub trader_usdc_account: Box<Account<'info, TokenAccount>>,

    /// The trader's own USDT account (see `trader_usdc_account`).
    #[account(
        mut,
        constraint = trader_usdt_account.owner == trader_wallet && trader_usdt_account.mint == usdt_mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub trader_usdt_account: Box<Account<'info, TokenAccount>>,

    /// Classic SPL Token only.
    pub token_program: Program<'info, Token>,
}

/// Checks the challenge, then books one payout against its cap.
///
/// If the challenge is past its inactivity window, this flips it to
/// `Abandoned` and returns **Ok with `PayoutOutcome::Abandoned`**, paying
/// nothing. It must not return an error here: a failed transaction reverts
/// every write, so the `Abandoned` status would never be stored. The sector
/// program must read the return data before telling anyone they were paid.
///
/// Otherwise the payout is paid in tokens from exactly ONE pool: the pool
/// (USDC or USDT) with the larger balance, USDC on a tie, straight to the
/// trader's own account for that mint. It is never split across pools, never
/// falls back to the smaller pool, and never sums the two: if the larger pool
/// holds less than `amount` the call fails with `InsufficientPoolBalance` and
/// every write reverts. Reserve floors are not consulted (they only bind the
/// future admin withdrawal). The pool is a PDA-owned token account, so the
/// transfer is signed with the `VaultState` seeds. The single transfer CPI is
/// the last thing the handler does.
pub fn request_payout(
    ctx: Context<RequestPayout>,
    _trader_wallet: Pubkey,
    amount: u64,
    _product_program_id: Pubkey,
    _challenge_id: u64,
    proposed_request_id: u64,
) -> Result<()> {
    let registry = &mut ctx.accounts.product_registry;
    assert_sector_authority(&ctx.accounts.sector_authority.key(), &registry.product_program_id)?;
    require!(registry.active, VaultError::ProductNotActive);
    require!(amount > 0, VaultError::ZeroAmount);

    let ts = &mut ctx.accounts.trader_state;
    require!(ts.status == TraderStatus::Active, VaultError::InvalidTraderStatus);

    let now = Clock::get()?.unix_timestamp;
    let paused_now = registry.paused_secs_at(now);

    if ts.is_stale(now, paused_now) {
        ts.status = TraderStatus::Abandoned;
        set_return_data(&[PayoutOutcome::Abandoned as u8]);
        return Ok(());
    }

    require!(ts.payout_count < registry.max_payout_count, VaultError::PayoutCapReached);

    // Mutual agreement: the vault, not the sector, decides the next id.
    let expected_request_id = ts.payout_count.checked_add(1).ok_or(VaultError::MathOverflow)?;
    require!(proposed_request_id == expected_request_id, VaultError::RequestIdMismatch);

    // One pool only: the larger; a tie goes to USDC.
    let usdc_balance = ctx.accounts.usdc_pool.amount;
    let usdt_balance = ctx.accounts.usdt_pool.amount;
    let from_usdc = usdc_balance >= usdt_balance;
    let pool_balance = if from_usdc { usdc_balance } else { usdt_balance };
    require!(pool_balance >= amount, VaultError::InsufficientPoolBalance);

    ts.payout_count = expected_request_id;
    ts.touch(now, paused_now);
    registry.total_requests_emitted = registry
        .total_requests_emitted
        .checked_add(1)
        .ok_or(VaultError::MathOverflow)?;

    if ts.payout_count >= registry.max_payout_count {
        ts.status = TraderStatus::Graduated;
    }

    let a = &ctx.accounts;
    if from_usdc {
        transfer_from_pool(&a.usdc_pool, &a.usdc_mint, &a.trader_usdc_account, &a.vault_state, &a.token_program, amount)?;
    } else {
        transfer_from_pool(&a.usdt_pool, &a.usdt_mint, &a.trader_usdt_account, &a.vault_state, &a.token_program, amount)?;
    }

    set_return_data(&[PayoutOutcome::Paid as u8]);
    Ok(())
}

/// pool -> trader, signed by the `VaultState` PDA (the pool's token
/// authority). `transfer_checked` enforces the mint's decimals.
fn transfer_from_pool<'info>(
    pool: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    to: &Account<'info, TokenAccount>,
    vault_state: &Account<'info, VaultState>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let seeds: &[&[u8]] = &[
        VAULT_STATE_SEED,
        SL8_ADMIN_PUBKEY.as_ref(),
        ROV_ADMIN_PUBKEY.as_ref(),
        &[vault_state.bump],
    ];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: pool.to_account_info(),
                mint: mint.to_account_info(),
                to: to.to_account_info(),
                authority: vault_state.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}
