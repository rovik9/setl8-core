//! Where a settled claim is paid: the trader's ASSOCIATED token accounts.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, spl_token};

use crate::constants::ATA_PROGRAM_ID;

/// The associated token account of `wallet` for `mint` (classic Token program).
pub fn associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[wallet.as_ref(), token::ID.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}

/// Whether `info` (already known to sit at the right ATA address) can receive a
/// payment: owned by the classic Token program, an initialised token account for
/// `mint` owned by `wallet`, and not frozen. Anything else means "skip this
/// claim", never an error: a trader who has not created (or has closed, or
/// hijacked) an account must not be able to block anyone else's settlement.
pub fn destination_usable(info: &AccountInfo, wallet: &Pubkey, mint: &Pubkey) -> bool {
    if *info.owner != token::ID {
        return false;
    }
    let Ok(data) = info.try_borrow_data() else {
        return false;
    };
    match <spl_token::state::Account as anchor_lang::solana_program::program_pack::Pack>::unpack(&data) {
        Ok(a) => a.owner == *wallet && a.mint == *mint && a.state == spl_token::state::AccountState::Initialized,
        Err(_) => false,
    }
}
