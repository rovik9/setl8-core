//! `plan`: build the UNSIGNED transaction and write the file.

use std::path::Path;

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountDeserialize;
use anchor_spl::token::spl_token::{
    solana_program::program_pack::Pack,
    state::{Account as TokAcc, AccountState},
};
use solana_hash::Hash;

use crate::admin_ix::{AdminIx, Keys, Product, Side};
use crate::cluster::Cluster;
use crate::constants::SYSTEM_PROGRAM_ID;
use crate::error::{Error, Result};
use crate::fmt::fmt_amount;
use crate::host::Host;
use crate::inspect::{description_text, inspect};
use crate::message::{build_message, Expect, NonceUse, Parts};
use crate::rpc::Rpc;
use crate::txfile::TxFile;

/// What the user asked for, before any account is resolved.
#[derive(Clone, Debug)]
pub enum AdminSpec {
    InitVault { usdc_mint: Pubkey, usdt_mint: Pubkey },
    Register(Product),
    Update(Product),
    Pause(Pubkey),
    Reactivate(Pubkey),
    Withdraw { pool: Side, amount: u64, mint: Option<Pubkey> },
}

pub enum Lifetime {
    /// A durable nonce account; `blockhash` given means "use this, do not read the chain".
    Nonce { account: Pubkey, authority: Pubkey, blockhash: Option<Hash> },
    /// A recent blockhash (same-session use). `None` = fetch one.
    Recent(Option<Hash>),
}

pub struct PlanRequest {
    pub keys: Keys,
    pub cluster: Cluster,
    pub rpc_url: String,
    pub offline: bool,
    pub genesis: Option<String>,
    pub spec: AdminSpec,
    pub fee_payer: Pubkey,
    pub lifetime: Lifetime,
    pub cu_limit: Option<u32>,
    pub cu_price: Option<u64>,
}

/// Parses the durable-nonce account: `Versions` (u32) + `State` (u32) + authority + nonce + fee.
pub fn parse_nonce_account(data: &[u8]) -> Result<(Pubkey, Hash)> {
    if data.len() != crate::constants::NONCE_ACCOUNT_LEN {
        return Err(Error(format!(
            "the nonce account has {} bytes, a nonce account has {}",
            data.len(),
            crate::constants::NONCE_ACCOUNT_LEN
        )));
    }
    let versions = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let state = u32::from_le_bytes(data[4..8].try_into().unwrap());
    if versions > 1 || state != 1 {
        return Err(Error("the nonce account is not initialised".into()));
    }
    let authority = Pubkey::try_from(&data[8..40]).unwrap();
    let hash = Hash::new_from_array(data[40..72].try_into().unwrap());
    Ok((authority, hash))
}

fn read_vault(rpc: &dyn Rpc, keys: &Keys) -> Result<core_vault::state::VaultState> {
    let acc = rpc.account(&keys.vault())?.ok_or_else(|| {
        Error(format!("the vault {} does not exist on this cluster (has init_vault run?)", keys.vault()))
    })?;
    if acc.owner != keys.program_id {
        return Err(Error(format!(
            "the vault account is owned by {}, not by the vault program {}",
            acc.owner, keys.program_id
        )));
    }
    let mut d = acc.data.as_slice();
    core_vault::state::VaultState::try_deserialize(&mut d)
        .map_err(|_| Error("the vault account does not decode as VaultState".into()))
}

