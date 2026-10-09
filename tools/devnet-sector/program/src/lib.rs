//! MOCK SECTOR PROGRAM: TEST SCAFFOLDING ONLY.
//!
//! A deliberately tiny and deliberately INSECURE stand-in for a real sector program (lev-trading, options, ...),
//! used only to rehearse the core-vault program on devnet. It has no authorisation at all: anyone can call
//! `SetTally`, and the CPI target of `DepositFee` / `RequestPayout` is whatever program account the caller passes
//! last. NEVER deploy it anywhere that holds real value, and never register it on a mainnet vault.
//!
//! What it does: it is the one thing the vault trusts to call its sector-only instructions. It signs for its
//! `sector_authority` PDA (`[b"setl8_sector_authority"]` under its own program id), forwards `deposit_fee` and
//! `request_payout` to the vault, and keeps the sector's payout tally (`[b"payout_tally"] PDA`, layout from
//! `setl8-shared-interfaces::PayoutTally`) in step with the vault's counters so `reconcile_product` can be rehearsed.
//! See `README.md` next to this crate for the exact data layouts and account lists.

use borsh::{BorshDeserialize, BorshSerialize};
use setl8_shared_interfaces as si;
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::AccountMeta,
    program::{get_return_data, invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use solana_system_interface::instruction as system_instruction;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// Instruction data: a Borsh enum (1 byte tag, then little-endian fields).
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub enum SectorIx {
    /// Tag 0. Creates the payout tally PDA with count 0 / total 0.
    InitTally,
    /// Tag 1. Overwrites the tally's count and total. NO authorisation (scaffolding): the "deliberately wrong
    /// tally" switch used to rehearse `reconcile_product` pausing a product.
    SetTally { count: u64, total: u64 },
    /// Tag 2. CPI vault `deposit_fee` (the trader pays the vault directly).
    DepositFee { amount: u64, challenge_id: u64, account_size: u64 },
    /// Tag 3. CPI vault `request_payout`; on a `Paid` outcome bumps the tally. The trader wallet is NOT an
    /// instruction field: it is read from the `trader_state` account (see `TRADER_WALLET_OFFSET`).
    RequestPayout { amount: u64, challenge_id: u64, proposed_request_id: u64 },
}

/// Number of accounts the vault's `deposit_fee` takes (as listed by the shared-interfaces builder).
pub const DEPOSIT_FEE_VAULT_ACCOUNTS: usize = 12;
/// Number of accounts the vault's `request_payout` takes (as listed by the shared-interfaces builder).
pub const REQUEST_PAYOUT_VAULT_ACCOUNTS: usize = 7;
/// Byte offset of `trader_wallet` inside a vault `TraderState` account: it is the first field, after the 8-byte
/// Anchor discriminator.
pub const TRADER_WALLET_OFFSET: usize = 8;

/// This program's own error codes (`ProgramError::Custom`), chosen far from the vault's 6000+ range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MockError {
    /// The account passed in the sector_authority slot is not this program's authority PDA.
    WrongSectorAuthority = 0x5300,
    /// The account passed as the tally is not this program's payout-tally PDA.
    WrongTallyAddress = 0x5301,
    /// The vault CPI succeeded but left no (or foreign, or unreadable) return data.
    MissingVaultReturnData = 0x5302,
    /// A tally addition would overflow u64.
    TallyOverflow = 0x5303,
}

impl From<MockError> for ProgramError {
    fn from(e: MockError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    match SectorIx::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)? {
        SectorIx::InitTally => init_tally(program_id, accounts),
        SectorIx::SetTally { count, total } => set_tally(program_id, accounts, count, total),
        SectorIx::DepositFee { amount, challenge_id, account_size } => {
            deposit_fee(program_id, accounts, amount, challenge_id, account_size)
        }
        SectorIx::RequestPayout { amount, challenge_id, proposed_request_id } => {
            request_payout(program_id, accounts, amount, challenge_id, proposed_request_id)
        }
    }
}

