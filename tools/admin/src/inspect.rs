//! `inspect`: decode a transaction file from its MESSAGE BYTES ALONE and report what it
//! would do. The file's own metadata is only compared against the result, never believed.

use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use solana_signature::Signature;

use crate::admin_ix::{AdminIx, Keys, Side};
use crate::cluster::{is_mainnet_genesis, label_for_genesis};
use crate::constants::TX_FILE_FORMAT;
use crate::error::Result;
use crate::fmt::fmt_amount;
use crate::message::{classify, parse_message, Classified, Expect};
use crate::rpc::Rpc;
use crate::txfile::{sha256_hex, TxFile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SigState {
    Valid,
    Invalid,
    Missing,
}

#[derive(Clone, Debug)]
pub struct Inspection {
    pub keys: Keys,
    pub message_hash: String,
    pub genesis: Option<String>,
    pub classified: Classified,
    pub sig_status: Vec<(Pubkey, SigState)>,
    /// The file's metadata disagrees with the message bytes (shown loudly).
    pub mismatches: Vec<String>,
    /// Everything that makes the transaction unsignable.
    pub problems: Vec<String>,
    /// Facts only the network can confirm (filled when `--rpc` is given).
    pub online: Vec<String>,
    pub compiled_in_program: bool,
}

impl Inspection {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty() && self.mismatches.is_empty()
    }

    /// Every required signer has a valid signature (counted per distinct required key).
    pub fn all_signed(&self) -> bool {
        !self.sig_status.is_empty() && self.sig_status.iter().all(|(_, s)| *s == SigState::Valid)
    }

    pub fn state_of(&self, key: &Pubkey) -> Option<&SigState> {
        self.sig_status.iter().find(|(k, _)| k == key).map(|(_, s)| s)
    }

    pub fn missing(&self) -> Vec<Pubkey> {
        self.sig_status.iter().filter(|(_, s)| *s != SigState::Valid).map(|(k, _)| *k).collect()
    }

    pub fn is_mainnet(&self) -> bool {
        self.genesis.as_deref().map(is_mainnet_genesis).unwrap_or(false)
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let k = &self.keys;
        o.push_str("================ setl8-admin inspect ================\n");
        if !self.mismatches.is_empty() {
            o.push_str("!!!! THE FILE'S METADATA DISAGREES WITH THE MESSAGE BYTES. DO NOT SIGN. !!!!\n");
            for m in &self.mismatches {
                o.push_str(&format!("  MISMATCH: {m}\n"));
            }
            o.push('\n');
        }
        o.push_str(&format!(
            "(everything below was decoded from the message bytes; this build embeds the {} admin keys)\n",
            if cfg!(feature = "localnet") { "PUBLIC TEST" } else { "REAL" }
        ));
        match &self.genesis {
            Some(g) => o.push_str(&format!("Cluster ........ {}   genesis {g}\n", label_for_genesis(g))),
            None => o.push_str("Cluster ........ UNKNOWN (no genesis memo)\n"),
        }
        o.push_str(&format!(
            "Program ........ {}{}\n",
            k.program_id,
            if self.compiled_in_program {
                "   (this build's compiled-in program id)"
            } else {
                "   (OVERRIDDEN with --program-id)"
            }
        ));
        o.push_str(&format!(
            "Vault PDA ...... {}   (re-derived from program id + SL8 {} + ROV {})\n",
            k.vault(),
            k.sl8,
            k.rov
        ));
        if let Some(p) = &self.classified.parts {
            o.push_str(&format!("Instruction .... {}\n", p.admin.name()));
            for l in p.admin.describe() {
                o.push_str(&format!("    {l}\n"));
            }
            o.push_str(&format!("Fee payer ...... {}{}\n", p.fee_payer, role_suffix(k, &p.fee_payer)));
            match &p.nonce {
                Some(n) => o.push_str(&format!(
                    "Lifetime ....... DURABLE NONCE {} (authority {}). Valid until the nonce is advanced.\n",
                    n.account, n.authority
                )),
                None => o.push_str("Lifetime ....... RECENT BLOCKHASH: valid for only ~60-90 seconds after it was fetched (same-session use only)\n"),
            }
            o.push_str(&format!("Blockhash ...... {}\n", p.blockhash));
            if let Some(u) = p.cu_limit {
                o.push_str(&format!("Compute limit .. {u} units\n"));
            }
            if let Some(u) = p.cu_price {
                o.push_str(&format!("Priority fee ... {u} micro-lamports per unit\n"));
            }
        } else {
            o.push_str("Instruction .... COULD NOT BE DECODED AS AN ADMIN TRANSACTION\n");
        }
        o.push_str(&format!("\nInstructions in the message ({}):\n", self.classified.ixs.len()));
        for ix in &self.classified.ixs {
            o.push_str(&format!(
                "  #{} program {}  {}  ({} accounts, {} data bytes)\n",
                ix.index,
                ix.program,
                ix.kind,
                ix.accounts.len(),
                ix.data_len
            ));
        }
        if !self.classified.account_checks.is_empty() {
            o.push_str("\nAccounts of the vault instruction:\n");
            for a in &self.classified.account_checks {
                o.push_str(&format!(
                    "  #{} {:<18} {}  {}{}\n      {}{}\n",
                    a.position,
                    a.role,
                    a.key,
                    if a.signer { "signer " } else { "" },
                    if a.writable { "writable" } else { "read-only" },
                    a.note,
                    match &a.fault {
                        Some(f) => format!("   <<<< WRONG: {f}"),
                        None => String::new(),
                    }
                ));
            }
        }
        o.push_str("\nSignatures:\n");
        for (key, st) in &self.sig_status {
            let label = match st {
                SigState::Valid => "present, VALID",
                SigState::Invalid => "present but INVALID (does not verify against this message)",
                SigState::Missing => "MISSING",
            };
            o.push_str(&format!("  {key}{}  {label}\n", role_suffix(k, key)));
        }
        o.push_str(&format!("\nMessage SHA-256: {}\n", self.message_hash));
        o.push_str("  (compare this out of band with the preparer; it covers every byte you are signing)\n");
        if self.online.is_empty() {
            o.push_str("\nOFFLINE inspection: mint addresses and live balances were NOT checked against the chain. Re-run with --rpc <url> for that.\n");
        } else {
            o.push_str("\nOnline checks:\n");
            for l in &self.online {
                o.push_str(&format!("  {l}\n"));
            }
        }
        if self.problems.is_empty() && self.mismatches.is_empty() {
            o.push_str("\nRESULT: this is a well-formed setl8-admin transaction. Read the instruction above; it is what you would be authorising.\n");
        } else {
            o.push_str("\nRESULT: DO NOT SIGN. Problems:\n");
            for p in &self.problems {
                o.push_str(&format!("  - {p}\n"));
            }
        }
        o
    }
}

