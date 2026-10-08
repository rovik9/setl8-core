//! Creating accounts at PDA addresses.

use anchor_lang::prelude::*;
use anchor_lang::system_program;

/// Creates `new_account` (a PDA of this program, derived from `seeds` which must
/// include the bump) with `space` bytes, rent-exempt, owned by `owner`.
///
/// Handles an already-funded address: anyone can send lamports to a predictable
/// PDA address, and `create_account` refuses an address that holds lamports.
/// Without the fallback, one dust transfer would permanently brick whatever
/// needs that address. The fallback tops the balance up to rent-exempt, then
/// `allocate`s and `assign`s (both signed with the PDA seeds).
pub fn create_pda_account<'info>(
    payer: &AccountInfo<'info>,
    new_account: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    space: usize,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> Result<()> {
    let required = Rent::get()?.minimum_balance(space);
    let have = new_account.lamports();
    let signer: &[&[&[u8]]] = &[seeds];

    if have == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system_program.clone(),
                system_program::CreateAccount { from: payer.clone(), to: new_account.clone() },
                signer,
            ),
            required,
            space as u64,
            owner,
        )
    } else {
        if required > have {
            system_program::transfer(
                CpiContext::new(
                    system_program.clone(),
                    system_program::Transfer { from: payer.clone(), to: new_account.clone() },
                ),
                required - have,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(
                system_program.clone(),
                system_program::Allocate { account_to_allocate: new_account.clone() },
                signer,
            ),
            space as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(
                system_program.clone(),
                system_program::Assign { account_to_assign: new_account.clone() },
                signer,
            ),
            owner,
        )
    }
}

/// `create_pda_account` followed by writing `value` (Anchor discriminator plus
/// Borsh data) into the new account.
pub fn create_pda_account_with<'info, T: AccountSerialize>(
    payer: &AccountInfo<'info>,
    new_account: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    space: usize,
    seeds: &[&[u8]],
    value: &T,
) -> Result<()> {
    create_pda_account(payer, new_account, system_program, space, &crate::ID, seeds)?;
    write_account(new_account, value)
}

/// Writes `value` (discriminator plus data) into an existing program-owned account.
pub fn write_account<T: AccountSerialize>(info: &AccountInfo, value: &T) -> Result<()> {
    let mut data = info.try_borrow_mut_data()?;
    let mut out: &mut [u8] = &mut data;
    value.try_serialize(&mut out)
}