/// `[0] payer (signer, writable), [1] tally PDA (writable), [2] system program`.
fn init_tally(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let [payer, tally, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let (expected, bump) = si::derive_payout_tally(program_id);
    if *tally.key != expected {
        return Err(MockError::WrongTallyAddress.into());
    }
    if !tally.data_is_empty() {
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    let space = si::PAYOUT_TALLY_MIN_LEN;
    let rent_min = Rent::get()?.minimum_balance(space);
    let signer_seeds: &[&[u8]] = &[si::PAYOUT_TALLY_SEED, &[bump]];
    if tally.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(payer.key, tally.key, rent_min, space as u64, program_id),
            &[payer.clone(), tally.clone(), system.clone()],
            &[signer_seeds],
        )?;
    } else {
        // Pre-funded address (anyone can send lamports to a PDA): create_account would fail on it, so top up to
        // rent exemption, then allocate and assign.
        let shortfall = rent_min.saturating_sub(tally.lamports());
        if shortfall > 0 {
            invoke(
                &system_instruction::transfer(payer.key, tally.key, shortfall),
                &[payer.clone(), tally.clone(), system.clone()],
            )?;
        }
        invoke_signed(
            &system_instruction::allocate(tally.key, space as u64),
            &[tally.clone(), system.clone()],
            &[signer_seeds],
        )?;
        invoke_signed(
            &system_instruction::assign(tally.key, program_id),
            &[tally.clone(), system.clone()],
            &[signer_seeds],
        )?;
    }
    write_tally(tally, si::PayoutTally { requested_count: 0, requested_total: 0 })
}

