//! The six admin instructions: build them, and decode them back from raw bytes.
//!
//! Builders reuse `setl8-shared-interfaces` v0.4.1 where one exists and the
//! `core_vault` crate's generated client types otherwise (`init_vault`). Decoding goes the
//! other way through `core_vault`'s own instruction structs, so the tool and the program
//! cannot disagree about a byte.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas};
use core_vault::constants::{
    MAX_CHALLENGE_SIZES, MAX_RESET_PHASES, POOL_SEED, PRODUCT_REGISTRY_SEED, ROV_ADMIN_PUBKEY, SL8_ADMIN_PUBKEY,
    VAULT_STATE_SEED,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::constants::{ata_address, token_label, SPL_TOKEN_ID, SYSTEM_PROGRAM_ID};
use crate::error::{Error, Result};
use crate::fmt::{fmt_amount, group};

/// The program id and the two admin keys every derivation depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    pub program_id: Pubkey,
    pub sl8: Pubkey,
    pub rov: Pubkey,
}

impl Keys {
    /// The program id and admin pubkeys compiled into `core_vault` for this build
    /// (REAL keys by default, the public test keys with `--features localnet`).
    pub fn compiled() -> Keys {
        Keys { program_id: core_vault::ID, sl8: SL8_ADMIN_PUBKEY, rov: ROV_ADMIN_PUBKEY }
    }

    pub fn with_program(mut self, program_id: Pubkey) -> Keys {
        self.program_id = program_id;
        self
    }

    pub fn vault(&self) -> Pubkey {
        Pubkey::find_program_address(&[VAULT_STATE_SEED, self.sl8.as_ref(), self.rov.as_ref()], &self.program_id).0
    }

    pub fn pool(&self, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[POOL_SEED, self.vault().as_ref(), mint.as_ref()], &self.program_id).0
    }

    pub fn registry(&self, product: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[PRODUCT_REGISTRY_SEED, product.as_ref()], &self.program_id).0
    }

    /// The only destination the tool accepts for a withdrawal: SL8's associated token account.
    pub fn sl8_ata(&self, mint: &Pubkey) -> Pubkey {
        ata_address(&self.sl8, mint)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Usdc,
    Usdt,
}

impl Side {
    pub fn parse(s: &str) -> Result<Side> {
        match s.to_ascii_lowercase().as_str() {
            "usdc" => Ok(Side::Usdc),
            "usdt" => Ok(Side::Usdt),
            _ => Err(Error(format!("pool must be usdc or usdt, not '{s}'"))),
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Side::Usdc => "USDC",
            Side::Usdt => "USDT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub size: u64,
    pub cost: u64,
}

/// `product.json` for `register_product` and `update_product_config`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductConfig {
    pub product_program_id: String,
    pub fee_split_bps: u16,
    pub challenge_sizes: Vec<Tier>,
    pub max_payout_count: u64,
    pub reset_price_bps: Vec<u16>,
}

/// A validated product configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Product {
    pub product_program_id: Pubkey,
    pub fee_split_bps: u16,
    pub challenge_sizes: Vec<Tier>,
    pub max_payout_count: u64,
    pub reset_price_bps: Vec<u16>,
}

impl ProductConfig {
    pub fn parse(json: &str) -> Result<ProductConfig> {
        serde_json::from_str(json).map_err(|e| Error(format!("product config is not valid: {e}")))
    }