/// The live-state pre-flight for a withdrawal. Returns the mint to use and notes for the user.
fn withdraw_preflight(
    rpc: &dyn Rpc,
    keys: &Keys,
    pool: Side,
    amount: u64,
    mint_arg: Option<Pubkey>,
) -> Result<(Pubkey, Vec<String>)> {
    let vs = read_vault(rpc, keys)?;
    let mint = match pool {
        Side::Usdc => vs.usdc_mint,
        Side::Usdt => vs.usdt_mint,
    };
    if let Some(m) = mint_arg {
        if m != mint {
            return Err(Error(format!("--mint {m} is not the vault's {} mint ({mint})", pool.name())));
        }
    }
    if vs.sl8_wallet != keys.sl8 {
        return Err(Error(format!(
            "the vault's SL8 wallet is {}, not this build's SL8 admin key {}",
            vs.sl8_wallet, keys.sl8
        )));
    }
    let pool_addr = keys.pool(&mint);
    let pool_acc = rpc
        .account(&pool_addr)?
        .ok_or_else(|| Error(format!("the {} pool {pool_addr} does not exist", pool.name())))?;
    let t = TokAcc::unpack(&pool_acc.data).map_err(|_| Error("the pool account is not a token account".into()))?;
    if t.state == AccountState::Frozen {
        return Err(Error(format!(
            "refused: the {} pool is FROZEN by the issuer; a withdrawal would fail inside the token program",
            pool.name()
        )));
    }
    let floor = match pool {
        Side::Usdc => vs.usdc_floor,
        Side::Usdt => vs.usdt_floor,
    };
    let reserve = core_vault::utils::reserve(t.amount, floor).map_err(|_| Error("reserve arithmetic failed".into()))?;
    let max =
        core_vault::utils::withdrawable(t.amount, floor).map_err(|_| Error("reserve arithmetic failed".into()))?;
    if amount == 0 {
        return Err(Error("refused: the amount is zero".into()));
    }
    if amount > max {
        return Err(Error(format!(
            "refused: the {} pool holds {} and must keep a reserve of {} (25% of the live balance, or the stored floor if higher); at most {} can be withdrawn now, you asked for {}",
            pool.name(),
            fmt_amount(t.amount),
            fmt_amount(reserve),
            fmt_amount(max),
            fmt_amount(amount)
        )));
    }
    let dest = keys.sl8_ata(&mint);
    let dest_acc = rpc.account(&dest)?.ok_or_else(|| {
        Error(format!("refused: SL8's token account {dest} for this mint does not exist yet; create the SL8 admin key's associated token account first (DEPLOY-CHECKLIST step 5.3)"))
    })?;
    let d = TokAcc::unpack(&dest_acc.data).map_err(|_| Error("the SL8 destination is not a token account".into()))?;
    if d.mint != mint || d.owner != keys.sl8 {
        return Err(Error(format!(
            "refused: {dest} is not a {} token account owned by the SL8 admin key",
            pool.name()
        )));
    }
    if d.state == AccountState::Frozen {
        return Err(Error(format!("refused: SL8's token account {dest} is frozen")));
    }
    Ok((
        mint,
        vec![
            format!(
                "pre-flight: the {} pool holds {}, reserve {}, at most {} can leave now; you are taking {}",
                pool.name(),
                fmt_amount(t.amount),
                fmt_amount(reserve),
                fmt_amount(max),
                fmt_amount(amount)
            ),
            format!("pre-flight: destination {dest} (SL8's associated token account) exists and is not frozen"),
        ],
    ))
}

