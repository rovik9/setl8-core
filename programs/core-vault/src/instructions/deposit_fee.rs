use anchor_lang::prelude::*;

use crate::constants::{PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED};
use crate::errors::VaultError;
use crate::instructions::assert_sector_authority;
use crate::state::{ProductRegistry, TraderState, TraderStatus};

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
    pub product_registry: Account<'info, ProductRegistry>,

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
    pub trader_state: Account<'info, TraderState>,

    /// Pays rent for `trader_state`. The sector authority PDA holds no
    /// lamports, so the sector passes a funded signer here.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

/// Creates the `TraderState` for a new challenge purchase. The fee transfer
/// itself is the proof of purchase, so there is no separate confirmation call.
///
/// NOT YET DONE (needs the dual-mint VaultState from the next module): the
/// actual token movement and the 35%/65% split. Everything that decides who
/// may buy what -- tier validation, challenge-id uniqueness, per-wallet
/// keying -- is enforced here.
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

    // TODO Module 2b: move `amount` into the vault's payout pool per
    // fee_split_bps (dual USDC/USDT pools).
    Ok(())
}