    /// Validates with the program's own limits. Returns the config and non-fatal warnings
    /// (the program accepts these; they are usually a typo).
    pub fn validate(&self) -> Result<(Product, Vec<String>)> {
        let id: Pubkey = self
            .product_program_id
            .parse()
            .map_err(|_| Error(format!("product_program_id '{}' is not a valid pubkey", self.product_program_id)))?;
        if self.challenge_sizes.len() > MAX_CHALLENGE_SIZES {
            return Err(Error(format!(
                "{} challenge sizes; the program allows at most {MAX_CHALLENGE_SIZES}",
                self.challenge_sizes.len()
            )));
        }
        if self.reset_price_bps.len() > MAX_RESET_PHASES {
            return Err(Error(format!(
                "{} reset phases; the program allows at most {MAX_RESET_PHASES}",
                self.reset_price_bps.len()
            )));
        }
        if self.fee_split_bps > 10_000 {
            return Err(Error(format!("fee_split_bps {} is above 10,000", self.fee_split_bps)));
        }
        let mut warnings = vec![];
        if self.challenge_sizes.iter().any(|t| t.cost == 0) {
            warnings.push("a tier has cost 0 (a free challenge); the program accepts this".to_string());
        }
        if self.challenge_sizes.iter().any(|t| t.size == 0) {
            warnings.push("a tier has size 0; the program accepts this".to_string());
        }
        if self.reset_price_bps.iter().any(|b| *b > 10_000) {
            warnings.push("a reset price is above 100% of the account size; the program accepts this".to_string());
        }
        if self.max_payout_count == 0 {
            warnings.push("max_payout_count is 0: no trader of this product can ever request a payout".to_string());
        }
        if self.challenge_sizes.is_empty() {
            warnings.push("no challenge sizes: nobody can buy a challenge".to_string());
        }
        Ok((
            Product {
                product_program_id: id,
                fee_split_bps: self.fee_split_bps,
                challenge_sizes: self.challenge_sizes.clone(),
                max_payout_count: self.max_payout_count,
                reset_price_bps: self.reset_price_bps.clone(),
            },
            warnings,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminIx {
    InitVault {
        usdc_mint: Pubkey,
        usdt_mint: Pubkey,
    },
    RegisterProduct(Product),
    UpdateProductConfig(Product),
    PauseProduct {
        product: Pubkey,
    },
    ReactivateProduct {
        product: Pubkey,
    },
    /// `mint` is the mint account passed to the program (the program checks it against the
    /// vault's own record).
    Withdraw {
        pool: Side,
        amount: u64,
        mint: Pubkey,
    },
}

fn si_tiers(p: &Product) -> Vec<si::ChallengeSize> {
    p.challenge_sizes.iter().map(|t| si::ChallengeSize { size: t.size, cost: t.cost }).collect()
}

impl AdminIx {
    pub fn name(&self) -> &'static str {
        match self {
            AdminIx::InitVault { .. } => "init_vault",
            AdminIx::RegisterProduct(_) => "register_product",
            AdminIx::UpdateProductConfig(_) => "update_product_config",
            AdminIx::PauseProduct { .. } => "pause_product",
            AdminIx::ReactivateProduct { .. } => "reactivate_product",
            AdminIx::Withdraw { .. } => "admin_withdraw_marketing_funds",
        }
    }

    /// The exact instruction the program expects for these contents.
    pub fn build(&self, k: &Keys) -> Instruction {
        match self {
            AdminIx::InitVault { usdc_mint, usdt_mint } => {
                let accounts = core_vault::accounts::InitVault {
                    sl8_admin: k.sl8,
                    rov_admin: k.rov,
                    vault_state: k.vault(),
                    usdc_mint: *usdc_mint,
                    usdt_mint: *usdt_mint,
                    usdc_pool: k.pool(usdc_mint),
                    usdt_pool: k.pool(usdt_mint),
                    token_program: SPL_TOKEN_ID,
                    system_program: SYSTEM_PROGRAM_ID,
                };
                Instruction {
                    program_id: k.program_id,
                    accounts: accounts.to_account_metas(None),
                    data: core_vault::instruction::InitVault { usdc_mint: *usdc_mint, usdt_mint: *usdt_mint }.data(),
                }
            }
            AdminIx::RegisterProduct(p) => si::register_product(
                k.program_id,
                k.sl8,
                k.rov,
                k.registry(&p.product_program_id),
                &[AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false)],
                si::RegisterProductArgs {
                    product_program_id: p.product_program_id,
                    fee_split_bps: p.fee_split_bps,
                    challenge_sizes: si_tiers(p),
                    max_payout_count: p.max_payout_count,
                    reset_price_bps: p.reset_price_bps.clone(),
                },
            ),
            AdminIx::UpdateProductConfig(p) => si::update_product_config(
                k.program_id,
                k.sl8,
                k.rov,
                k.registry(&p.product_program_id),
                &[],
                si::UpdateProductConfigArgs {
                    product_program_id: p.product_program_id,
                    challenge_sizes: si_tiers(p),
                    fee_split_bps: p.fee_split_bps,
                    max_payout_count: p.max_payout_count,
                    reset_price_bps: p.reset_price_bps.clone(),
                },
            ),
            AdminIx::PauseProduct { product } => si::pause_product(
                k.program_id,
                k.sl8,
                k.rov,
                k.registry(product),
                &[],
                si::PauseProductArgs { product_program_id: *product },
            ),
            AdminIx::ReactivateProduct { product } => si::reactivate_product(
                k.program_id,
                k.sl8,
                k.rov,
                k.registry(product),
                &[],
                si::ReactivateProductArgs { product_program_id: *product },
            ),
            AdminIx::Withdraw { pool, amount, mint } => si::admin_withdraw_marketing_funds(
                k.program_id,
                k.sl8,
                k.rov,
                k.vault(),
                *mint,
                k.pool(mint),
                k.sl8_ata(mint),
                SPL_TOKEN_ID,
                si::AdminWithdrawMarketingFundsArgs {
                    pool: match pool {
                        Side::Usdc => si::PoolSide::Usdc,
                        Side::Usdt => si::PoolSide::Usdt,
                    },
                    amount: *amount,
                },
            ),
        }
    }

    /// Names of the instruction's accounts, in the program's declared order.
    pub fn account_roles(&self) -> &'static [&'static str] {
        match self {
            AdminIx::InitVault { .. } => &[
                "sl8_admin",
                "rov_admin",
                "vault_state",
                "usdc_mint",
                "usdt_mint",
                "usdc_pool",
                "usdt_pool",
                "token_program",
                "system_program",
            ],
            AdminIx::RegisterProduct(_) => &["sl8_admin", "rov_admin", "product_registry", "system_program"],
            AdminIx::UpdateProductConfig(_) | AdminIx::PauseProduct { .. } | AdminIx::ReactivateProduct { .. } => {
                &["sl8_admin", "rov_admin", "product_registry"]
            }
            AdminIx::Withdraw { .. } => &[
                "sl8_admin",
                "rov_admin",
                "vault_state",
                "mint",
                "pool_token_account",
                "sl8_token_account",
                "token_program",
            ],
        }
    }

    /// For each account position: how it can be checked without a network.
    pub fn account_notes(&self) -> &'static [&'static str] {
        match self {
            AdminIx::InitVault { .. } => &[
                "SL8 admin key (compiled in)",
                "ROV admin key (compiled in)",
                "vault PDA re-derived from both admin keys",
                "taken from the instruction argument",
                "taken from the instruction argument",
                "pool PDA re-derived from vault + mint",
                "pool PDA re-derived from vault + mint",
                "SPL Token program",
                "System program",
            ],
            AdminIx::RegisterProduct(_) => &[
                "SL8 admin key (compiled in)",
                "ROV admin key (compiled in)",
                "registry PDA re-derived from the product id",
                "System program",
            ],
            AdminIx::UpdateProductConfig(_) | AdminIx::PauseProduct { .. } | AdminIx::ReactivateProduct { .. } => &[
                "SL8 admin key (compiled in)",
                "ROV admin key (compiled in)",
                "registry PDA re-derived from the product id",
            ],
            AdminIx::Withdraw { .. } => &[
                "SL8 admin key (compiled in)",
                "ROV admin key (compiled in)",
                "vault PDA re-derived from both admin keys",
                "taken from the message (checked against the vault's record only with --rpc)",
                "pool PDA re-derived from vault + mint",
                "SL8 admin's associated token account for this mint, re-derived",
                "SPL Token program",
            ],
        }
    }