fn role_suffix(k: &Keys, key: &Pubkey) -> &'static str {
    if *key == k.sl8 {
        "  [SL8 admin]"
    } else if *key == k.rov {
        "  [ROV admin]"
    } else {
        ""
    }
}

/// One-line description the preparer stores in the file; `inspect` compares it.
pub fn description_text(admin: &AdminIx) -> String {
    admin.describe().join("; ")
}

pub fn inspect(file: &TxFile, keys: &Keys, expect: &Expect, rpc: Option<&dyn Rpc>) -> Result<Inspection> {
    let bytes = file.message_bytes()?;
    let msg = parse_message(&bytes)?;
    let classified = classify(keys, &msg, expect);
    let message_hash = sha256_hex(&bytes);
    let genesis = classified.parts.as_ref().map(|p| p.genesis.clone());

    // metadata vs message
    let mut mismatches = vec![];
    if file.format != TX_FILE_FORMAT {
        mismatches.push(format!("file format is '{}', this tool reads '{TX_FILE_FORMAT}'", file.format));
    }
    if file.message_sha256 != message_hash {
        mismatches.push(format!(
            "file says the message hash is {}, the message bytes hash to {message_hash}",
            file.message_sha256
        ));
    }
    match &genesis {
        Some(g) => {
            if &file.genesis_hash != g {
                mismatches.push(format!("file says genesis {}, the message is bound to {g}", file.genesis_hash));
            }
            let label = label_for_genesis(g);
            let known = label == "mainnet-beta" || label == "devnet";
            if known && file.cluster != label {
                mismatches.push(format!("file says cluster '{}', the message is bound to {label}", file.cluster));
            }
        }
        None => mismatches.push("the message carries no genesis memo but the file claims a cluster".to_string()),
    }
    let actual_signers: Vec<String> = classified.required_signers.iter().map(|k| k.to_string()).collect();
    if file.required_signers != actual_signers {
        mismatches
            .push(format!("file lists signers {:?}, the message requires {:?}", file.required_signers, actual_signers));
    }
    let message_nonce = classified.parts.as_ref().and_then(|p| p.nonce.as_ref().map(|n| n.account.to_string()));
    if file.nonce_account != message_nonce {
        mismatches
            .push(format!("file says nonce account {:?}, the message uses {:?}", file.nonce_account, message_nonce));
    }
    if let Some(p) = &classified.parts {
        let want = description_text(&p.admin);
        if file.description != want {
            mismatches.push(format!(
                "file describes this as \"{}\", the message actually does: \"{want}\"",
                file.description
            ));
        }
    }

    // signatures
    let mut problems = classified.problems.clone();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for e in &file.signatures {
        *seen.entry(e.pubkey.clone()).or_default() += 1;
    }
    for (k, n) in &seen {
        if *n > 1 {
            problems.push(format!("signature entry for {k} appears {n} times"));
        }
    }
    let mut sig_status = vec![];
    for req in &classified.required_signers {
        let entries: Vec<_> = file.signatures.iter().filter(|e| e.pubkey == req.to_string()).collect();
        let state = match entries.first() {
            None => SigState::Missing,
            Some(e) => match e.signature.parse::<Signature>() {
                Ok(sig) if sig.verify(req.as_ref(), &bytes) => SigState::Valid,
                _ => SigState::Invalid,
            },
        };
        if state == SigState::Invalid {
            problems.push(format!("the signature of {req} does not verify against this message (the message was changed after it signed, or the signature is corrupt)"));
        }
        sig_status.push((*req, state));
    }
    for e in &file.signatures {
        let known = classified.required_signers.iter().any(|k| k.to_string() == e.pubkey);
        if !known {
            problems.push(format!("the file carries a signature for {}, which is not a required signer", e.pubkey));
        }
    }

    let mut insp = Inspection {
        keys: *keys,
        message_hash,
        genesis,
        classified,
        sig_status,
        mismatches,
        problems,
        online: vec![],
        compiled_in_program: keys.program_id == core_vault::ID,
    };
    if let Some(rpc) = rpc {
        online_checks(rpc, &mut insp)?;
    }
    Ok(insp)
}

