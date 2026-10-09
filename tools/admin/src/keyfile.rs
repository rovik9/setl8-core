//! Reading a keypair file. Read-only: nothing in this tool ever writes secret key material.
//!
//! * the file must not be readable by group or others (unix) unless the caller overrides;
//! * parse errors never echo the file's contents;
//! * the raw bytes are zeroised as soon as the keypair is built (best effort).

use std::path::Path;

use solana_keypair::Keypair;
use zeroize::Zeroize;

use crate::error::{Error, Result};

pub struct LoadedKey {
    pub keypair: Keypair,
    /// A warning to show the user (only when the loose-permission override was used).
    pub warning: Option<String>,
}

pub fn check_permissions(path: &Path, allow_loose: bool) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|e| Error(format!("cannot read key file {}: {}", path.display(), e.kind())))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let msg = format!(
                "key file {} has mode {:03o}: other users can read it. Fix with: chmod 600 {}",
                path.display(),
                mode,
                path.display()
            );
            if allow_loose {
                return Ok(Some(format!("WARNING: {msg} (continuing because of --allow-loose-perms)")));
            }
            return Err(Error(format!("refused: {msg}")));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, allow_loose);
    }
    Ok(None)
}

pub fn load_keypair(path: &Path, allow_loose: bool) -> Result<LoadedKey> {
    let warning = check_permissions(path, allow_loose)?;
    let mut raw =
        std::fs::read(path).map_err(|e| Error(format!("cannot read key file {}: {}", path.display(), e.kind())))?;
    let generic = || {
        Error(format!(
            "{} is not a keypair file (expected a JSON array of 64 numbers, as written by solana-keygen)",
            path.display()
        ))
    };
    let parsed: std::result::Result<Vec<u8>, _> = serde_json::from_slice(&raw);
    raw.zeroize();
    let mut bytes = parsed.map_err(|_| generic())?;
    if bytes.len() != 64 {
        bytes.zeroize();
        return Err(generic());
    }
    let kp = Keypair::try_from(bytes.as_slice());
    let consistent = solana_keypair::keypair_from_seed(&bytes[..32]).map(|k| {
        use solana_signer::Signer;
        k.pubkey().to_bytes()[..] == bytes[32..]
    });
    bytes.zeroize();
    match (kp, consistent) {
        (Ok(k), Ok(true)) => Ok(LoadedKey { keypair: k, warning }),
        _ => Err(generic()),
    }
}
