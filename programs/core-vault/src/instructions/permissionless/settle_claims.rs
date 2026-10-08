use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

use crate::constants::{
    MAX_SETTLE_BATCH, PAYOUT_CLAIM_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED,
};
use crate::errors::VaultError;
use crate::state::{PayoutClaim, VaultState};
use crate::utils::{associated_token_address, cycle_ratio, destination_usable, plan_settlement};

#[derive(Accounts)]
pub struct SettleClaims<'info> {
    /// Anyone. Receives the rent of every claim this call closes.
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
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

    /// Classic SPL Token only.
    pub token_program: Program<'info, Token>,
    // remaining_accounts: triples [payout_claim (mut), trader_usdc_ata (mut),
    // trader_usdt_ata (mut)], 1..=MAX_SETTLE_BATCH of them.
}

/// Permissionless. Settles one batch of claims against the open cycle.
///
/// Every eligible claim gets the cycle's single ratio (see `utils::settlement`):
/// paid from the larger pool first and topped up from the other, straight to the
/// trader's ASSOCIATED token accounts. The unpaid remainder stays owed on the
/// claim for the next cycle; a claim paid in full is closed and its rent goes to
/// the caller. A claim whose (correctly addressed) destinations are unusable is
/// SKIPPED: it counts as processed for this cycle but stays open and owed.
///
/// A WRONG destination address is a hard error instead: the whole transaction
/// reverts, so nobody can mark someone else's claim processed by passing garbage.
///
/// Each claim is re-read from its account and any change is written back inside
/// the loop (not at the end), so a claim listed twice in one batch is caught.
pub fn settle_claims<'info>(ctx: Context<'_, '_, '_, 'info, SettleClaims<'info>>) -> Result<()> {
    require!(ctx.accounts.vault_state.cycle_active, VaultError::NoCycleInProgress);

    let remaining = ctx.remaining_accounts;
    require!(!remaining.is_empty(), VaultError::EmptyBatch);
    require!(remaining.len() % 3 == 0, VaultError::InvalidClaim);
    require!(remaining.len() / 3 <= MAX_SETTLE_BATCH, VaultError::BatchTooLarge);

    let (num, den) = cycle_ratio(
        ctx.accounts.vault_state.cycle_available_snapshot,
        ctx.accounts.vault_state.cycle_owed_snapshot,
    );
    for triple in remaining.chunks_exact(3) {
        settle_one(&mut *ctx.accounts, &triple[0], &triple[1], &triple[2], num, den)?;
    }
    Ok(())
}

