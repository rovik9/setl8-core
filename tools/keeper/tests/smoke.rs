//! Harness smoke test: the world builds, claims can be queued, a keeper pass runs.
#![cfg(feature = "localnet")]
mod common;
use common::*;

#[test]
fn the_world_builds_and_a_claim_can_be_queued() {
    let mut e = Env::new();
    e.fill_pool(&e.usdc.clone(), 500 * M);
    let (t, claim) = e.queue_claim(&e.usdc.clone(), 30 * M);
    assert_eq!(e.claim(&claim).unwrap().owed, 30 * M);
    assert_eq!(e.vault_state().open_claims_count, 1);
    assert_eq!(e.claim(&claim).unwrap().trader_wallet, e.wallet(t));
    let k = e.keeper();
    let w = k.read_world().unwrap();
    assert_eq!(w.vault.open_claims_count, 1);
    assert_eq!(w.products.len(), 1);
    assert!(w.products[0].active);
    assert_eq!(k.load_claims().unwrap().len(), 1);
}
