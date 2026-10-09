//! `nonce-create` and `nonce-advance`: single-signer transactions that manage the durable
//! nonce account the admin transactions use as their lifetime.

use std::path::Path;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::system_instruction;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;

use crate::cluster::{is_mainnet_genesis, Cluster};
use crate::constants::NONCE_ACCOUNT_LEN;
use crate::error::{Error, Result};
use crate::host::Host;
use crate::keyfile::load_keypair;
use crate::send::{mainnet_gate, wire_transaction};
use crate::txfile::sha256_hex;

pub struct NonceOpts<'a> {
    pub cluster: Cluster,
    pub rpc_url: String,
    pub key_path: &'a Path,
    pub allow_loose_perms: bool,
    pub mainnet_flag: bool,
}

fn submit(
    host: &mut dyn Host,
    o: &NonceOpts,
    msg: Message,
    extra_signers: &[&Keypair],
    authority: &Keypair,
    what: &str,
) -> Result<String> {
    let rpc = host.rpc(&o.rpc_url)?;
    let live = rpc.genesis_hash()?;
    if let Some(k) = o.cluster.known_genesis() {
        if k != live {
            return Err(Error(format!(
                "refused: --cluster {} means genesis {k} but the node at {} is on {live}",
                o.cluster.name(),
                crate::rpc::host_of(&o.rpc_url)
            )));
        }
    }
    let bytes = msg.serialize();
    let hash = sha256_hex(&bytes);
    host.out(&format!("\nMessage SHA-256: {hash}\nFee payer and authority: {}\n", authority.pubkey()));
    if is_mainnet_genesis(&live) {
        mainnet_gate(host, o.mainnet_flag, what)?;
    }
    let answer = host.prompt("To go ahead, retype the first 8 characters of the message hash: ")?;
    if !answer.trim().eq_ignore_ascii_case(&hash[..8]) {
        return Err(Error("refused: that is not the start of the message hash; nothing was sent".into()));
    }
    let mut signers: Vec<&Keypair> = vec![authority];
    signers.extend_from_slice(extra_signers);
    let required: Vec<Pubkey> =
        msg.account_keys.iter().take(msg.header.num_required_signatures as usize).copied().collect();
    let mut sigs = vec![];
    for r in &required {
        let kp = signers
            .iter()
            .find(|k| k.pubkey() == *r)
            .ok_or_else(|| Error(format!("no key for required signer {r}")))?;
        sigs.push(kp.try_sign_message(&bytes).map_err(|_| Error("signing failed".into()))?);
    }
    let tx = wire_transaction(&sigs, &bytes);
    let sim = rpc.simulate(&tx)?;
    if let Some(e) = sim.err {
        return Err(Error(format!("refused: the simulation failed ({e}); nothing was sent")));
    }
    let sig = rpc.send(&tx)?;
    host.out(&format!("Sent. Signature: {sig}\n"));
    rpc.confirm(&sig)?;
    host.out("Confirmed.\n");
    Ok(sig)
}

/// Creates a rent-exempt nonce account. The account's own key is generated in memory, signs
/// the creation and is dropped: it is never written anywhere and never needed again.
pub fn nonce_create(host: &mut dyn Host, o: &NonceOpts) -> Result<Pubkey> {
    let loaded = load_keypair(o.key_path, o.allow_loose_perms)?;
    if let Some(w) = &loaded.warning {
        host.err(&format!("{w}\n"));
    }
    let authority = loaded.keypair;
    let rpc = host.rpc(&o.rpc_url)?;
    let lamports = rpc.min_balance_for_rent(NONCE_ACCOUNT_LEN)?;
    let bh: Hash =
        rpc.latest_blockhash()?.parse().map_err(|_| Error("the node returned an invalid blockhash".into()))?;
    let nonce_kp = Keypair::new();
    let ixs = system_instruction::create_nonce_account(
        &authority.pubkey(),
        &nonce_kp.pubkey(),
        &authority.pubkey(),
        lamports,
    );
    let msg = Message::new_with_blockhash(&ixs, Some(&authority.pubkey()), &bh);
    host.out(&format!(
        "Create durable nonce account {}\n  rent-exempt funding: {lamports} lamports, paid by {}\n  nonce authority: {} (only this key can advance or withdraw it)\n",
        nonce_kp.pubkey(),
        authority.pubkey(),
        authority.pubkey()
    ));
    submit(host, o, msg, &[&nonce_kp], &authority, "nonce-create transaction")?;
    host.out(&format!(
        "\nNonce account: {}\nUse it with: plan ... --nonce-account {}\n",
        nonce_kp.pubkey(),
        nonce_kp.pubkey()
    ));
    Ok(nonce_kp.pubkey())
}

/// Advances the nonce, which invalidates every transaction built on its current value.
pub fn nonce_advance(host: &mut dyn Host, o: &NonceOpts, nonce: &Pubkey) -> Result<()> {
    let loaded = load_keypair(o.key_path, o.allow_loose_perms)?;
    if let Some(w) = &loaded.warning {
        host.err(&format!("{w}\n"));
    }
    let authority = loaded.keypair;
    let rpc = host.rpc(&o.rpc_url)?;
    let acc = rpc.account(nonce)?.ok_or_else(|| Error(format!("the nonce account {nonce} does not exist")))?;
    let (auth, _) = crate::plan::parse_nonce_account(&acc.data)?;
    if auth != authority.pubkey() {
        return Err(Error(format!(
            "refused: the nonce authority is {auth}, but the key you gave is {}",
            authority.pubkey()
        )));
    }
    let bh: Hash =
        rpc.latest_blockhash()?.parse().map_err(|_| Error("the node returned an invalid blockhash".into()))?;
    let ix = system_instruction::advance_nonce_account(nonce, &authority.pubkey());
    let msg = Message::new_with_blockhash(&[ix], Some(&authority.pubkey()), &bh);
    host.out(&format!(
        "Advance durable nonce {nonce}.\nTHIS INVALIDATES EVERY TRANSACTION ALREADY SIGNED AGAINST THE CURRENT NONCE VALUE (signed-but-unsent authorisations are revoked).\n"
    ));
    submit(host, o, msg, &[], &authority, "nonce-advance transaction")?;
    Ok(())
}