    /// Plain-language lines for the human (used by `plan` and `inspect`).
    pub fn describe(&self) -> Vec<String> {
        match self {
            AdminIx::InitVault { usdc_mint, usdt_mint } => vec![
                "Create the vault (ONE TIME) and its two payout pools.".to_string(),
                format!("USDC mint: {usdc_mint}{}", label_suffix(usdc_mint)),
                format!("USDT mint: {usdt_mint}{}", label_suffix(usdt_mint)),
                "The mints can never be changed afterwards.".to_string(),
            ],
            AdminIx::RegisterProduct(p) => {
                let mut v = vec![format!(
                    "Register sector program {} (its registry cannot be closed or corrected).",
                    p.product_program_id
                )];
                v.extend(product_lines(p));
                v
            }
            AdminIx::UpdateProductConfig(p) => {
                let mut v = vec![format!("Replace the configuration of sector program {}.", p.product_program_id)];
                v.extend(product_lines(p));
                v
            }
            AdminIx::PauseProduct { product } => {
                vec![format!("Pause sector program {product} (reason: planned upgrade).")]
            }
            AdminIx::ReactivateProduct { product } => vec![format!("Reactivate sector program {product}.")],
            AdminIx::Withdraw { pool, amount, mint } => vec![
                format!("withdraw {} {} from the {} pool", fmt_amount(*amount), pool.name(), pool.name()),
                format!("mint passed: {mint}{}", label_suffix(mint)),
                "MONEY LEAVES THE PAYOUT POOL to SL8's own token account (the documented admin-withdrawal exception)."
                    .to_string(),
            ],
        }
    }