/// `[0] tally PDA (writable)`. No authorisation, by design.
fn set_tally(program_id: &Pubkey, accounts: &[AccountInfo], count: u64, total: u64) -> ProgramResult {
    let [tally, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    check_tally(program_id, tally)?;
    write_tally(tally, si::PayoutTally { requested_count: count, requested_total: total })
}

/// The 12 vault accounts of `si::deposit_fee` in the builder's order (`sector_authority`, `product_registry`, then
/// the vault's remaining accounts), plus the vault PROGRAM account last (13 accounts).
fn deposit_fee(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
    challenge_id: u64,
    account_size: u64,
) -> ProgramResult {
    if accounts.len() != DEPOSIT_FEE_VAULT_ACCOUNTS + 1 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let (vault_program, vault_accounts) = accounts.split_last().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (sector_authority, registry, remaining) = (&vault_accounts[0], &vault_accounts[1], &vault_accounts[2..]);
    // remaining: 0 trader_state, 1 payer, 2 system, 3 vault_state, 4 trader, 5 trader_token_account, 6 mint,
    // 7 pool, 8 sl8_token_account, 9 token_program
    let trader = &remaining[4];

    let bump = check_authority(program_id, sector_authority)?;
    let ix = si::deposit_fee(
        *vault_program.key,
        *sector_authority.key,
        *registry.key,
        &metas_like_outer(remaining),
        si::DepositFeeArgs {
            amount,
            product_program_id: *program_id,
            challenge_id,
            trader_wallet: *trader.key,
            account_size,
        },
    );
    invoke_signed(&ix, accounts, &[&[si::SECTOR_AUTHORITY_SEED, &[bump]]])
}

/// The 7 vault accounts of `si::request_payout` in the builder's order, then `[7] tally PDA (writable)` and
/// `[8] vault PROGRAM account` (9 accounts).
fn request_payout(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
    challenge_id: u64,
    proposed_request_id: u64,
) -> ProgramResult {
    if accounts.len() != REQUEST_PAYOUT_VAULT_ACCOUNTS + 2 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let (vault_program, rest) = accounts.split_last().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (tally, vault_accounts) = rest.split_last().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (sector_authority, registry, remaining) = (&vault_accounts[0], &vault_accounts[1], &vault_accounts[2..]);
    let trader_state = &remaining[0];

    let bump = check_authority(program_id, sector_authority)?;
    check_tally(program_id, tally)?;
    if !tally.is_writable {
        return Err(ProgramError::InvalidArgument);
    }
    // The vault re-derives the trader_state PDA from this wallet and rejects a mismatch, so a lie here only fails.
    let trader_wallet = read_trader_wallet(trader_state)?;

    let ix = si::request_payout(
        *vault_program.key,
        *sector_authority.key,
        *registry.key,
        &metas_like_outer(remaining),
        si::RequestPayoutArgs {
            trader_wallet,
            amount,
            product_program_id: *program_id,
            challenge_id,
            proposed_request_id,
        },
    );
    invoke_signed(&ix, accounts, &[&[si::SECTOR_AUTHORITY_SEED, &[bump]]])?;

    // Only an accepted request (Paid) is counted. Abandoned created no claim, so the tally must not move.
    let outcome = match get_return_data() {
        Some((from, d)) if from == *vault_program.key && d.len() == 1 => si::PayoutOutcome::from_u8(d[0]),
        _ => None,
    };
    match outcome.ok_or(MockError::MissingVaultReturnData)? {
        si::PayoutOutcome::Abandoned => Ok(()),
        si::PayoutOutcome::Paid => {
            let current = read_tally(tally)?;
            write_tally(
                tally,
                si::PayoutTally {
                    requested_count: current.requested_count.checked_add(1).ok_or(MockError::TallyOverflow)?,
                    requested_total: current.requested_total.checked_add(amount).ok_or(MockError::TallyOverflow)?,
                },
            )
        }
    }
}

/// Checks the sector_authority slot and returns the PDA bump to sign with.
fn check_authority(program_id: &Pubkey, sector_authority: &AccountInfo) -> Result<u8, ProgramError> {
    let (expected, bump) = si::derive_sector_authority(program_id);
    if *sector_authority.key != expected {
        return Err(MockError::WrongSectorAuthority.into());
    }
    Ok(bump)
}

fn check_tally(program_id: &Pubkey, tally: &AccountInfo) -> ProgramResult {
    let (expected, _) = si::derive_payout_tally(program_id);
    if *tally.key != expected {
        return Err(MockError::WrongTallyAddress.into());
    }
    if tally.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

fn read_tally(tally: &AccountInfo) -> Result<si::PayoutTally, ProgramError> {
    si::PayoutTally::parse(&tally.try_borrow_data()?).map_err(|_| ProgramError::InvalidAccountData)
}

fn write_tally(tally: &AccountInfo, value: si::PayoutTally) -> ProgramResult {
    value.write_into(&mut tally.try_borrow_mut_data()?).map_err(|_| ProgramError::AccountDataTooSmall)
}

fn read_trader_wallet(trader_state: &AccountInfo) -> Result<Pubkey, ProgramError> {
    let data = trader_state.try_borrow_data()?;
    let bytes = data.get(TRADER_WALLET_OFFSET..TRADER_WALLET_OFFSET + 32).ok_or(ProgramError::InvalidAccountData)?;
    Pubkey::try_from(bytes).map_err(|_| ProgramError::InvalidAccountData)
}

/// The CPI metas for the vault's remaining accounts: same key and flags as the outer instruction. The vault checks
/// every one of them itself, so a wrong flag is rejected there.
fn metas_like_outer(accounts: &[AccountInfo]) -> Vec<AccountMeta> {
    accounts
        .iter()
        .map(|a| AccountMeta { pubkey: *a.key, is_signer: a.is_signer, is_writable: a.is_writable })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_layout_are_stable() {
        let enc = |ix: SectorIx| ix.try_to_vec().unwrap();
        assert_eq!(enc(SectorIx::InitTally), vec![0]);
        assert_eq!(
            enc(SectorIx::SetTally { count: 1, total: 2 }),
            [&[1u8][..], &1u64.to_le_bytes(), &2u64.to_le_bytes()].concat()
        );
        let df = enc(SectorIx::DepositFee { amount: 1, challenge_id: 2, account_size: 3 });
        assert_eq!((df[0], df.len()), (2, 25));
        let rp = enc(SectorIx::RequestPayout { amount: 1, challenge_id: 2, proposed_request_id: 3 });
        assert_eq!((rp[0], rp.len()), (3, 25));
    }
}
