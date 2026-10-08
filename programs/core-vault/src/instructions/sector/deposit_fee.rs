use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, TRADER_STATE_SEED, VAULT_STATE_SEED,
};
use crate::errors::VaultError;
use crate::utils::{assert_sector_authority, plan_payment, Payment};
use crate::state::{ProductRegistry, TraderState, TraderStatus, VaultState};

#[derive(Accounts)]
#[instruction(amount: u64, product_program_id: Pubkey, challenge_id: u64, trader_wallet: Pubkey)]
pub struct DepositFee<'info> {
    /// CPI-auth identity: the calling sector program's own PDA. See
    /// `assert_sector_authority` for what a valid signature here proves.
    pub sector_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [PRODUCT_REGISTRY_SEED, product_program_id.as_ref()],
        bump = product_registry.bump,
    )]
    pub product_registry: Box<Account<'info, ProductRegistry>>,

    /// The new challenge's record. `init` rejects a reused `challenge_id`.
    #[account(
        init,
        payer = payer,
        space = TraderState::SPACE,
        seeds = [
            TRADER_STATE_SEED,
            product_program_id.as_ref(),
            trader_wallet.as_ref(),
            &challenge_id.to_le_bytes(),
        ],
        bump,
    )]
    pub trader_state: Box<Account<'info, TraderState>>,

    /// Pays rent for `trader_state`. The sector authority PDA holds no
    /// lamports, so the sector passes a funded signer here.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,

    // ---- token movement (appended; everything above keeps its position) ----
    #[account(
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// The paying trader. Must sign and must be the `trader_wallet` argument.
    #[account(address = trader_wallet @ VaultError::TraderWalletMismatch)]
    pub trader: Signer<'info>,

    /// The trader's own token account for `mint` (source of the payment).
    #[account(
        mut,
        constraint = trader_token_account.owner == trader.key() && trader_token_account.mint == mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub trader_token_account: Box<Account<'info, TokenAccount>>,

    /// The stablecoin being paid: exactly one of the vault's two mints.
    #[account(
        constraint = mint.key() == vault_state.usdc_mint || mint.key() == vault_state.usdt_mint
            @ VaultError::InvalidMint,
    )]
    pub mint: Box<Account<'info, Mint>>,

    /// The vault's payout pool for `mint`.
    #[account(
        mut,
        constraint = Some(pool_token_account.key()) == vault_state.pool_for(&mint.key())
            @ VaultError::InvalidTokenAccount,
    )]
    pub pool_token_account: Box<Account<'info, TokenAccount>>,

    /// SL8's destination: any `mint` token account owned by `vault_state.sl8_wallet`.
    #[account(
        mut,
        constraint = sl8_token_account.owner == vault_state.sl8_wallet && sl8_token_account.mint == mint.key()
            @ VaultError::InvalidTokenAccount,
    )]
    pub sl8_token_account: Box<Account<'info, TokenAccount>>,

    /// Classic SPL Token only: Token-2022 fails the program-id check.
    pub token_program: Program<'info, Token>,
}

/// Creates the `TraderState` for a new challenge purchase. The fee transfer
/// itself is the proof of purchase, so there is no separate confirmation call.
///
/// Also takes the payment: `amount` base units of USDC or USDT move from the
/// trader's own token account, `floor(amount * fee_split_bps / 10_000)` to the
/// payout pool and the exact remainder to the SL8 wallet. The trader must sign.
/// All validation and state writes happen before the two token CPIs.
pub fn deposit_fee(
    ctx: Context<DepositFee>,
    amount: u64,
    product_program_id: Pubkey,
    challenge_id: u64,
    trader_wallet: Pubkey,
    account_size: u64,
) -> Result<()> {
    let registry = &ctx.accounts.product_registry;
    assert_sector_authority(&ctx.accounts.sector_authority.key(), &registry.product_program_id)?;
    require!(registry.active, VaultError::ProductNotActive);

    // (account_size, amount) must be an exact registered tier.
    require!(
        registry
            .challenge_sizes
            .iter()
            .any(|t| t.size == account_size && t.cost == amount),
        VaultError::InvalidChallengeTier
    );

    let (pool_amount, sl8_amount) =
        plan_payment(amount, registry.fee_split_bps, &ctx.accounts.trader_token_account)?;

    let now = Clock::get()?.unix_timestamp;
    let paused_now = registry.paused_secs_at(now);

    let ts = &mut ctx.accounts.trader_state;
    ts.trader_wallet = trader_wallet;
    ts.product_program_id = product_program_id;
    ts.challenge_id = challenge_id;
    ts.account_size = account_size;
    ts.payout_count = 0;
    ts.status = TraderStatus::Active;
    ts.last_activity_timestamp = now;
    ts.paused_secs_snapshot = paused_now;
    ts.reset_used = false;
    ts.bump = ctx.bumps.trader_state;

    Payment {
        trader: &ctx.accounts.trader,
        trader_token_account: &ctx.accounts.trader_token_account,
        mint: &ctx.accounts.mint,
        pool_token_account: &ctx.accounts.pool_token_account,
        sl8_token_account: &ctx.accounts.sl8_token_account,
        token_program: &ctx.accounts.token_program,
    }
    .execute(pool_amount, sl8_amount)
}