    /// Decode what a vault-program instruction says, from raw bytes only. `accounts` are
    /// the instruction's account keys in order.
    pub fn decode(data: &[u8], accounts: &[Pubkey]) -> std::result::Result<AdminIx, String> {
        let disc: [u8; 8] = data
            .get(..8)
            .ok_or_else(|| "instruction data is shorter than the 8-byte discriminator".to_string())?
            .try_into()
            .unwrap();
        let body = &data[8..];
        fn parse<T: AnchorDeserialize>(body: &[u8]) -> std::result::Result<T, String> {
            let mut s = body;
            let v = T::deserialize(&mut s).map_err(|_| "arguments do not decode".to_string())?;
            if !s.is_empty() {
                return Err(format!("{} trailing byte(s) after the arguments", s.len()));
            }
            Ok(v)
        }
        let acc = |i: usize| accounts.get(i).copied().ok_or_else(|| format!("account #{i} is missing"));
        if disc == core_vault::instruction::InitVault::DISCRIMINATOR {
            let a: core_vault::instruction::InitVault = parse(body)?;
            Ok(AdminIx::InitVault { usdc_mint: a.usdc_mint, usdt_mint: a.usdt_mint })
        } else if disc == core_vault::instruction::RegisterProduct::DISCRIMINATOR {
            let a: core_vault::instruction::RegisterProduct = parse(body)?;
            Ok(AdminIx::RegisterProduct(Product {
                product_program_id: a.product_program_id,
                fee_split_bps: a.fee_split_bps,
                challenge_sizes: a.challenge_sizes.iter().map(|c| Tier { size: c.size, cost: c.cost }).collect(),
                max_payout_count: a.max_payout_count,
                reset_price_bps: a.reset_price_bps,
            }))
        } else if disc == core_vault::instruction::UpdateProductConfig::DISCRIMINATOR {
            let a: core_vault::instruction::UpdateProductConfig = parse(body)?;
            Ok(AdminIx::UpdateProductConfig(Product {
                product_program_id: a.product_program_id,
                fee_split_bps: a.fee_split_bps,
                challenge_sizes: a.challenge_sizes.iter().map(|c| Tier { size: c.size, cost: c.cost }).collect(),
                max_payout_count: a.max_payout_count,
                reset_price_bps: a.reset_price_bps,
            }))
        } else if disc == core_vault::instruction::PauseProduct::DISCRIMINATOR {
            let a: core_vault::instruction::PauseProduct = parse(body)?;
            Ok(AdminIx::PauseProduct { product: a.product_program_id })
        } else if disc == core_vault::instruction::ReactivateProduct::DISCRIMINATOR {
            let a: core_vault::instruction::ReactivateProduct = parse(body)?;
            Ok(AdminIx::ReactivateProduct { product: a.product_program_id })
        } else if disc == core_vault::instruction::AdminWithdrawMarketingFunds::DISCRIMINATOR {
            let a: core_vault::instruction::AdminWithdrawMarketingFunds = parse(body)?;
            let pool = match a.pool {
                core_vault::state::PoolSide::Usdc => Side::Usdc,
                core_vault::state::PoolSide::Usdt => Side::Usdt,
            };
            Ok(AdminIx::Withdraw { pool, amount: a.amount, mint: acc(3)? })
        } else {
            Err(match other_vault_instruction(&disc) {
                Some(n) => {
                    format!("this is the vault's `{n}` instruction, which is NOT one of the six admin instructions")
                }
                None => format!("unknown vault instruction (discriminator {})", crate::fmt::hex(&disc)),
            })
        }
    }
}

fn label_suffix(mint: &Pubkey) -> String {
    token_label(mint).map(|l| format!("  [{l}]")).unwrap_or_default()
}

fn product_lines(p: &Product) -> Vec<String> {
    let mut v = vec![format!(
        "fee split {} bps ({}.{:02}% of each fee goes to the payout pool), max payouts per challenge {}",
        p.fee_split_bps,
        p.fee_split_bps / 100,
        p.fee_split_bps % 100,
        p.max_payout_count
    )];
    for (i, t) in p.challenge_sizes.iter().enumerate() {
        v.push(format!(
            "tier {i}: size {} / cost {}  ({} / {} as 6-decimal dollars)",
            group(t.size),
            group(t.cost),
            fmt_amount(t.size),
            fmt_amount(t.cost)
        ));
    }
    if p.challenge_sizes.is_empty() {
        v.push("no tiers".to_string());
    }
    v.push(format!(
        "reset prices (bps of account size, by phase): {}",
        if p.reset_price_bps.is_empty() {
            "none".to_string()
        } else {
            p.reset_price_bps.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")
        }
    ));
    v
}

