//! PDA seeds. Every account this program owns is derived from one of these.

/// Seed for the `ProductRegistry` PDA: `[PRODUCT_REGISTRY_SEED,
/// product_program_id.as_ref()]`, one registry per registered sector
/// program.
pub const PRODUCT_REGISTRY_SEED: &[u8] = b"product_registry";

/// Seed for the `TraderState` PDA: `[TRADER_STATE_SEED,
/// product_program_id, trader_wallet, challenge_id.to_le_bytes()]`. One
/// record per wallet + product + challenge; a new purchase is always a new
/// record, never a reuse of an old one.
pub const TRADER_STATE_SEED: &[u8] = b"trader_state";

/// Seed for the singleton `VaultState` PDA: `[VAULT_STATE_SEED,
/// SL8_ADMIN_PUBKEY, ROV_ADMIN_PUBKEY]`.
pub const VAULT_STATE_SEED: &[u8] = b"vault_state";

/// Seed for the two payout-pool token accounts: `[POOL_SEED, vault_state,
/// mint]`. Their token authority is the `VaultState` PDA.
pub const POOL_SEED: &[u8] = b"pool";

/// Seed for a `PayoutClaim` PDA: `[PAYOUT_CLAIM_SEED, trader_state_key,
/// request_id.to_le_bytes()]`. One claim per accepted `request_payout`; the
/// request id is unique per challenge, so a claim address is never reused.
pub const PAYOUT_CLAIM_SEED: &[u8] = b"payout_claim";

/// Seed for a `BondPosition` PDA: `[BOND_SEED, depositor, deposit_index.to_le_bytes()]`.
/// The index comes from the depositor's `BondCapTracker` and is never reused.
pub const BOND_SEED: &[u8] = b"bond";

/// Seed for a depositor's `BondCapTracker` PDA: `[BOND_CAP_SEED, depositor]`.
pub const BOND_CAP_SEED: &[u8] = b"bond_cap";

/// Seed for the `PayoutClaim` a bond withdrawal creates (claim kind 1):
/// `[BOND_CLAIM_SEED, depositor, deposit_index.to_le_bytes()]`. A position is
/// closed by the request, so its index (and this address) is used exactly once.
pub const BOND_CLAIM_SEED: &[u8] = b"bond_claim";

#[cfg(test)]
mod tests {
    use super::*;

    /// A PDA is derived from the CONCATENATION of its seeds (no separators). Two PDA
    /// types whose seeds have the same total length could in principle alias each
    /// other, and a variable-length part would blur the boundary inside one type. Every
    /// part after the literal is fixed width (a `Pubkey` is 32 bytes, a `u64` 8), so the
    /// total length identifies the type. This fails if a new or renamed seed makes two
    /// totals equal. (docs/SECURITY-REVIEW.md, class 3.)
    #[test]
    fn pda_seed_lengths_are_pairwise_distinct() {
        const KEY: usize = 32;
        const U64: usize = 8;
        let totals: [(&str, usize); 8] = [
            ("product_registry", PRODUCT_REGISTRY_SEED.len() + KEY),
            ("trader_state", TRADER_STATE_SEED.len() + KEY + KEY + U64),
            ("vault_state", VAULT_STATE_SEED.len() + KEY + KEY),
            ("pool", POOL_SEED.len() + KEY + KEY),
            ("payout_claim", PAYOUT_CLAIM_SEED.len() + KEY + U64),
            ("bond", BOND_SEED.len() + KEY + U64),
            ("bond_cap", BOND_CAP_SEED.len() + KEY),
            ("bond_claim", BOND_CLAIM_SEED.len() + KEY + U64),
        ];
        for (i, (a, la)) in totals.iter().enumerate() {
            for (b, lb) in totals.iter().skip(i + 1) {
                assert_ne!(la, lb, "{a} and {b} PDAs would have the same total seed length ({la})");
            }
        }
        // the figures quoted in docs/SECURITY-REVIEW.md
        let lens: Vec<usize> = totals.iter().map(|t| t.1).collect();
        assert_eq!(lens, vec![48, 84, 75, 68, 52, 44, 40, 50]);
    }

    /// Solana limits one seed to 32 bytes; the literals are far below it.
    #[test]
    fn every_literal_seed_fits_the_per_seed_limit() {
        for s in [
            PRODUCT_REGISTRY_SEED, TRADER_STATE_SEED, VAULT_STATE_SEED, POOL_SEED, PAYOUT_CLAIM_SEED, BOND_SEED,
            BOND_CAP_SEED, BOND_CLAIM_SEED,
        ] {
            assert!(s.len() <= 32);
        }
    }
}
