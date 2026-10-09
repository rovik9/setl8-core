//! Fixed public addresses and formats the tool checks against.

use anchor_lang::prelude::{pubkey, Pubkey};

/// Genesis hashes of the two public clusters. They bind a transaction to a cluster.
pub const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
pub const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";

/// SPL Memo v2. The tool puts `setl8-admin/1 genesis=<hash>` in every transaction so the
/// cluster is inside the signed bytes.
pub const MEMO_PROGRAM_ID: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
pub const MEMO_PREFIX: &str = "setl8-admin/1 genesis=";

pub const COMPUTE_BUDGET_ID: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");
pub const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSVAR_RECENT_BLOCKHASHES: Pubkey = pubkey!("SysvarRecentB1ockHashes11111111111111111111");
pub const SPL_TOKEN_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Labels only (the vault's real mints are whatever `init_vault` was given).
pub const MAINNET_USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const MAINNET_USDT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");

/// Wire format version of the transaction file.
pub const TX_FILE_FORMAT: &str = "setl8-admin-tx/1";

/// Lamports a nonce account needs beyond the rent minimum is zero; this is its data length.
pub const NONCE_ACCOUNT_LEN: usize = 80;

pub fn ata_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[wallet.as_ref(), SPL_TOKEN_ID.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}

pub fn token_label(mint: &Pubkey) -> Option<&'static str> {
    if *mint == MAINNET_USDC {
        Some("USDC (mainnet mint)")
    } else if *mint == MAINNET_USDT {
        Some("USDT (mainnet mint)")
    } else {
        None
    }
}
