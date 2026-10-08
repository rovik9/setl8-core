use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use setl8_shared_interfaces::PayoutOutcome;

use crate::constants::{
    PAYOUT_CLAIM_SEED, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, TRADER_STATE_SEED,
    VAULT_STATE_SEED,
};
use crate::errors::VaultError;
use crate::state::{PayoutClaim, ProductRegistry, TraderState, TraderStatus, VaultState};
use crate::utils::{assert_sector_authority, create_pda_account};

#[derive(Accounts)]
#[instruction(trader_wallet: Pubkey, amount: u64, product_program_id: Pubkey, challenge_id: u64, proposed_request_id: u64)]
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

    // ---- payout queue (appended; everything above keeps its position) ----
    // Large accounts are boxed: an unboxed `try_accounts` frame past 4,096
    // bytes silently corrupts memory on SBF.
    /// Holds the open-claims counters and the current cycle id.
    #[account(
        mut,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump = vault_state.bump,
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    /// CHECK: the new claim's PDA. Deliberately NOT created with Anchor `init`:
    /// `init` would create it before the handler runs, and the stale path
    /// returns Ok (so `Abandoned` persists), which would leave an empty claim
    /// account behind. The handler creates it by hand, only once every check
    /// has passed.
    #[account(
        mut,
        seeds = [PAYOUT_CLAIM_SEED, trader_state.key().as_ref(), &proposed_request_id.to_le_bytes()],
        bump,
    )]
    pub payout_claim: UncheckedAccount<'info>,

    /// Pays the claim account's rent. A sector program passes a signer it
    /// controls (typically the same payer it uses for `deposit_fee`).
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

/// Checks the challenge, then books one payout against its cap and records it
/// as a `PayoutClaim` owed to the trader. **No tokens move here**: a heartbeat
/// cycle (`settle_claims`) pays claims later, pro rata.
///
/// If the challenge is past its inactivity window, this flips it to
/// `Abandoned` and returns **Ok with `PayoutOutcome::Abandoned`**, creating no
/// claim. It must not return an error here: a failed transaction reverts every
/// write, so the `Abandoned` status would never be stored. The sector program
/// must read the return data before telling anyone a payout was queued.
///
/// Otherwise (`PayoutOutcome::Paid`, meaning "accepted and queued") the claim is
/// created with `owed = amount`, and `open_claims_count` / `open_claims_total`
/// grow. The claim is created last, after every check, by hand (see
/// `payout_claim`).
pub fn request_payout(
    ctx: Context<RequestPayout>,
    trader_wallet: Pubkey,
    amount: u64,
    product_program_id: Pubkey,
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

    let vs = &mut ctx.accounts.vault_state;
    let open_claims_count = vs.open_claims_count.checked_add(1).ok_or(VaultError::MathOverflow)?;
    let open_claims_total = vs.open_claims_total.checked_add(amount).ok_or(VaultError::MathOverflow)?;

    ts.payout_count = expected_request_id;
    ts.touch(now, paused_now);
    registry.total_requests_emitted = registry
        .total_requests_emitted
        .checked_add(1)
        .ok_or(VaultError::MathOverflow)?;

    if ts.payout_count >= registry.max_payout_count {
        ts.status = TraderStatus::Graduated;
    }

    vs.open_claims_count = open_claims_count;
    vs.open_claims_total = open_claims_total;

    let claim = PayoutClaim {
        trader_wallet,
        trader_state: ts.key(),
        product_program_id,
        request_id: proposed_request_id,
        owed: amount,
        created_in_cycle: vs.cycle_id,
        last_settled_cycle: 0,
        bump: ctx.bumps.payout_claim,
    };
    create_claim_account(
        &ctx.accounts.payer,
        &ctx.accounts.payout_claim,
        &ctx.accounts.system_program,
        &claim,
        ts.key(),
    )?;

    set_return_data(&[PayoutOutcome::Paid as u8]);
    Ok(())
}

/// Creates the claim account (safe against a pre-funded address) and writes
/// the Anchor discriminator plus the Borsh data.
fn create_claim_account<'info>(
    payer: &Signer<'info>,
    payout_claim: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    claim: &PayoutClaim,
    trader_state: Pubkey,
) -> Result<()> {
    let id = claim.request_id.to_le_bytes();
    let bump = [claim.bump];
    create_pda_account(
        &payer.to_account_info(),
        &payout_claim.to_account_info(),
        &system_program.to_account_info(),
        PayoutClaim::SPACE,
        &crate::ID,
        &[PAYOUT_CLAIM_SEED, trader_state.as_ref(), &id, &bump],
    )?;

    let mut data = payout_claim.try_borrow_mut_data()?;
    let mut out: &mut [u8] = &mut data;
    claim.try_serialize(&mut out)
}