/// The vault's other twelve instructions, so a misuse is named rather than "unknown".
fn other_vault_instruction(disc: &[u8; 8]) -> Option<&'static str> {
    const NAMES: [&str; 12] = [
        "deposit_fee",
        "deposit_reset",
        "record_activity",
        "request_payout",
        "flag_trader_failed",
        "mark_abandoned",
        "begin_heartbeat",
        "settle_claims",
        "finalize_heartbeat",
        "reconcile_product",
        "deposit_bond",
        "request_bond_payout",
    ];
    NAMES.into_iter().find(|n| {
        let h = Sha256::digest(format!("global:{n}").as_bytes());
        h[..8] == disc[..]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default build must embed the REAL admin keys and the `localnet` build the public
    /// test keys: the tool signs for exactly the keys compiled into the program.
    #[test]
    fn the_compiled_in_admin_keys_are_the_expected_ones() {
        let k = Keys::compiled();
        #[cfg(not(feature = "localnet"))]
        {
            assert_eq!(k.sl8.to_string(), "SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ");
            assert_eq!(k.rov.to_string(), "RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ");
        }
        #[cfg(feature = "localnet")]
        {
            assert_eq!(k.sl8.to_string(), "9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");
            assert_eq!(k.rov.to_string(), "D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");
        }
        assert_eq!(k.program_id, core_vault::ID);
        assert_ne!(k.vault(), k.sl8);
    }

    #[test]
    fn every_builder_decodes_back_to_itself() {
        let k = Keys::compiled();
        let mint = Pubkey::new_unique();
        let prod = Product {
            product_program_id: Pubkey::new_unique(),
            fee_split_bps: 6500,
            challenge_sizes: vec![Tier { size: 10_000_000_000, cost: 100_000_000 }, Tier { size: 5, cost: 6 }],
            max_payout_count: 3,
            reset_price_bps: vec![100, 150],
        };
        let all = vec![
            AdminIx::InitVault { usdc_mint: Pubkey::new_unique(), usdt_mint: Pubkey::new_unique() },
            AdminIx::RegisterProduct(prod.clone()),
            AdminIx::UpdateProductConfig(prod.clone()),
            AdminIx::PauseProduct { product: prod.product_program_id },
            AdminIx::ReactivateProduct { product: prod.product_program_id },
            AdminIx::Withdraw { pool: Side::Usdt, amount: 1_250_000_000, mint },
        ];
        for a in all {
            let ix = a.build(&k);
            let keys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
            assert_eq!(AdminIx::decode(&ix.data, &keys).unwrap(), a, "{}", a.name());
            assert!(!a.describe().is_empty());
        }
    }

    #[test]
    fn other_vault_instructions_are_named() {
        let h = Sha256::digest(b"global:deposit_fee");
        let mut data = h[..8].to_vec();
        data.extend_from_slice(&[0; 4]);
        let e = AdminIx::decode(&data, &[]).unwrap_err();
        assert!(e.contains("deposit_fee") && e.contains("NOT one of the six"), "{e}");
        let e = AdminIx::decode(&[1, 2, 3, 4, 5, 6, 7, 8], &[]).unwrap_err();
        assert!(e.contains("unknown vault instruction"), "{e}");
        assert!(AdminIx::decode(&[1, 2, 3], &[]).is_err());
    }

    #[test]
    fn trailing_bytes_after_the_arguments_are_refused() {
        let k = Keys::compiled();
        let ix = AdminIx::PauseProduct { product: Pubkey::new_unique() }.build(&k);
        let mut data = ix.data.clone();
        data.push(0);
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(AdminIx::decode(&data, &keys).unwrap_err().contains("trailing"));
    }

    #[test]
    fn product_config_limits_are_the_programs_own() {
        let base = |n_tiers: usize, n_phases: usize, fee: u16| ProductConfig {
            product_program_id: Pubkey::new_unique().to_string(),
            fee_split_bps: fee,
            challenge_sizes: vec![Tier { size: 1, cost: 1 }; n_tiers],
            max_payout_count: 1,
            reset_price_bps: vec![100; n_phases],
        };
        assert!(base(32, 8, 10_000).validate().is_ok());
        assert!(base(33, 0, 0).validate().is_err());
        assert!(base(1, 9, 0).validate().is_err());
        assert!(base(1, 1, 10_001).validate().is_err());
        let mut bad = base(1, 1, 1);
        bad.product_program_id = "nope".into();
        assert!(bad.validate().is_err());
        let (_, w) = ProductConfig { max_payout_count: 0, ..base(1, 1, 1) }.validate().unwrap();
        assert!(w.iter().any(|x| x.contains("max_payout_count")));
        assert!(ProductConfig::parse(r#"{"product_program_id":"x","fee_split_bps":1,"challenge_sizes":[],"max_payout_count":1,"reset_price_bps":[],"extra":1}"#).is_err());
    }
}
