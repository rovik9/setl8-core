//! The transaction file passed between machines.
//!
//! Only `message_b64` and `signatures` are authoritative. Every other field is advisory
//! metadata for people and for cross-checking: `inspect` re-derives all of it from the
//! message bytes and shouts if the file disagrees.

use std::path::Path;

use anchor_lang::prelude::Pubkey;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_signature::Signature;

use crate::constants::TX_FILE_FORMAT;
use crate::error::{Error, Result};
use crate::fmt::hex;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SigEntry {
    pub pubkey: String,
    pub signature: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TxFile {
    pub format: String,
    pub message_b64: String,
    pub message_sha256: String,
    pub cluster: String,
    pub genesis_hash: String,
    pub description: String,
    pub required_signers: Vec<String>,
    pub nonce_account: Option<String>,
    #[serde(default)]
    pub signatures: Vec<SigEntry>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

impl TxFile {
    pub fn new(
        message: &[u8],
        cluster: &str,
        genesis: &str,
        description: &str,
        signers: &[Pubkey],
        nonce: Option<Pubkey>,
    ) -> TxFile {
        TxFile {
            format: TX_FILE_FORMAT.to_string(),
            message_b64: base64::engine::general_purpose::STANDARD.encode(message),
            message_sha256: sha256_hex(message),
            cluster: cluster.to_string(),
            genesis_hash: genesis.to_string(),
            description: description.to_string(),
            required_signers: signers.iter().map(|k| k.to_string()).collect(),
            nonce_account: nonce.map(|n| n.to_string()),
            signatures: vec![],
        }
    }

    pub fn message_bytes(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(self.message_b64.trim())
            .map_err(|_| Error("message_b64 is not valid base64".into()))
    }

    pub fn load(path: &Path) -> Result<TxFile> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error(format!("cannot read {}: {}", path.display(), e.kind())))?;
        serde_json::from_str(&text)
            .map_err(|e| Error(format!("{} is not a setl8-admin transaction file: {e}", path.display())))
    }

    /// Writes next to the target and renames, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|_| Error("cannot serialise the transaction file".into()))?
            + "\n";
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| Error(format!("cannot write {}: {}", tmp.display(), e.kind())))?;
        std::fs::rename(&tmp, path).map_err(|e| Error(format!("cannot replace {}: {}", path.display(), e.kind())))?;
        Ok(())
    }

    /// Attaches one signature after verifying it. Returns `true` if the file changed.
    /// The same pubkey with the same signature again is a no-op; a different signature for a
    /// pubkey that already has one is refused.
    pub fn add_signature(
        &mut self,
        pubkey: &Pubkey,
        signature: &Signature,
        message: &[u8],
        required: &[Pubkey],
    ) -> Result<bool> {
        if !required.contains(pubkey) {
            return Err(Error(format!("{pubkey} is not a required signer of this transaction")));
        }
        if !signature.verify(pubkey.as_ref(), message) {
            return Err(Error(format!("the signature does not verify for {pubkey} against this message")));
        }
        for e in &self.signatures {
            if e.pubkey == pubkey.to_string() {
                return if e.signature == signature.to_string() {
                    Ok(false)
                } else {
                    Err(Error(format!("{pubkey} already has a different signature in this file")))
                };
            }
        }
        self.signatures.push(SigEntry { pubkey: pubkey.to_string(), signature: signature.to_string() });
        Ok(true)
    }
}
