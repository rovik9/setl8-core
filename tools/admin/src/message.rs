//! Building, parsing and classifying the transaction message.
//!
//! The allowlist lives here: a message is signable only if its instructions are exactly
//!
//! ```text
//!   [AdvanceNonceAccount]?  [SetComputeUnitLimit]?  [SetComputeUnitPrice]?  Memo  <one admin instruction>
//! ```
//!
//! and the message is byte-for-byte what [`build_message`] produces for the decoded
//! contents. Anything else is reported as a problem and `sign` refuses.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::system_instruction;
use solana_hash::Hash;
use solana_message::Message;

use crate::admin_ix::{AdminIx, Keys};
use crate::constants::{COMPUTE_BUDGET_ID, MEMO_PREFIX, MEMO_PROGRAM_ID, SYSTEM_PROGRAM_ID};
use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonceUse {
    pub account: Pubkey,
    pub authority: Pubkey,
}

/// Everything a message says, in decoded form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parts {
    pub fee_payer: Pubkey,
    pub blockhash: Hash,
    pub nonce: Option<NonceUse>,
    pub cu_limit: Option<u32>,
    pub cu_price: Option<u64>,
    pub genesis: String,
    pub admin: AdminIx,
}

pub fn memo_text(genesis: &str) -> String {
    format!("{MEMO_PREFIX}{genesis}")
}

/// The canonical instruction list for `parts`.
pub fn build_instructions(keys: &Keys, parts: &Parts) -> Vec<Instruction> {
    let mut ixs = vec![];
    if let Some(n) = &parts.nonce {
        ixs.push(system_instruction::advance_nonce_account(&n.account, &n.authority));
    }
    if let Some(u) = parts.cu_limit {
        let mut data = vec![2u8];
        data.extend_from_slice(&u.to_le_bytes());
        ixs.push(Instruction { program_id: COMPUTE_BUDGET_ID, accounts: vec![], data });
    }
    if let Some(p) = parts.cu_price {
        let mut data = vec![3u8];
        data.extend_from_slice(&p.to_le_bytes());
        ixs.push(Instruction { program_id: COMPUTE_BUDGET_ID, accounts: vec![], data });
    }
    ixs.push(Instruction {
        program_id: MEMO_PROGRAM_ID,
        accounts: vec![],
        data: memo_text(&parts.genesis).into_bytes(),
    });
    ixs.push(parts.admin.build(keys));
    ixs
}

pub fn build_message(keys: &Keys, parts: &Parts) -> Message {
    Message::new_with_blockhash(&build_instructions(keys, parts), Some(&parts.fee_payer), &parts.blockhash)
}

/// Strict parse: legacy format only, and the bytes must be exactly what re-serialising gives.
pub fn parse_message(bytes: &[u8]) -> Result<Message> {
    if bytes.first().map(|b| *b >= 0x80).unwrap_or(true) {
        return Err(Error("not a legacy transaction message (versioned messages are not accepted)".into()));
    }
    let m: Message = bincode::deserialize(bytes).map_err(|_| Error("the message bytes do not decode".into()))?;
    if m.serialize() != bytes {
        return Err(Error("the message bytes are not canonical (trailing or re-ordered bytes)".into()));
    }
    Ok(m)
}

