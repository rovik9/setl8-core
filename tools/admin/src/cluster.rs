//! Cluster names, public RPC endpoints and genesis hashes.

use crate::constants::{DEVNET_GENESIS, MAINNET_GENESIS};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cluster {
    Mainnet,
    Devnet,
    Localnet,
    Custom(String),
}

impl Cluster {
    pub fn parse(s: &str) -> Result<Cluster> {
        match s {
            "mainnet" | "mainnet-beta" => Ok(Cluster::Mainnet),
            "devnet" => Ok(Cluster::Devnet),
            "localnet" | "localhost" => Ok(Cluster::Localnet),
            u if u.starts_with("http://") || u.starts_with("https://") => Ok(Cluster::Custom(u.to_string())),
            other => Err(Error(format!("unknown cluster '{other}' (use devnet, mainnet, localnet or an http(s) URL)"))),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Cluster::Mainnet => "mainnet-beta".into(),
            Cluster::Devnet => "devnet".into(),
            Cluster::Localnet => "localnet".into(),
            Cluster::Custom(u) => format!("custom ({u})"),
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
        assert!(Cluster::Mainnet.known_genesis().unwrap() != Cluster::Devnet.known_genesis().unwrap());
        assert!(Cluster::Localnet.known_genesis().is_none());
    }
}
