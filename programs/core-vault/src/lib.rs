//! Setl8 core vault: the payout pools and the per-challenge bookkeeping that sector
//! programs (lev-trading, options, ...) talk to over CPI.
//!
//! # Layout
//!
//! ```text
//! src/
//! |- lib.rs            the program: one thin wrapper per instruction, grouped by caller
//! |- constants/        seeds, admin keys, limits, token constants
//! |- errors.rs         VaultError
//! |- state/            on-chain accounts: VaultState, ProductRegistry, TraderState,
//! |                    PayoutClaim, BondPosition, BondCapTracker
//! |- instructions/     one file per instruction (Accounts struct + handler)
//! |  |- admin/             2-of-2 admin multisig
//! |  |- sector/            registered sector programs, via CPI
//! |  `- permissionless/    anyone
//! `- utils/            shared helpers: CPI-auth check, payment splitting, settlement and
//!                      bond arithmetic, reconciliation, the marketing reserve, PDA creation
//! ```
//!
//! Instruction names are exact snake_case matches to what `setl8-shared-interfaces`
//! precomputed as `sha256("global:<name>")[..8]` (Anchor's default discriminator),
//! which keeps this program's wire format compatible with that crate's builders
//! without either repo hardcoding the other's discriminator bytes.

use anchor_lang::prelude::*;
use setl8_shared_interfaces::ChallengeSize;

pub mod constants;
pub mod errors;
pub mod instructions;
pub mod state;
pub mod utils;

use instructions::*;
use state::{BondTerm, PoolSide};

declare_id!("2Z6WNsj4hNhKhmK9Cj3sXV5San9VYhh8gwtyvBfpP6ft");

#[program]
pub mod core_vault {
    use super::*;

    // ---- Admin instructions (2-of-2 admin multisig: SL8 + Rov) ----

    /// One-time vault setup: records the USDC/USDT mints and creates the two
    /// payout-pool token accounts. See `instructions::admin::init_vault`.
    pub fn init_vault(ctx: Context<InitVault>, usdc_mint: Pubkey, usdt_mint: Pubkey) -> Result<()> {
        instructions::init_vault(ctx, usdc_mint, usdt_mint)
    }

    pub fn register_product(
        ctx: Context<RegisterProduct>,
        product_program_id: Pubkey,
        fee_split_bps: u16,
        challenge_sizes: Vec<ChallengeSize>,
        max_payout_count: u64,
        reset_price_bps: Vec<u16>,
    ) -> Result<()> {
        instructions::register_product(
            ctx,
            product_program_id,
            fee_split_bps,
            challenge_sizes,
            max_payout_count,
            reset_price_bps,
        )
    }

    pub fn update_product_config(
        ctx: Context<UpdateProductConfig>,
        product_program_id: Pubkey,
        challenge_sizes: Vec<ChallengeSize>,
        fee_split_bps: u16,
        max_payout_count: u64,
        reset_price_bps: Vec<u16>,
    ) -> Result<()> {
        instructions::update_product_config(
            ctx,
            product_program_id,
            challenge_sizes,
            fee_split_bps,
            max_payout_count,
            reset_price_bps,
        )
    }

    /// Manual planned pause, 2-of-2. See `instructions::admin::pause_product`.
    pub fn pause_product(ctx: Context<PauseProduct>, product_program_id: Pubkey) -> Result<()> {
        instructions::pause_product(ctx, product_program_id)
    }

    pub fn reactivate_product(ctx: Context<ReactivateProduct>, product_program_id: Pubkey) -> Result<()> {
        instructions::reactivate_product(ctx, product_program_id)
    }

    /// DOCUMENTED EXCEPTION to "no admin key on money": both admins together move up to
    /// 75% of one pool's live balance (25% reserve) to the SL8 wallet's token account,
    /// with no deduction for claims or bonds. See
    /// `instructions::admin::admin_withdraw_marketing_funds` and the README's security model.
    pub fn admin_withdraw_marketing_funds(
        ctx: Context<AdminWithdrawMarketingFunds>,
        pool: PoolSide,
        amount: u64,
    ) -> Result<()> {
        instructions::admin_withdraw_marketing_funds(ctx, pool, amount)
    }

    // ---- Sector-program instructions (CPI from a registered sector program, `sector_authority` PDA signer) ----

    /// Creates the `TraderState` for a new challenge and takes the payment
    /// (split between the payout pool and the SL8 wallet). See
    /// `instructions::sector::deposit_fee`.
    pub fn deposit_fee(
        ctx: Context<DepositFee>,
        amount: u64,
        product_program_id: Pubkey,
        challenge_id: u64,
        trader_wallet: Pubkey,
        account_size: u64,
    ) -> Result<()> {
        instructions::deposit_fee(ctx, amount, product_program_id, challenge_id, trader_wallet, account_size)
    }