/// Facts that need the chain: genesis, and the vault's own records.
fn online_checks(rpc: &dyn Rpc, insp: &mut Inspection) -> Result<()> {
    let keys = insp.keys;
    if let Some(g) = &insp.genesis {
        let live = rpc.genesis_hash()?;
        if &live == g {
            insp.online.push(format!("the node's genesis hash equals the message's ({})", label_for_genesis(g)));
        } else {
            insp.problems.push(format!("the node is on genesis {live}, the message is bound to {g}"));
        }
    }
    let Some(parts) = insp.classified.parts.clone() else { return Ok(()) };
    let vault = rpc.account(&keys.vault())?;
    let vault_state = vault.as_ref().and_then(|a| {
        use anchor_lang::AccountDeserialize;
        let mut d = a.data.as_slice();
        core_vault::state::VaultState::try_deserialize(&mut d).ok()
    });
    match &parts.admin {
        AdminIx::InitVault { .. } => {
            if vault.is_some() {
                insp.problems.push("the vault already exists on this cluster; init_vault would fail".to_string());
            } else {
                insp.online.push("the vault does not exist yet (as expected for init_vault)".to_string());
            }
        }
        AdminIx::RegisterProduct(p) => {
            if rpc.account(&keys.registry(&p.product_program_id))?.is_some() {
                insp.problems.push("this product is already registered; register_product would fail".to_string());
            } else {
                insp.online.push("the product is not registered yet (as expected)".to_string());
            }
        }
        AdminIx::UpdateProductConfig(p) => {
            if rpc.account(&keys.registry(&p.product_program_id))?.is_none() {
                insp.problems.push("this product is not registered; update_product_config would fail".to_string());
            } else {
                insp.online.push("the product is registered".to_string());
            }
        }
        AdminIx::PauseProduct { product } | AdminIx::ReactivateProduct { product } => {
            if rpc.account(&keys.registry(product))?.is_none() {
                insp.problems.push("this product is not registered".to_string());
            } else {
                insp.online.push("the product is registered".to_string());
            }
        }
        AdminIx::Withdraw { pool, amount, mint } => {
            match vault_state {
                None => insp.problems.push("the vault does not exist on this cluster".to_string()),
                Some(vs) => {
                    let want_mint = match pool {
                        Side::Usdc => vs.usdc_mint,
                        Side::Usdt => vs.usdt_mint,
                    };
                    if want_mint == *mint {
                        insp.online.push(format!("the mint is the vault's {} mint", pool.name()));
                    } else {
                        insp.problems.push(format!(
                            "the mint in the message ({mint}) is not the vault's {} mint ({want_mint})",
                            pool.name()
                        ));
                    }
                    if let Some(acc) = rpc.account(&keys.pool(mint))? {
                        use anchor_spl::token::spl_token::{solana_program::program_pack::Pack, state::Account as Tok};
                        if let Ok(t) = Tok::unpack(&acc.data) {
                            let floor = match pool {
                                Side::Usdc => vs.usdc_floor,
                                Side::Usdt => vs.usdt_floor,
                            };
                            let max = core_vault::utils::withdrawable(t.amount, floor).unwrap_or(0);
                            insp.online.push(format!(
                                "the pool holds {}; the program would let at most {} out right now",
                                fmt_amount(t.amount),
                                fmt_amount(max)
                            ));
                            if *amount > max {
                                insp.problems.push(format!("the amount {} is above what the 25% reserve leaves ({}): the program would refuse it", fmt_amount(*amount), fmt_amount(max)));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
