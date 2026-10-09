//! The four permissionless vault instructions, and nothing else. This module is the only place the
//! keeper builds a vault instruction; there is no deposit, no admin instruction and no transfer here.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token::ID as TOKEN_PROGRAM;
use setl8_admin::admin_ix::Keys;

use crate::model::VaultView;
use crate::plan::triple;

pub fn reconcile(keys: &Keys, caller: &Pubkey, product: &Pubkey) -> Instruction {
    Instruction {
        program_id: keys.program_id,
        accounts: core_vault::accounts::ReconcileProduct {
            caller: *caller,
            product_registry: keys.registry(product),
            payout_tally: si::derive_payout_tally(product).0,
        }
        .to_account_metas(None),
        data: core_vault::instruction::ReconcileProduct { product_program_id: *product }.data(),
    }
}

pub fn begin(keys: &Keys, caller: &Pubkey, v: &VaultView) -> Instruction {
    Instruction {
        program_id: keys.program_id,
        accounts: core_vault::accounts::BeginHeartbeat {
            caller: *caller,
            vault_state: keys.vault(),
            usdc_pool: v.usdc_pool,
            usdt_pool: v.usdt_pool,
        }
        .to_account_metas(None),
        data: core_vault::instruction::BeginHeartbeat {}.data(),
    }
}

pub fn finalize(keys: &Keys, caller: &Pubkey, v: &VaultView) -> Instruction {
    Instruction {
        program_id: keys.program_id,
        accounts: core_vault::accounts::FinalizeHeartbeat {
            caller: *caller,
            vault_state: keys.vault(),
            usdc_pool: v.usdc_pool,
            usdt_pool: v.usdt_pool,
        }
        .to_account_metas(None),
        data: core_vault::instruction::FinalizeHeartbeat {}.data(),
    }
}

/// `settle_claims` over the given triples `[claim, trader_usdc_ata, trader_usdt_ata]`.
pub fn settle(keys: &Keys, caller: &Pubkey, v: &VaultView, triples: &[[Pubkey; 3]]) -> Instruction {
    let mut accounts = core_vault::accounts::SettleClaims {
        caller: *caller,
        vault_state: keys.vault(),
        usdc_mint: v.usdc_mint,
        usdt_mint: v.usdt_mint,
        usdc_pool: v.usdc_pool,
        usdt_pool: v.usdt_pool,
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    for [claim, usdc, usdt] in triples {
        accounts.push(AccountMeta::new(*claim, false));
        accounts.push(AccountMeta::new(*usdc, false));
        accounts.push(AccountMeta::new(*usdt, false));
    }
    Instruction { program_id: keys.program_id, accounts, data: core_vault::instruction::SettleClaims {}.data() }
}

pub fn settle_claims_for(
    keys: &Keys,
    caller: &Pubkey,
    v: &VaultView,
    claims: &[crate::model::ClaimView],
) -> Instruction {
    let triples: Vec<[Pubkey; 3]> = claims.iter().map(|c| triple(c, &v.usdc_mint, &v.usdt_mint)).collect();
    settle(keys, caller, v, &triples)
}

/// ComputeBudget SetComputeUnitLimit / SetComputeUnitPrice, used only when a priority fee is configured.
pub fn compute_budget(limit: u32, micro_lamports_per_unit: u64) -> [Instruction; 2] {
    let id = setl8_admin::constants::COMPUTE_BUDGET_ID;
    let mut a = vec![2u8];
    a.extend_from_slice(&limit.to_le_bytes());
    let mut b = vec![3u8];
    b.extend_from_slice(&micro_lamports_per_unit.to_le_bytes());
    [
        Instruction { program_id: id, accounts: vec![], data: a },
        Instruction { program_id: id, accounts: vec![], data: b },
    ]
}

/// The only instruction names the keeper is allowed to send.
pub const ALLOWED: [&str; 4] = ["reconcile_product", "begin_heartbeat", "settle_claims", "finalize_heartbeat"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClaimView;
    use anchor_lang::Discriminator;

    fn v() -> VaultView {
        VaultView {
            usdc_mint: Pubkey::new_unique(),
            usdt_mint: Pubkey::new_unique(),
            usdc_pool: Pubkey::new_unique(),
            usdt_pool: Pubkey::new_unique(),
            open_claims_count: 0,
            open_claims_total: 0,
            cycle_id: 0,
            cycle_started_at: 0,
            cycle_active: false,
            cycle_owed_snapshot: 0,
            cycle_available_snapshot: 0,
            cycle_eligible_count: 0,
            cycle_processed_count: 0,
        }
    }

    #[test]
    fn discriminators_and_account_lists() {
        let k = Keys::compiled();
        let caller = Pubkey::new_unique();
        let vault = v();
        let product = Pubkey::new_unique();
        let r = reconcile(&k, &caller, &product);
        assert_eq!(&r.data[..8], core_vault::instruction::ReconcileProduct::DISCRIMINATOR);
        assert_eq!(r.accounts.len(), 3);
        assert!(r.accounts[0].is_signer && !r.accounts[1].is_signer);
        assert_eq!(r.accounts[1].pubkey, k.registry(&product));
        assert_eq!(r.accounts[2].pubkey, si::derive_payout_tally(&product).0);
        let b = begin(&k, &caller, &vault);
        assert_eq!(&b.data[..8], core_vault::instruction::BeginHeartbeat::DISCRIMINATOR);
        assert_eq!(b.accounts.len(), 4);
        let f = finalize(&k, &caller, &vault);
        assert_eq!(&f.data[..8], core_vault::instruction::FinalizeHeartbeat::DISCRIMINATOR);
        let c = ClaimView {
            address: Pubkey::new_unique(),
            trader_wallet: Pubkey::new_unique(),
            owed: 1,
            created_in_cycle: 1,
            last_settled_cycle: 0,
            kind: 0,
        };
        let s = settle_claims_for(&k, &caller, &vault, &[c.clone(), c.clone()]);
        assert_eq!(&s.data[..8], core_vault::instruction::SettleClaims::DISCRIMINATOR);
        assert_eq!(s.accounts.len(), 7 + 6);
        assert_eq!(s.accounts[7].pubkey, c.address);
        assert_eq!(s.accounts[8].pubkey, crate::plan::associated_token_address(&c.trader_wallet, &vault.usdc_mint));
        assert_eq!(s.accounts[9].pubkey, crate::plan::associated_token_address(&c.trader_wallet, &vault.usdt_mint));
        assert!(s.accounts[7..].iter().all(|m| m.is_writable && !m.is_signer));
    }

    #[test]
    fn compute_budget_encoding() {
        let [l, p] = compute_budget(400_000, 7);
        assert_eq!(l.data, [vec![2], 400_000u32.to_le_bytes().to_vec()].concat());
        assert_eq!(p.data, [vec![3], 7u64.to_le_bytes().to_vec()].concat());
    }
}
