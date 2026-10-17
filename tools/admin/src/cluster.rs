//! Cluster names, public RPC endpoints and genesis hashes.

use crate::constants::{DEVNET_GENESIS, MAINNET_GENESIS};
use crate::error::{Error, Result};

#[derive(Clone, PartialEq, Eq)]
pub enum Cluster {
    Mainnet,
    Devnet,
    Localnet,
    /// The full RPC URL (it is needed to reach the node). It may carry an API key, so it is never
    /// printed, logged or written to a file: use `name()`, which shows the host only.
    Custom(String),
}

/// Debug output (assert messages, panics) shows the host only, like `name()`.
impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cluster({})", self.name())
    }
}

impl Cluster {
    pub fn parse(s: &str) -> Result<Cluster> {
        match s {
            "mainnet" | "mainnet-beta" => Ok(Cluster::Mainnet),
            "devnet" => Ok(Cluster::Devnet),
            "localnet" | "localhost" => Ok(Cluster::Localnet),
            u if u.starts_with("http://") || u.starts_with("https://") => Ok(Cluster::Custom(u.to_string())),
            other => Err(Error(format!(
                "unknown cluster '{}' (use devnet, mainnet, localnet or an http(s) URL)",
                safe_echo(other)
            ))),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Cluster::Mainnet => "mainnet-beta".into(),
            Cluster::Devnet => "devnet".into(),
            Cluster::Localnet => "localnet".into(),
            Cluster::Custom(u) => format!("custom ({})", crate::rpc::host_of(u)),
        }
    }

    pub fn default_rpc_url(&self) -> String {
        match self {
            Cluster::Mainnet => "https://api.mainnet-beta.solana.com".into(),
            Cluster::Devnet => "https://api.devnet.solana.com".into(),
            Cluster::Localnet => "http://127.0.0.1:8899".into(),
            Cluster::Custom(u) => u.clone(),
        }
    }

    /// The genesis hash this cluster name stands for, when it is one of the public clusters.
    pub fn known_genesis(&self) -> Option<&'static str> {
        match self {
            Cluster::Mainnet => Some(MAINNET_GENESIS),
            Cluster::Devnet => Some(DEVNET_GENESIS),
            _ => None,
        }
    }
}

/// A user-typed value that is safe to repeat in an error message. A mistyped URL (a missing scheme,
/// say) carries its API key with it, so anything beyond a short plain word is not echoed.
pub fn safe_echo(s: &str) -> String {
    let plain =
        !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if plain {
        s.to_string()
    } else {
        "<value hidden: it may be a URL with a key in it>".to_string()
    }
}

/// A file path as it may be printed. A URL typed where a file was expected (`inspect https://...`)
/// is shown by host only.
pub fn safe_path(p: &std::path::Path) -> String {
    let t = p.display().to_string();
    if t.contains("://") {
        format!("<a URL on {}, not a file>", crate::rpc::host_of(&t))
    } else {
        t
    }
}

/// A `cluster` label read from a transaction file, made safe to print and to write back. Files written
/// by older versions say `custom (<full RPC URL>)`; those are cut down to the host. The field is
/// informational only: nothing is signed or allowlisted from it.
pub fn sanitize_label(label: &str) -> String {
    if let Some(inner) = label.strip_prefix("custom (").and_then(|r| r.strip_suffix(')')) {
        return format!("custom ({})", crate::rpc::host_of(inner));
    }
    let plain =
        label.len() <= 64 && label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '));
    if plain {
        label.to_string()
    } else {
        "<unrecognised cluster label>".to_string()
    }
}

/// Human label for a genesis hash found in a message.
pub fn label_for_genesis(genesis: &str) -> &'static str {
    match genesis {
        MAINNET_GENESIS => "mainnet-beta",
        DEVNET_GENESIS => "devnet",
        _ => "unrecognised cluster (not mainnet-beta, not devnet)",
    }
}

pub fn is_mainnet_genesis(genesis: &str) -> bool {
    genesis == MAINNET_GENESIS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_and_urls() {
        assert_eq!(Cluster::parse("devnet").unwrap(), Cluster::Devnet);
        assert_eq!(Cluster::parse("mainnet-beta").unwrap(), Cluster::Mainnet);
        assert_eq!(Cluster::parse("mainnet").unwrap(), Cluster::Mainnet);
        assert_eq!(Cluster::parse("http://x:1").unwrap(), Cluster::Custom("http://x:1".into()));
        assert!(Cluster::parse("testnet").is_err());
        assert!(!format!("{:?}", Cluster::parse("https://h.example.com/SECRETKEY?k=SECRETKEY").unwrap())
            .contains("SECRETKEY"));
        assert!(Cluster::Mainnet.known_genesis().unwrap() != Cluster::Devnet.known_genesis().unwrap());
        assert!(Cluster::Localnet.known_genesis().is_none());
    }

    #[test]
    fn labels_from_old_files_are_cut_down_to_the_host() {
        assert_eq!(sanitize_label("custom (https://u:k@rpc.example.com/v2/KEY?a=KEY)"), "custom (rpc.example.com)");
        assert_eq!(sanitize_label("custom (rpc.example.com)"), "custom (rpc.example.com)");
        assert_eq!(sanitize_label("custom (http://127.0.0.1:8899)"), "custom (127.0.0.1:8899)");
        assert_eq!(sanitize_label("devnet"), "devnet");
        assert_eq!(sanitize_label("mainnet-beta"), "mainnet-beta");
        assert_eq!(sanitize_label("https://x.example.com/KEY"), "<unrecognised cluster label>");
        assert_eq!(sanitize_label(&"a".repeat(65)), "<unrecognised cluster label>");
    }

    #[test]
    fn echoed_values_are_plain_words_or_hidden() {
        assert_eq!(safe_echo("testnet"), "testnet");
        assert_eq!(safe_echo(&"a".repeat(64)), "a".repeat(64));
        assert!(!safe_echo(&"a".repeat(65)).contains("aaaa"), "a long token may be a key");
        assert_eq!(safe_echo("rpc.example.com/KEY"), safe_echo("https://rpc.example.com/KEY"));
        assert!(!safe_echo("rpc.example.com/KEY").contains("KEY"));
        assert!(!safe_echo("").contains("KEY"));
        assert!(!safe_path(std::path::Path::new("https://rpc.example.com/KEY?k=KEY")).contains("KEY"));
        assert_eq!(safe_path(std::path::Path::new("dir/tx.json")), "dir/tx.json");
    }
}
