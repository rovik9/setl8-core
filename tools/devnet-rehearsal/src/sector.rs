//! Client for the mock sector program (tools/devnet-sector). Layouts: see its README.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_program;
use anchor_spl::token::spl_token;

use crate::chain::{ata, Ctx};

fn authority(ctx: &Ctx) -> Pubkey {
    si::derive_sector_authority(&ctx.sector).0
}

pub fn tally(ctx: &Ctx) -> Pubkey {
    si::derive_payout_tally(&ctx.sector).0
}

fn data(tag: u8, fields: &[u64]) -> Vec<u8> {
    let mut d = vec![tag];
    for f in fields {
        d.extend_from_slice(&f.to_le_bytes());
    }
    d
}

pub fn init_tally(ctx: &Ctx, payer: &Pubkey) -> Instruction {
    Instruction {
        program_id: ctx.sector,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(tally(ctx), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: data(0, &[]),
    }
}

pub fn set_tally(ctx: &Ctx, count: u64, total: u64) -> Instruction {
    Instruction {
        program_id: ctx.sector,
        accounts: vec![AccountMeta::new(tally(ctx), false)],
        data: data(1, &[count, total]),
    }
}

pub fn trader_state(ctx: &Ctx, wallet: &Pubkey, challenge_id: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"trader_state", ctx.sector.as_ref(), wallet.as_ref(), &challenge_id.to_le_bytes()],
        &ctx.keys.program_id,
    )
    .0
}

pub fn claim_address(ctx: &Ctx, wallet: &Pubkey, challenge_id: u64, request_id: u64) -> Pubkey {
    let ts = trader_state(ctx, wallet, challenge_id);
    Pubkey::find_program_address(&[b"payout_claim", ts.as_ref(), &request_id.to_le_bytes()], &ctx.keys.program_id).0
}

/// DepositFee through the sector: the vault builder's accounts (the authority PDA is signed
/// by the sector, not the transaction), then the vault program account.
pub fn deposit_fee(
    ctx: &Ctx,
    trader: &Pubkey,
    payer: &Pubkey,
    challenge_id: u64,
    amount: u64,
    size: u64,
    mint: &Pubkey,
) -> Instruction {
    let vault = ctx.keys.vault();
    let remaining = [
        AccountMeta::new(trader_state(ctx, trader, challenge_id), false),
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new_readonly(vault, false),
        AccountMeta::new_readonly(*trader, true),
        AccountMeta::new(ata(trader, mint), false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(ctx.keys.pool(mint), false),
        AccountMeta::new(ctx.keys.sl8_ata(mint), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    let vix = si::deposit_fee(
        ctx.keys.program_id,
        authority(ctx),
        ctx.keys.registry(&ctx.sector),
        &remaining,
        si::DepositFeeArgs {
            amount,
            product_program_id: ctx.sector,
            challenge_id,
            trader_wallet: *trader,
            account_size: size,
        },
    );
    let mut accounts = vix.accounts;
    accounts[0].is_signer = false; // the sector program signs for its authority PDA
    accounts.push(AccountMeta::new_readonly(ctx.keys.program_id, false));
    Instruction { program_id: ctx.sector, accounts, data: data(2, &[amount, challenge_id, size]) }
}

/// RequestPayout through the sector: builder accounts, then the tally, then the vault program.
pub fn request_payout(
    ctx: &Ctx,
    trader: &Pubkey,
    payer: &Pubkey,
    challenge_id: u64,
    amount: u64,
    request_id: u64,
) -> Instruction {
    let claim = claim_address(ctx, trader, challenge_id, request_id);
    let vix = si::request_payout(
        ctx.keys.program_id,
        authority(ctx),
        ctx.keys.registry(&ctx.sector),
        &[
            AccountMeta::new(trader_state(ctx, trader, challenge_id), false),
            AccountMeta::new(ctx.keys.vault(), false),
            AccountMeta::new(claim, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        si::RequestPayoutArgs {
            trader_wallet: *trader,
            amount,
            product_program_id: ctx.sector,
            challenge_id,
            proposed_request_id: request_id,
        },
    );
    let mut accounts = vix.accounts;
    accounts[0].is_signer = false;
    accounts.push(AccountMeta::new(tally(ctx), false));
    accounts.push(AccountMeta::new_readonly(ctx.keys.program_id, false));
    Instruction { program_id: ctx.sector, accounts, data: data(3, &[amount, challenge_id, request_id]) }
}
