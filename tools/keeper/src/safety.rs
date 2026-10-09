//! Refusals that run before the keeper sends anything.

use anchor_lang::prelude::{pubkey, Pubkey};
use setl8_admin::admin_ix::Keys;
use setl8_admin::cluster::Cluster;
use setl8_admin::constants::{DEVNET_GENESIS, MAINNET_GENESIS};
use setl8_admin::error::{Error, Result};

/// A cluster name that is safe to log: a custom cluster is shown by host only, because its URL can carry an API key.
pub fn cluster_label(c: &Cluster) -> String {
    match c {
        Cluster::Custom(u) => format!("custom ({})", setl8_admin::rpc::host_of(u)),
        other => other.name(),
    }
}

/// Every admin public key this repository has ever embedded: the real pair and the public test pair.
/// A hot keeper key must never be one of them, whichever build is running.
pub const ADMIN_PUBKEYS: [(&str, Pubkey); 4] = [
    ("SL8 admin (real)", pubkey!("SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ")),
    ("ROV admin (real)", pubkey!("RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ")),
    ("SL8 admin (public test key)", pubkey!("9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP")),
    ("ROV admin (public test key)", pubkey!("D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6")),
];

/// The keeper holds no authority and must not be given any: refuse an admin key as the fee payer.
pub fn refuse_admin_key(pk: &Pubkey, keys: &Keys) -> Result<()> {
    for (name, admin) in ADMIN_PUBKEYS
        .iter()
        .map(|(n, k)| (*n, *k))
        .chain([("the SL8 admin key of this build", keys.sl8), ("the ROV admin key of this build", keys.rov)])
    {
        if *pk == admin {
            return Err(Error(format!(
                "refused: the keeper key {pk} is {name}. A keeper runs hot and needs no authority: use a fresh key that only pays fees."
            )));
        }
    }
    Ok(())
}

/// The cluster named on the command line must be the one the node serves.
pub fn check_genesis(cluster: &Cluster, live: &str) -> Result<()> {
    match cluster {
        Cluster::Mainnet if live != MAINNET_GENESIS => {
            Err(Error(format!("refused: --cluster mainnet but the node's genesis hash is {live}")))
        }
        Cluster::Devnet if live != DEVNET_GENESIS => {
            Err(Error(format!("refused: --cluster devnet but the node's genesis hash is {live}")))
        }
        Cluster::Localnet | Cluster::Custom(_) if live == MAINNET_GENESIS || live == DEVNET_GENESIS => {
            Err(Error(format!(
                "refused: --cluster {} but the node is on a public cluster (genesis {live})",
                cluster_label(cluster)
            )))
        }
        _ => Ok(()),
    }
}

/// Mainnet needs the explicit flag, every time.
pub fn check_mainnet_gate(cluster: &Cluster, flag: bool) -> Result<()> {
    if *cluster == Cluster::Mainnet && !flag {
        return Err(Error("refused: --cluster mainnet needs --i-understand-this-is-mainnet".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_admin_key_is_refused() {
        let keys = Keys::compiled();
        for (name, k) in ADMIN_PUBKEYS {
            let e = refuse_admin_key(&k, &keys).unwrap_err();
            assert!(e.0.starts_with("refused:") && e.0.contains(name), "{e}");
        }
        assert!(refuse_admin_key(&keys.sl8, &keys).is_err() && refuse_admin_key(&keys.rov, &keys).is_err());
        assert!(refuse_admin_key(&Pubkey::new_unique(), &keys).is_ok());
    }

    #[test]
    fn the_hardcoded_pairs_match_the_two_builds() {
        let k = Keys::compiled();
        assert!(ADMIN_PUBKEYS.iter().any(|(_, p)| *p == k.sl8) && ADMIN_PUBKEYS.iter().any(|(_, p)| *p == k.rov));
        assert_eq!(ADMIN_PUBKEYS[0].1.to_string(), "SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ");
    }

    #[test]
    fn genesis_checks() {
        assert!(check_genesis(&Cluster::Devnet, DEVNET_GENESIS).is_ok());
        assert!(check_genesis(&Cluster::Devnet, MAINNET_GENESIS).is_err());
        assert!(check_genesis(&Cluster::Devnet, "other").is_err());
        assert!(check_genesis(&Cluster::Mainnet, MAINNET_GENESIS).is_ok());
        assert!(check_genesis(&Cluster::Mainnet, DEVNET_GENESIS).is_err());
        assert!(check_genesis(&Cluster::Localnet, "LocalGenesis").is_ok());
        assert!(check_genesis(&Cluster::Localnet, DEVNET_GENESIS).is_err());
        assert!(check_genesis(&Cluster::Localnet, MAINNET_GENESIS).is_err());
    }

    #[test]
    fn custom_clusters_are_labelled_by_host_only() {
        assert_eq!(
            cluster_label(&Cluster::Custom("https://node.example.test:8899/?api-key=SECRET".into())),
            "custom (node.example.test:8899)"
        );
        assert_eq!(cluster_label(&Cluster::Devnet), "devnet");
        assert!(check_genesis(&Cluster::Custom("https://x.test/?k=SECRET".into()), DEVNET_GENESIS)
            .unwrap_err()
            .0
            .find("SECRET")
            .is_none());
    }

    #[test]
    fn mainnet_needs_the_flag() {
        assert!(check_mainnet_gate(&Cluster::Mainnet, false).is_err());
        assert!(check_mainnet_gate(&Cluster::Mainnet, true).is_ok());
        assert!(check_mainnet_gate(&Cluster::Devnet, false).is_ok());
    }
}