fn settle_one<'info>(
    a: &mut SettleClaims<'info>,
    claim_info: &AccountInfo<'info>,
    usdc_ata: &AccountInfo<'info>,
    usdt_ata: &AccountInfo<'info>,
    num: u64,
    den: u64,
) -> Result<()> {
    // ---- the claim: really ours, at its canonical address, eligible, not yet done
    require_keys_eq!(*claim_info.owner, crate::ID, VaultError::InvalidClaim);
    require!(claim_info.is_writable, VaultError::InvalidClaim);
    let mut claim = read_claim(claim_info)?;
    let canonical = Pubkey::create_program_address(
        &[PAYOUT_CLAIM_SEED, claim.trader_state.as_ref(), &claim.request_id.to_le_bytes(), &[claim.bump]],
        &crate::ID,
    )
    .map_err(|_| error!(VaultError::InvalidClaim))?;
    require_keys_eq!(claim_info.key(), canonical, VaultError::InvalidClaim);

    let cycle = a.vault_state.cycle_id;
    require!(claim.created_in_cycle < cycle, VaultError::ClaimNotEligible);
    require!(claim.last_settled_cycle != cycle, VaultError::ClaimAlreadySettled);

    // ---- destinations: must be the trader's ATAs (hard error), then usable (else skip)
    let usdc_mint = a.usdc_mint.key();
    let usdt_mint = a.usdt_mint.key();
    require_keys_eq!(
        usdc_ata.key(),
        associated_token_address(&claim.trader_wallet, &usdc_mint),
        VaultError::InvalidTokenAccount
    );
    require_keys_eq!(
        usdt_ata.key(),
        associated_token_address(&claim.trader_wallet, &usdt_mint),
        VaultError::InvalidTokenAccount
    );

    if !(destination_usable(usdc_ata, &claim.trader_wallet, &usdc_mint)
        && destination_usable(usdt_ata, &claim.trader_wallet, &usdt_mint))
    {
        claim.last_settled_cycle = cycle;
        write_claim(claim_info, &claim)?;
        a.vault_state.cycle_processed_count = bump_one(a.vault_state.cycle_processed_count)?;
        return Ok(());
    }

    // ---- pay, from the LIVE pool balances (they can change during a cycle)
    a.usdc_pool.reload()?;
    a.usdt_pool.reload()?;
    let plan = plan_settlement(claim.owed, num, den, a.usdc_pool.amount, a.usdt_pool.amount)?;
    if plan.from_usdc > 0 {
        transfer_from_pool(a, true, usdc_ata, plan.from_usdc)?;
    }
    if plan.from_usdt > 0 {
        transfer_from_pool(a, false, usdt_ata, plan.from_usdt)?;
    }

    let paid = plan.total();
    claim.owed = claim.owed.checked_sub(paid).ok_or(VaultError::MathOverflow)?;
    claim.last_settled_cycle = cycle;
    let vs = &mut a.vault_state;
    vs.open_claims_total = vs.open_claims_total.checked_sub(paid).ok_or(VaultError::MathOverflow)?;
    vs.cycle_processed_count = bump_one(vs.cycle_processed_count)?;

    if claim.owed == 0 {
        vs.open_claims_count = vs.open_claims_count.checked_sub(1).ok_or(VaultError::MathOverflow)?;
        close_account(claim_info, &a.caller.to_account_info())
    } else {
        write_claim(claim_info, &claim)
    }
}

fn bump_one(n: u64) -> Result<u64> {
    Ok(n.checked_add(1).ok_or(VaultError::MathOverflow)?)
}

/// pool -> trader ATA, signed by the `VaultState` PDA (the pool's token
/// authority). `transfer_checked` enforces the mint's decimals.
fn transfer_from_pool<'info>(a: &SettleClaims<'info>, usdc: bool, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
    let (pool, mint) = if usdc {
        (a.usdc_pool.to_account_info(), &a.usdc_mint)
    } else {
        (a.usdt_pool.to_account_info(), &a.usdt_mint)
    };
    let seeds: &[&[u8]] = &[
        VAULT_STATE_SEED,
        SL8_ADMIN_PUBKEY.as_ref(),
        ROV_ADMIN_PUBKEY.as_ref(),
        &[a.vault_state.bump],
    ];
    token::transfer_checked(
        CpiContext::new_with_signer(
            a.token_program.to_account_info(),
            TransferChecked {
                from: pool,
                mint: mint.to_account_info(),
                to: to.clone(),
                authority: a.vault_state.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

/// Decodes a claim from its account; any layout problem is `InvalidClaim`.
fn read_claim(info: &AccountInfo) -> Result<PayoutClaim> {
    let data = info.try_borrow_data()?;
    PayoutClaim::try_deserialize(&mut &data[..]).map_err(|_| error!(VaultError::InvalidClaim))
}

/// Writes the claim back (discriminator + data) right away.
fn write_claim(info: &AccountInfo, claim: &PayoutClaim) -> Result<()> {
    let mut data = info.try_borrow_mut_data()?;
    let mut out: &mut [u8] = &mut data;
    claim.try_serialize(&mut out)
}

/// Closes `info` with Anchor's `close` semantics: lamports to `destination`,
/// data emptied, owner reassigned to the system program. The account can no
/// longer be read as a claim (owner check) even if it is re-funded in the same
/// transaction, so a second listing of it in a batch fails as `InvalidClaim`.
fn close_account<'info>(info: &AccountInfo<'info>, destination: &AccountInfo<'info>) -> Result<()> {
    let total = destination
        .lamports()
        .checked_add(info.lamports())
        .ok_or(VaultError::MathOverflow)?;
    **destination.try_borrow_mut_lamports()? = total;
    **info.try_borrow_mut_lamports()? = 0;
    info.assign(&anchor_lang::system_program::ID);
    info.resize(0).map_err(Into::into)
}
