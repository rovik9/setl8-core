//! `send`: the only command that puts a signed admin transaction on a network.

use std::path::Path;

use crate::admin_ix::Keys;
use crate::cluster::{is_mainnet_genesis, label_for_genesis};
use crate::error::{Error, Result};
use crate::host::Host;
use crate::inspect::inspect;
use crate::message::Expect;
use crate::txfile::TxFile;

pub struct SendOpts {
    pub keys: Keys,
    pub expect: Expect,
    pub rpc_url: String,
    /// `--i-understand-this-is-mainnet`
    pub mainnet_flag: bool,
}

/// The wire form of a transaction: short-vec of signatures, then the message.
pub fn wire_transaction(sigs: &[solana_signature::Signature], message: &[u8]) -> Vec<u8> {
    assert!(sigs.len() < 128, "more than 127 signatures cannot occur in an admin transaction");
    let mut tx = vec![sigs.len() as u8];
    for s in sigs {
        tx.extend_from_slice(s.as_ref());
    }
    tx.extend_from_slice(message);
    tx
}

/// Enforces the mainnet gate: the flag AND the typed word.
pub fn mainnet_gate(host: &mut dyn Host, flag: bool, what: &str) -> Result<()> {
    if !flag {
        return Err(Error(format!(
            "refused: this {what} targets MAINNET-BETA. Add --i-understand-this-is-mainnet and type MAINNET when asked."
        )));
    }
    host.out(&format!("\n*** MAINNET-BETA: this {what} affects real funds and cannot be undone. ***\n"));
    let word = host.prompt("Type MAINNET to continue: ")?;
    if word.trim() != "MAINNET" {
        return Err(Error("refused: you did not type MAINNET; nothing was sent".into()));
    }
    Ok(())
}

pub fn send_file(host: &mut dyn Host, path: &Path, opts: &SendOpts) -> Result<String> {
    let file = TxFile::load(path)?;
    let insp = inspect(&file, &opts.keys, &opts.expect, None)?;
    if !insp.is_clean() {
        host.out(&insp.render());
        return Err(Error("refused: this transaction did not pass inspection; nothing was sent".into()));
    }
    if !insp.all_signed() {
        let missing: Vec<String> = insp.missing().iter().map(|k| k.to_string()).collect();
        return Err(Error(format!("refused: signatures are missing or invalid for: {}", missing.join(", "))));
    }
    let genesis = insp.genesis.clone().ok_or_else(|| Error("refused: the message names no cluster".into()))?;
    if insp.is_mainnet() && !opts.mainnet_flag {
        return Err(Error("refused: this transaction is bound to MAINNET-BETA. Add --i-understand-this-is-mainnet and type MAINNET when asked.".into()));
    }
    let rpc = host.rpc(&opts.rpc_url)?;
    let live = rpc.genesis_hash()?;
    if live != genesis {
        return Err(Error(format!(
            "refused: the node at {} is on {} (genesis {live}) but the transaction is bound to {} (genesis {genesis})",
            crate::rpc::host_of(&opts.rpc_url),
            label_for_genesis(&live),
            label_for_genesis(&genesis)
        )));
    }
    host.out(&insp.render());
    if is_mainnet_genesis(&genesis) {
        mainnet_gate(host, opts.mainnet_flag, "transaction")?;
    }
    let bytes = file.message_bytes()?;
    // order the signatures like the message's signer list
    let ordered: Vec<solana_signature::Signature> = insp
        .classified
        .required_signers
        .iter()
        .map(|k| {
            let e = file.signatures.iter().find(|e| e.pubkey == k.to_string()).expect("all_signed guarantees an entry");
            e.signature.parse().expect("all_signed guarantees a valid signature")
        })
        .collect();
    let tx = wire_transaction(&ordered, &bytes);

    let sim = rpc.simulate(&tx)?;
    host.out(&format!("\nSimulation: {}", if sim.err.is_none() { "OK" } else { "FAILED" }));
    if let Some(u) = sim.units {
        host.out(&format!(" ({u} compute units)"));
    }
    host.out("\n");
    for l in sim.logs.iter().filter(|l| l.contains("Program log:") || l.contains("failed")) {
        host.out(&format!("  {l}\n"));
    }
    if let Some(e) = sim.err {
        return Err(Error(format!("refused: the simulation failed ({e}); nothing was sent")));
    }
    let sig = rpc.send(&tx)?;
    host.out(&format!("Sent. Signature: {sig}\nWaiting for confirmation...\n"));
    rpc.confirm(&sig)?;
    host.out("Confirmed.\n");
    Ok(sig)
}