#[derive(Clone, Debug)]
pub struct AcctView {
    pub key: Pubkey,
    pub signer: bool,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct IxView {
    pub index: usize,
    pub program: Pubkey,
    pub kind: String,
    pub accounts: Vec<AcctView>,
    pub data_len: usize,
}

#[derive(Clone, Debug)]
pub struct AccountCheck {
    pub position: usize,
    pub role: String,
    pub key: Pubkey,
    pub signer: bool,
    pub writable: bool,
    pub note: String,
    /// `None` = fine; `Some(reason)` = a problem.
    pub fault: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Classified {
    pub parts: Option<Parts>,
    pub problems: Vec<String>,
    pub ixs: Vec<IxView>,
    pub account_checks: Vec<AccountCheck>,
    pub required_signers: Vec<Pubkey>,
}

/// What the signer states they expect (everything else is re-derived).
#[derive(Clone, Debug)]
pub struct Expect {
    pub fee_payer: Pubkey,
    pub nonce: Option<Pubkey>,
}

fn system_kind(data: &[u8]) -> String {
    if data.len() >= 4 {
        let tag = u32::from_le_bytes(data[..4].try_into().unwrap());
        let name = match tag {
            0 => "CreateAccount",
            1 => "Assign",
            2 => "Transfer",
            3 => "CreateAccountWithSeed",
            4 => "AdvanceNonceAccount",
            5 => "WithdrawNonceAccount",
            6 => "InitializeNonceAccount",
            7 => "AuthorizeNonceAccount",
            8 => "Allocate",
            9 => "AllocateWithSeed",
            10 => "AssignWithSeed",
            11 => "TransferWithSeed",
            _ => "unknown",
        };
        if tag == 2 && data.len() == 12 {
            let lamports = u64::from_le_bytes(data[4..12].try_into().unwrap());
            return format!("System Program {name} of {lamports} lamports");
        }
        return format!("System Program {name}");
    }
    "System Program (undecodable)".into()
}

/// Decode every instruction, enforce the allowlist and the canonical form.
pub fn classify(keys: &Keys, msg: &Message, expect: &Expect) -> Classified {
    let mut c = Classified::default();
    let n_sig = msg.header.num_required_signatures as usize;
    c.required_signers = msg.account_keys.iter().take(n_sig).copied().collect();

    // 1. resolve
    let mut resolved: Vec<(Pubkey, Vec<Pubkey>, Vec<u8>)> = vec![];
    for (i, ci) in msg.instructions.iter().enumerate() {
        let Some(program) = msg.account_keys.get(ci.program_id_index as usize).copied() else {
            c.problems.push(format!("instruction #{i} names a program that is not in the account list"));
            return c;
        };
        let mut accts = vec![];
        let mut views = vec![];
        for a in &ci.accounts {
            let Some(k) = msg.account_keys.get(*a as usize).copied() else {
                c.problems.push(format!("instruction #{i} names an account that is not in the account list"));
                return c;
            };
            accts.push(k);
            views.push(AcctView {
                key: k,
                signer: msg.is_signer(*a as usize),
                writable: msg.is_maybe_writable(*a as usize, None),
            });
        }
        c.ixs.push(IxView { index: i, program, kind: String::new(), accounts: views, data_len: ci.data.len() });
        resolved.push((program, accts, ci.data.clone()));
    }

    // 2. classify each instruction
    let mut nonce: Option<NonceUse> = None;
    let (mut cu_limit, mut cu_price, mut genesis): (Option<u32>, Option<u64>, Option<String>) = (None, None, None);
    let mut admin: Option<(usize, AdminIx)> = None;
    let mut vault_ix_count = 0;
    for (i, (program, accts, data)) in resolved.iter().enumerate() {
        let kind: String;
        if *program == SYSTEM_PROGRAM_ID {
            if data.as_slice() == [4, 0, 0, 0] && accts.len() == 3 {
                if i != 0 {
                    c.problems.push(format!("instruction #{i}: AdvanceNonceAccount must be the FIRST instruction"));
                }
                if nonce.is_some() {
                    c.problems.push("more than one AdvanceNonceAccount".to_string());
                }
                nonce = Some(NonceUse { account: accts[0], authority: accts[2] });
                kind = format!("System Program AdvanceNonceAccount (durable nonce {})", accts[0]);
            } else {
                kind = system_kind(data);
                c.problems.push(format!(
                    "instruction #{i} is `{kind}`: only AdvanceNonceAccount of the declared nonce is allowed from the System Program"
                ));
            }
        } else if *program == COMPUTE_BUDGET_ID {
            if accts.is_empty() && data.len() == 5 && data[0] == 2 {
                if cu_limit.is_some() {
                    c.problems.push("more than one SetComputeUnitLimit".to_string());
                }
                cu_limit = Some(u32::from_le_bytes(data[1..5].try_into().unwrap()));
                kind = format!("ComputeBudget SetComputeUnitLimit({})", cu_limit.unwrap());
            } else if accts.is_empty() && data.len() == 9 && data[0] == 3 {
                if cu_price.is_some() {
                    c.problems.push("more than one SetComputeUnitPrice".to_string());
                }
                cu_price = Some(u64::from_le_bytes(data[1..9].try_into().unwrap()));
                kind = format!("ComputeBudget SetComputeUnitPrice({} micro-lamports)", cu_price.unwrap());
            } else {
                kind = "ComputeBudget (other)".to_string();
                c.problems.push(format!("instruction #{i} is a ComputeBudget instruction other than SetComputeUnitLimit / SetComputeUnitPrice"));
            }
        } else if *program == MEMO_PROGRAM_ID {
            let text = std::str::from_utf8(data).unwrap_or("");
            match text.strip_prefix(MEMO_PREFIX) {
                Some(g) if accts.is_empty() && !g.is_empty() => {
                    if genesis.is_some() {
                        c.problems.push("more than one memo".to_string());
                    }
                    genesis = Some(g.to_string());
                    kind = format!("Memo: {text}");
                }
                _ => {
                    kind = "Memo (unexpected content)".to_string();
                    c.problems.push(format!(
                        "instruction #{i} is a Memo that is not exactly `{MEMO_PREFIX}<genesis hash>` with no accounts"
                    ));
                }
            }
        } else if *program == keys.program_id {
            vault_ix_count += 1;
            match AdminIx::decode(data, accts) {
                Ok(a) => {
                    kind = format!("core-vault {}", a.name());
                    admin = Some((i, a));
                }
                Err(e) => {
                    kind = "core-vault (NOT an admin instruction)".to_string();
                    c.problems.push(format!("instruction #{i} to the vault program: {e}"));
                }
            }
        } else {
            kind = format!("UNKNOWN PROGRAM {program}");
            c.problems.push(format!("instruction #{i} calls program {program}, which is not the vault, the System Program (nonce), ComputeBudget or Memo"));
        }
        c.ixs[i].kind = kind;
    }
    if vault_ix_count != 1 {
        c.problems.push(format!(
            "the message must contain exactly ONE instruction to the vault program, it has {vault_ix_count}"
        ));
    }
    if genesis.is_none() {
        c.problems
            .push("no `setl8-admin/1 genesis=<hash>` memo: this transaction is not bound to a cluster".to_string());
    }

    // 3. the signer's own expectations
    if msg.account_keys.first() != Some(&expect.fee_payer) {
        c.problems.push(format!(
            "fee payer is {} but {} was expected (state the expected one with --fee-payer)",
            msg.account_keys.first().map(|k| k.to_string()).unwrap_or_else(|| "(none)".into()),
            expect.fee_payer
        ));
    }
    if let Some(want) = expect.nonce {
        match &nonce {
            Some(n) if n.account == want => {}
            Some(n) => c.problems.push(format!("the message advances nonce {} but {want} was declared", n.account)),
            None => c.problems.push(format!("durable nonce {want} was declared but the message does not advance it (it uses a short-lived recent blockhash)")),
        }
    }
    if let Some(n) = &nonce {
        if ![keys.sl8, keys.rov, expect.fee_payer].contains(&n.authority) {
            c.problems.push(format!("nonce authority {} is neither an admin key nor the fee payer", n.authority));
        }
    }

    // 4. rebuild and compare
    if let (Some((_, a)), Some(g), Some(fee_payer)) =
        (admin.clone(), genesis.clone(), msg.account_keys.first().copied())
    {
        let parts = Parts {
            fee_payer,
            blockhash: msg.recent_blockhash,
            nonce,
            cu_limit,
            cu_price,
            genesis: g,
            admin: a.clone(),
        };
        let want = build_message(keys, &parts);
        if want.serialize() != msg.serialize() {
            c.problems.push(
                "the message is not byte-for-byte the canonical transaction for its decoded contents (instruction order, account list or account flags differ from what this tool builds)".to_string(),
            );
        }
        // per-account diagnostics for the vault instruction
        let want_ix = a.build(keys);
        let (vi, _) = admin.as_ref().unwrap();
        let got = &c.ixs[*vi].accounts;
        let roles = a.account_roles();
        let notes = a.account_notes();
        if got.len() != want_ix.accounts.len() {
            c.problems.push(format!(
                "the vault instruction has {} accounts, {} expected",
                got.len(),
                want_ix.accounts.len()
            ));
        }
        for (pos, w) in want_ix.accounts.iter().enumerate() {
            if let Some(g) = got.get(pos) {
                let fault = if g.key != w.pubkey {
                    Some(format!("expected {}", w.pubkey))
                } else if w.is_signer && !g.signer {
                    Some("must be a signer".to_string())
                } else {
                    None
                };
                if let Some(f) = &fault {
                    c.problems.push(format!("account #{pos} ({}): {f}", roles[pos]));
                }
                c.account_checks.push(AccountCheck {
                    position: pos,
                    role: roles[pos].to_string(),
                    key: g.key,
                    signer: g.signer,
                    writable: g.writable,
                    note: notes[pos].to_string(),
                    fault,
                });
            }
        }
        c.parts = Some(parts);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(k: &Keys) -> Parts {
        Parts {
            fee_payer: k.sl8,
            blockhash: Hash::new_unique(),
            nonce: None,
            cu_limit: None,
            cu_price: None,
            genesis: "G".into(),
            admin: AdminIx::PauseProduct { product: Pubkey::new_unique() },
        }
    }

    #[test]
    fn a_built_message_classifies_clean_and_is_canonical() {
        let k = Keys::compiled();
        let mut p = parts(&k);
        p.nonce = Some(NonceUse { account: Pubkey::new_unique(), authority: k.sl8 });
        p.cu_limit = Some(200_000);
        p.cu_price = Some(5);
        let m = build_message(&k, &p);
        let c = classify(&k, &m, &Expect { fee_payer: k.sl8, nonce: p.nonce.as_ref().map(|n| n.account) });
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert_eq!(c.parts.unwrap(), p);
        assert_eq!(parse_message(&m.serialize()).unwrap(), m);
    }

    #[test]
    fn versioned_and_trailing_bytes_are_refused() {
        let k = Keys::compiled();
        let m = build_message(&k, &parts(&k));
        let mut b = m.serialize();
        b.push(0);
        assert!(parse_message(&b).is_err());
        assert!(parse_message(&[0x80, 0, 0]).is_err());
        assert!(parse_message(&[]).is_err());
    }
}