pub fn plan(host: &mut dyn Host, req: &PlanRequest, out: &Path) -> Result<TxFile> {
    let keys = req.keys;
    let mut notes: Vec<String> = vec![];

    // ---- genesis (the cluster) ----
    let needs_rpc = !req.offline;
    let rpc: Option<Box<dyn Rpc>> = if needs_rpc { Some(host.rpc(&req.rpc_url)?) } else { None };
    let live_genesis = match &rpc {
        Some(r) => Some(r.genesis_hash()?),
        None => None,
    };
    let genesis = match (&req.genesis, req.cluster.known_genesis(), &live_genesis) {
        (Some(g), _, _) => g.clone(),
        (None, Some(k), _) => k.to_string(),
        (None, None, Some(l)) => l.clone(),
        (None, None, None) => {
            return Err(Error(format!(
                "cluster {} has no well-known genesis hash: pass --genesis-hash <hash> (or drop --offline so it can be read from the node)",
                req.cluster.name()
            )))
        }
    };
    if let Some(l) = &live_genesis {
        if *l != genesis {
            return Err(Error(format!(
                "refused: the node at {} is on genesis {l} but {} means {genesis}",
                crate::rpc::host_of(&req.rpc_url),
                req.cluster.name()
            )));
        }
    }

    // ---- resolve the instruction ----
    let admin = match &req.spec {
        AdminSpec::InitVault { usdc_mint, usdt_mint } => {
            AdminIx::InitVault { usdc_mint: *usdc_mint, usdt_mint: *usdt_mint }
        }
        AdminSpec::Register(p) => AdminIx::RegisterProduct(p.clone()),
        AdminSpec::Update(p) => AdminIx::UpdateProductConfig(p.clone()),
        AdminSpec::Pause(p) => AdminIx::PauseProduct { product: *p },
        AdminSpec::Reactivate(p) => AdminIx::ReactivateProduct { product: *p },
        AdminSpec::Withdraw { pool, amount, mint } => {
            if *amount == 0 {
                return Err(Error("refused: the amount is zero".into()));
            }
            let mint = match &rpc {
                Some(r) => {
                    let (m, n) = withdraw_preflight(r.as_ref(), &keys, *pool, *amount, *mint)?;
                    notes.extend(n);
                    m
                }
                None => {
                    notes.push("OFFLINE: no reserve pre-flight was done; the program refuses an amount above what its 25% reserve leaves".to_string());
                    mint.ok_or_else(|| Error("--offline needs --mint <pubkey> for admin-withdraw".into()))?
                }
            };
            AdminIx::Withdraw { pool: *pool, amount: *amount, mint }
        }
    };

    // ---- transaction lifetime ----
    let (nonce, blockhash) = match &req.lifetime {
        Lifetime::Nonce { account, authority, blockhash } => {
            let h = match (blockhash, &rpc) {
                (Some(h), _) => {
                    notes.push("the nonce value was supplied by hand and NOT verified against the chain".to_string());
                    *h
                }
                (None, Some(r)) => {
                    let acc = r.account(account)?.ok_or_else(|| {
                        Error(format!("the nonce account {account} does not exist (run nonce-create)"))
                    })?;
                    if acc.owner != SYSTEM_PROGRAM_ID {
                        return Err(Error(format!(
                            "{account} is not owned by the System Program, so it is not a nonce account"
                        )));
                    }
                    let (auth, hash) = parse_nonce_account(&acc.data)?;
                    if auth != *authority {
                        return Err(Error(format!("the nonce account's authority is {auth}, not {authority}")));
                    }
                    hash
                }
                (None, None) => {
                    return Err(Error(
                        "--offline with --nonce-account needs --nonce-blockhash <current nonce value>".into(),
                    ))
                }
            };
            (Some(NonceUse { account: *account, authority: *authority }), h)
        }
        Lifetime::Recent(h) => {
            notes.push("WARNING: a recent blockhash expires after about 60-90 seconds. Use this only if both signers sign and send within that window; otherwise use --nonce-account.".to_string());
            let h = match (h, &rpc) {
                (Some(h), _) => *h,
                (None, Some(r)) => {
                    r.latest_blockhash()?.parse().map_err(|_| Error("the node returned an invalid blockhash".into()))?
                }
                (None, None) => return Err(Error("--offline with --recent-blockhash needs the hash value".into())),
            };
            (None, h)
        }
    };

    let parts = Parts {
        fee_payer: req.fee_payer,
        blockhash,
        nonce: nonce.clone(),
        cu_limit: req.cu_limit,
        cu_price: req.cu_price,
        genesis: genesis.clone(),
        admin,
    };
    let msg = build_message(&keys, &parts);
    let signers: Vec<Pubkey> =
        msg.account_keys.iter().take(msg.header.num_required_signatures as usize).copied().collect();
    let cluster_name = match crate::cluster::label_for_genesis(&genesis) {
        l @ ("mainnet-beta" | "devnet") => l.to_string(),
        _ => req.cluster.name(),
    };
    let file = TxFile::new(
        &msg.serialize(),
        &cluster_name,
        &genesis,
        &description_text(&parts.admin),
        &signers,
        nonce.as_ref().map(|n| n.account),
    );

    // self-check: the tool must be able to inspect what it just built
    let insp =
        inspect(&file, &keys, &Expect { fee_payer: req.fee_payer, nonce: nonce.as_ref().map(|n| n.account) }, None)?;
    if !insp.is_clean() {
        host.out(&insp.render());
        return Err(Error(
            "internal error: the plan did not pass the tool's own inspection; nothing was written".into(),
        ));
    }
    file.save(out)?;

    host.out(&format!("Wrote {}\n\n", out.display()));
    for l in parts.admin.describe() {
        host.out(&format!("  {l}\n"));
    }
    host.out(&format!("\nCluster ........ {cluster_name} (genesis {genesis})\n"));
    host.out(&format!("Message SHA-256  {}\n", insp.message_hash));
    host.out(&format!("Signers needed . {}\n", signers.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")));
    for n in notes {
        host.out(&format!("{n}\n"));
    }
    host.out(
        "\nNext: send the file to each signer. Each signer runs `inspect`, then `sign`. Anyone can then `send`.\n",
    );
    Ok(file)
}
