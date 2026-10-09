//! `sign` and `add-signature`: attach exactly one verified signature to a transaction file.

use std::path::Path;

use anchor_lang::prelude::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;

use crate::admin_ix::Keys;
use crate::error::{Error, Result};
use crate::host::Host;
use crate::inspect::{inspect, Inspection};
use crate::keyfile::load_keypair;
use crate::message::Expect;
use crate::txfile::TxFile;

pub struct SignOpts {
    pub keys: Keys,
    pub expect: Expect,
    pub allow_loose_perms: bool,
}

/// Number of characters the human must retype.
pub const CONFIRM_CHARS: usize = 8;

fn report_progress(host: &mut dyn Host, insp: &Inspection) {
    let total = insp.sig_status.len();
    let have = insp.sig_status.iter().filter(|(_, s)| *s == crate::inspect::SigState::Valid).count();
    host.out(&format!("{have} of {total} required signatures are now present.\n"));
    let missing = insp.missing();
    if !missing.is_empty() {
        host.out(&format!("Still missing: {}\n", missing.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")));
    }
}

pub fn sign_file(host: &mut dyn Host, input: &Path, out: &Path, key_path: &Path, opts: &SignOpts) -> Result<()> {
    let mut file = TxFile::load(input)?;
    let insp = inspect(&file, &opts.keys, &opts.expect, None)?;
    host.out(&insp.render());
    if !insp.is_clean() {
        return Err(Error("refused: this transaction did not pass inspection; nothing was signed".into()));
    }
    let loaded = load_keypair(key_path, opts.allow_loose_perms)?;
    if let Some(w) = &loaded.warning {
        host.err(&format!("{w}\n"));
    }
    let signer: Pubkey = loaded.keypair.pubkey();
    match insp.state_of(&signer) {
        None => return Err(Error(format!("refused: {signer} is not one of this transaction's required signers"))),
        Some(crate::inspect::SigState::Valid) => {
            return Err(Error(format!("refused: {signer} has already signed this file")));
        }
        Some(_) => {}
    }
    if insp.is_mainnet() {
        host.out("\n*** THIS TRANSACTION IS BOUND TO MAINNET-BETA. IT MOVES OR CONTROLS REAL FUNDS. ***\n");
    }
    host.out(&format!("\nYou are about to sign as {signer}.\nMessage SHA-256: {}\n", insp.message_hash));
    let answer = host.prompt(&format!("To sign, retype the first {CONFIRM_CHARS} characters of the message hash: "))?;
    let want = &insp.message_hash[..CONFIRM_CHARS];
    if !answer.trim().eq_ignore_ascii_case(want) {
        return Err(Error("refused: that is not the start of the message hash; nothing was signed".into()));
    }
    let bytes = file.message_bytes()?;
    let sig = loaded.keypair.try_sign_message(&bytes).map_err(|_| Error("signing failed".into()))?;
    drop(loaded);
    file.add_signature(&signer, &sig, &bytes, &insp.classified.required_signers)?;
    file.save(out)?;
    let after = inspect(&file, &opts.keys, &opts.expect, None)?;
    host.out(&format!("\nSigned as {signer}. Wrote {}.\n", out.display()));
    report_progress(host, &after);
    Ok(())
}

pub fn add_signature(
    host: &mut dyn Host,
    input: &Path,
    out: &Path,
    pubkey: &Pubkey,
    signature: &Signature,
    opts: &SignOpts,
) -> Result<()> {
    let mut file = TxFile::load(input)?;
    let insp = inspect(&file, &opts.keys, &opts.expect, None)?;
    if !insp.is_clean() {
        host.out(&insp.render());
        return Err(Error("refused: this transaction did not pass inspection; nothing was added".into()));
    }
    let bytes = file.message_bytes()?;
    let changed = file.add_signature(pubkey, signature, &bytes, &insp.classified.required_signers)?;
    if changed {
        file.save(out)?;
        host.out(&format!("Signature of {pubkey} verified and added. Wrote {}.\n", out.display()));
    } else {
        host.out(&format!("{pubkey} already has this exact signature in the file; nothing changed.\n"));
    }
    let after = inspect(&file, &opts.keys, &opts.expect, None)?;
    report_progress(host, &after);
    Ok(())
}
