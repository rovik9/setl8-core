//! Fee / reset payments: one stablecoin payment from the trader, split between
//! the payout pool and the SL8 wallet.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

use crate::constants::{BPS_DENOMINATOR, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::VaultState;

/// `(pool_amount, sl8_amount)` for a payment of `amount`.
///
/// `pool_amount = floor(amount * fee_split_bps / 10_000)` (u128 math);
/// `sl8_amount` is the exact remainder, so the two always sum to `amount` and
/// no base unit is created or lost. A `fee_split_bps` above 10_000 would make
/// the pool's share exceed the payment, so it fails closed.
pub fn split_amount(amount: u64, fee_split_bps: u16) -> Result<(u64, u64)> {
    let pool = (amount as u128)
        .checked_mul(fee_split_bps as u128)
        .ok_or(VaultError::MathOverflow)?
        / BPS_DENOMINATOR;
    let pool = u64::try_from(pool).map_err(|_| error!(VaultError::MathOverflow))?;
    let sl8 = amount.checked_sub(pool).ok_or(VaultError::InvalidFeeSplit)?;
    Ok((pool, sl8))
}

/// Validation half, run BEFORE any state is written: the split, and that the
/// trader can actually pay.
pub fn plan_payment(amount: u64, fee_split_bps: u16, trader_token_account: &TokenAccount) -> Result<(u64, u64)> {
    let (pool, sl8) = split_amount(amount, fee_split_bps)?;
    require!(trader_token_account.amount >= amount, VaultError::InsufficientTokenBalance);
    Ok((pool, sl8))
}

/// The accounts one payment moves tokens between. All of them were already
/// validated by the instruction's `Accounts` constraints.
pub struct Payment<'a, 'info> {
    pub trader: &'a Signer<'info>,
    pub trader_token_account: &'a Account<'info, TokenAccount>,
    pub mint: &'a Account<'info, Mint>,
    pub pool_token_account: &'a Account<'info, TokenAccount>,
    pub sl8_token_account: &'a Account<'info, TokenAccount>,
    pub token_program: &'a Program<'info, Token>,
}

impl<'a, 'info> Payment<'a, 'info> {
    /// trader -> pool (`pool_amount`), trader -> SL8 (`sl8_amount`), with the
    /// trader as authority. `transfer_checked` enforces the mint's decimals.
    /// A zero-amount leg is skipped instead of failing.
    pub fn execute(&self, pool_amount: u64, sl8_amount: u64) -> Result<()> {
        self.leg(self.pool_token_account, pool_amount)?;
        self.leg(self.sl8_token_account, sl8_amount)
    }

    fn leg(&self, to: &Account<'info, TokenAccount>, amount: u64) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        token::transfer_checked(
            CpiContext::new(
                self.token_program.to_account_info(),
                TransferChecked {
                    from: self.trader_token_account.to_account_info(),
                    mint: self.mint.to_account_info(),
                    to: to.to_account_info(),
                    authority: self.trader.to_account_info(),
                },
            ),
            amount,
            self.mint.decimals,
        )
    }
}

/// pool -> `to`, signed by the `VaultState` PDA (the pool's token authority).
/// `transfer_checked` enforces the mint's decimals. Used for every transfer OUT of a
/// pool: claim settlement and the admin marketing withdrawal.
pub fn transfer_from_pool<'info>(
    vault_state: &Account<'info, VaultState>,
    token_program: &Program<'info, Token>,
    pool: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    to: &AccountInfo<'info>,
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
                to: to.clone(),
                authority: vault_state.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}
