use anchor_lang::prelude::*;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::system_program;
use anchor_spl::token::{self, spl_token, InitializeAccount3, Token};

use crate::constants::{POOL_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY, TOKEN_DECIMALS, VAULT_STATE_SEED};
use crate::errors::VaultError;
use crate::state::VaultState;

#[derive(Accounts)]
pub struct InitVault<'info> {
    /// SL8's half of the 2-of-2 admin multisig. Pays rent for `vault_state`
    /// and both pool token accounts.
    #[account(mut, address = SL8_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub sl8_admin: Signer<'info>,

    #[account(address = ROV_ADMIN_PUBKEY @ VaultError::MissingMultisigSignature)]
    pub rov_admin: Signer<'info>,

    /// `init` means a second `init_vault` fails as "account already in use".
    #[account(
        init,
        payer = sl8_admin,
        space = VaultState::SPACE,
        seeds = [VAULT_STATE_SEED, SL8_ADMIN_PUBKEY.as_ref(), ROV_ADMIN_PUBKEY.as_ref()],
        bump,
    )]
    pub vault_state: Account<'info, VaultState>,

    /// CHECK: validated by hand in the handler (owner, layout, decimals) so
    /// every failure is a precise `VaultError` rather than an Anchor or token
    /// program error raised mid-way through account creation.
    pub usdc_mint: UncheckedAccount<'info>,

    /// CHECK: see `usdc_mint`.
    pub usdt_mint: UncheckedAccount<'info>,

    /// CHECK: PDA `[POOL_SEED, vault_state, usdc_mint]`, created and
    /// initialised as a token account by the handler.
    #[account(mut, seeds = [POOL_SEED, vault_state.key().as_ref(), usdc_mint.key().as_ref()], bump)]
    pub usdc_pool: UncheckedAccount<'info>,

    /// CHECK: PDA `[POOL_SEED, vault_state, usdt_mint]`, as above.
    #[account(mut, seeds = [POOL_SEED, vault_state.key().as_ref(), usdt_mint.key().as_ref()], bump)]
    pub usdt_pool: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// One-time setup: records the two accepted mints and creates one payout-pool
/// token account per mint, owned by the `VaultState` PDA.
///
/// Both mints must be classic-SPL (Token-2022 is rejected), initialised, and
/// 6-decimal. All checks run before anything is created.
pub fn init_vault(ctx: Context<InitVault>, usdc_mint: Pubkey, usdt_mint: Pubkey) -> Result<()> {
    require_keys_neq!(usdc_mint, usdt_mint, VaultError::DuplicateMint);
    require_keys_eq!(ctx.accounts.usdc_mint.key(), usdc_mint, VaultError::InvalidMint);
    require_keys_eq!(ctx.accounts.usdt_mint.key(), usdt_mint, VaultError::InvalidMint);
    validate_mint(&ctx.accounts.usdc_mint)?;
    validate_mint(&ctx.accounts.usdt_mint)?;

    let vault_key = ctx.accounts.vault_state.key();
    for (pool, mint, bump) in [
        (&ctx.accounts.usdc_pool, &ctx.accounts.usdc_mint, ctx.bumps.usdc_pool),
        (&ctx.accounts.usdt_pool, &ctx.accounts.usdt_mint, ctx.bumps.usdt_pool),
    ] {
        create_pool(
            &ctx.accounts.sl8_admin,
            pool,
            mint,
            &ctx.accounts.vault_state,
            &ctx.accounts.token_program,
            &ctx.accounts.system_program,
            &[POOL_SEED, vault_key.as_ref(), mint.key().as_ref(), &[bump]],
        )?;
    }

    let vs = &mut ctx.accounts.vault_state;
    vs.usdc_mint = usdc_mint;
    vs.usdt_mint = usdt_mint;
    vs.usdc_pool = ctx.accounts.usdc_pool.key();
    vs.usdt_pool = ctx.accounts.usdt_pool.key();
    vs.sl8_wallet = SL8_ADMIN_PUBKEY;
    vs.usdc_floor = 0;
    vs.usdt_floor = 0;
    vs.floor_updated_at = 0;
    vs.bump = ctx.bumps.vault_state;
    Ok(())
}

/// Classic-token-program-owned, initialised `Mint` with 6 decimals.
fn validate_mint(info: &AccountInfo) -> Result<()> {
    require_keys_eq!(*info.owner, token::ID, VaultError::WrongTokenProgram);
    let data = info.try_borrow_data()?;
    let mint = spl_token::state::Mint::unpack(&data).map_err(|_| error!(VaultError::InvalidMint))?;
    require!(mint.decimals == TOKEN_DECIMALS, VaultError::InvalidDecimals);
    Ok(())
}

/// Creates `pool` (a PDA of this program) as a token account for `mint` with
/// `authority` as its token owner.
///
/// Handles an already-funded address: anyone can send lamports to a
/// predictable PDA address, and `create_account` refuses an address that holds
/// lamports. Without the fallback below, one dust transfer to a pool address
/// would brick `init_vault` permanently (the `VaultState` PDA is fixed).
#[allow(clippy::too_many_arguments)]
fn create_pool<'info>(
    payer: &Signer<'info>,
    pool: &UncheckedAccount<'info>,
    mint: &UncheckedAccount<'info>,
    authority: &Account<'info, VaultState>,
    token_program: &Program<'info, Token>,
    system_program: &Program<'info, System>,
    pool_seeds: &[&[u8]],
) -> Result<()> {
    let space = spl_token::state::Account::LEN;
    let required = Rent::get()?.minimum_balance(space);
    let have = pool.lamports();
    let signer: &[&[&[u8]]] = &[pool_seeds];

    if have == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system_program.to_account_info(),
                system_program::CreateAccount { from: payer.to_account_info(), to: pool.to_account_info() },
                signer,
            ),
            required,
            space as u64,
            &token::ID,
        )?;
    } else {
        if required > have {
            system_program::transfer(
                CpiContext::new(
                    system_program.to_account_info(),
                    system_program::Transfer { from: payer.to_account_info(), to: pool.to_account_info() },
                ),
                required - have,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(
                system_program.to_account_info(),
                system_program::Allocate { account_to_allocate: pool.to_account_info() },
                signer,
            ),
            space as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(
                system_program.to_account_info(),
                system_program::Assign { account_to_assign: pool.to_account_info() },
                signer,
            ),
            &token::ID,
        )?;
    }

    token::initialize_account3(CpiContext::new(
        token_program.to_account_info(),
        InitializeAccount3 {
            account: pool.to_account_info(),
            mint: mint.to_account_info(),
            authority: authority.to_account_info(),
        },
    ))
}