    /// Phase-specific reset of a `Failed` record. See `instructions::sector::deposit_reset`.
    pub fn deposit_reset(
        ctx: Context<DepositReset>,
        amount: u64,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        prev_challenge_id: u64,
        new_challenge_id: u64,
        reset_phase: u8,
    ) -> Result<()> {
        instructions::deposit_reset(
            ctx,
            amount,
            trader_wallet,
            product_program_id,
            prev_challenge_id,
            new_challenge_id,
            reset_phase,
        )
    }

    /// Refreshes a challenge's inactivity clock. See `instructions::sector::record_activity`.
    pub fn record_activity(
        ctx: Context<RecordActivity>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::record_activity(ctx, trader_wallet, product_program_id, challenge_id)
    }

    /// Books one payout against the challenge cap and queues it as a
    /// `PayoutClaim`; no tokens move (a heartbeat cycle pays it later). See
    /// `instructions::sector::request_payout`.
    pub fn request_payout(
        ctx: Context<RequestPayout>,
        trader_wallet: Pubkey,
        amount: u64,
        product_program_id: Pubkey,
        challenge_id: u64,
        proposed_request_id: u64,
    ) -> Result<()> {
        instructions::request_payout(ctx, trader_wallet, amount, product_program_id, challenge_id, proposed_request_id)
    }

    /// Marks a challenge `Failed`. See `instructions::sector::flag_trader_failed`.
    pub fn flag_trader_failed(
        ctx: Context<FlagTraderFailed>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::flag_trader_failed(ctx, trader_wallet, product_program_id, challenge_id)
    }

    // ---- Permissionless instructions (anyone may call) ----

    /// Permissionless. See `instructions::permissionless::mark_abandoned`.
    pub fn mark_abandoned(
        ctx: Context<MarkAbandoned>,
        trader_wallet: Pubkey,
        product_program_id: Pubkey,
        challenge_id: u64,
    ) -> Result<()> {
        instructions::mark_abandoned(ctx, trader_wallet, product_program_id, challenge_id)
    }

    /// Permissionless. Opens a heartbeat cycle (at most one at a time, at least 5
    /// days between starts) and snapshots what is owed and what is available. See
    /// `instructions::permissionless::begin_heartbeat`.
    pub fn begin_heartbeat(ctx: Context<BeginHeartbeat>) -> Result<()> {
        instructions::begin_heartbeat(ctx)
    }

    /// Permissionless. Pays one batch of queued payout claims pro rata (remaining
    /// accounts: `[claim, trader_usdc_ata, trader_usdt_ata]` triples). See
    /// `instructions::permissionless::settle_claims`.
    pub fn settle_claims<'info>(ctx: Context<'_, '_, '_, 'info, SettleClaims<'info>>) -> Result<()> {
        instructions::settle_claims(ctx)
    }

    /// Permissionless. Ends the cycle once every eligible claim was processed and
    /// sets the 25% reserve floors. See `instructions::permissionless::finalize_heartbeat`.
    pub fn finalize_heartbeat(ctx: Context<FinalizeHeartbeat>) -> Result<()> {
        instructions::finalize_heartbeat(ctx)
    }

    /// Permissionless. Compares the sector's payout tally with the vault's books
    /// and pauses the product on any mismatch. See
    /// `instructions::permissionless::reconcile_product`.
    pub fn reconcile_product(ctx: Context<ReconcileProduct>, product_program_id: Pubkey) -> Result<()> {
        instructions::reconcile_product(ctx, product_program_id)
    }

    /// Opens a bond (6 or 9 months). The depositor pays principal plus a 0.2% fee;
    /// the principal is split 50/50 between the same-mint pool and the SL8 wallet.
    /// See `instructions::permissionless::deposit_bond`.
    pub fn deposit_bond(ctx: Context<DepositBond>, deposit_index: u64, principal: u64, term: BondTerm) -> Result<()> {
        instructions::deposit_bond(ctx, deposit_index, principal, term)
    }

    /// Withdraws a bond: closes the position and queues its worth (principal, plus
    /// interest at maturity, minus the 0.2% withdrawal fee) as a claim settled by the
    /// heartbeat. Depositor only. See `instructions::permissionless::request_bond_payout`.
    pub fn request_bond_payout(ctx: Context<RequestBondPayout>, deposit_index: u64) -> Result<()> {
        instructions::request_bond_payout(ctx, deposit_index)
    }
}
