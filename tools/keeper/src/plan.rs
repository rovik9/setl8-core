//! The decisions, as pure functions: what is eligible, how it is batched, when a cycle may begin.

use anchor_lang::prelude::Pubkey;
use core_vault::constants::{HEARTBEAT_MIN_GAP_SECS, MAX_SETTLE_BATCH};

use crate::model::{ClaimView, ProductView, VaultView};

/// What to do when no cycle is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// A cycle is open: continue it, do not begin.
    CycleActive,
    /// Nothing owed: do not burn a cycle slot on an empty cycle (SR-08).
    NoClaims,
    /// The 432,000 s gap since the last cycle started has not passed yet.
    Wait { seconds_left: i64 },
    /// Reconcile every product, then begin.
    Begin,
}

/// The earliest second `begin_heartbeat` is accepted (`now >= cycle_started_at + gap`; a first cycle may start at once).
pub fn earliest_begin(v: &VaultView) -> i64 {
    if v.cycle_started_at == 0 {
        0
    } else {
        v.cycle_started_at.saturating_add(HEARTBEAT_MIN_GAP_SECS)
    }
}

pub fn schedule(v: &VaultView, now: i64) -> Schedule {
    if v.cycle_active {
        return Schedule::CycleActive;
    }
    if v.open_claims_count == 0 {
        return Schedule::NoClaims;
    }
    let earliest = earliest_begin(v);
    if now < earliest {
        return Schedule::Wait { seconds_left: earliest - now };
    }
    Schedule::Begin
}

/// The claims `settle_claims` will accept in the current cycle: created before it began and not yet
/// processed in it (`created_in_cycle < cycle_id && last_settled_cycle != cycle_id`, read from the program).
/// Sorted by address so batches are stable.
pub fn eligible_unprocessed(v: &VaultView, claims: &[ClaimView]) -> Vec<ClaimView> {
    if !v.cycle_active {
        return vec![];
    }
    let mut out: Vec<ClaimView> = claims
        .iter()
        .filter(|c| c.created_in_cycle < v.cycle_id && c.last_settled_cycle != v.cycle_id)
        .cloned()
        .collect();
    out.sort_by_key(|c| c.address.to_bytes());
    out
}

/// Chunks of at most the program's batch limit, in the given order.
pub fn batches(claims: &[ClaimView]) -> Vec<Vec<ClaimView>> {
    claims.chunks(MAX_SETTLE_BATCH).map(|c| c.to_vec()).collect()
}

pub fn associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), anchor_spl::token::ID.as_ref(), mint.as_ref()],
        &core_vault::constants::ATA_PROGRAM_ID,
    )
    .0
}

/// `[claim, trader_usdc_ata, trader_usdt_ata]`: the exact accounts `settle_claims` demands.
pub fn triple(c: &ClaimView, usdc_mint: &Pubkey, usdt_mint: &Pubkey) -> [Pubkey; 3] {
    [
        c.address,
        associated_token_address(&c.trader_wallet, usdc_mint),
        associated_token_address(&c.trader_wallet, usdt_mint),
    ]
}

/// Active products get a reconcile before a cycle begins; a paused one would only fail with `ProductAlreadyPaused`.
pub fn products_to_reconcile(products: &[ProductView]) -> (Vec<ProductView>, Vec<ProductView>) {
    let mut send: Vec<ProductView> = products.iter().filter(|p| p.active).cloned().collect();
    let mut skip: Vec<ProductView> = products.iter().filter(|p| !p.active).cloned().collect();
    send.sort_by_key(|p| p.id.to_bytes());
    skip.sort_by_key(|p| p.id.to_bytes());
    (send, skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> VaultView {
        VaultView {
            usdc_mint: Pubkey::new_unique(),
            usdt_mint: Pubkey::new_unique(),
            usdc_pool: Pubkey::new_unique(),
            usdt_pool: Pubkey::new_unique(),
            open_claims_count: 3,
            open_claims_total: 300,
            cycle_id: 4,
            cycle_started_at: 1_000_000,
            cycle_active: false,
            cycle_owed_snapshot: 0,
            cycle_available_snapshot: 0,
            cycle_eligible_count: 0,
            cycle_processed_count: 0,
        }
    }

    fn claim(created: u64, settled: u64) -> ClaimView {
        ClaimView {
            address: Pubkey::new_unique(),
            trader_wallet: Pubkey::new_unique(),
            owed: 10,
            created_in_cycle: created,
            last_settled_cycle: settled,
            kind: 0,
        }
    }

    #[test]
    fn the_gap_boundary_to_the_second() {
        let v = vault();
        let edge = v.cycle_started_at + 432_000;
        assert_eq!(HEARTBEAT_MIN_GAP_SECS, 432_000);
        assert_eq!(schedule(&v, edge - 1), Schedule::Wait { seconds_left: 1 });
        assert_eq!(schedule(&v, edge), Schedule::Begin);
        assert_eq!(schedule(&v, edge + 1), Schedule::Begin);
        assert_eq!(schedule(&v, 0), Schedule::Wait { seconds_left: edge });
        assert_eq!(earliest_begin(&v), edge);
    }

    #[test]
    fn the_schedule_table() {
        let mut v = vault();
        // (cycle_active, claims, started_at, now) -> decision
        let t = v.cycle_started_at + 432_000;
        let cases = [
            (true, 5, 1_000_000, t + 99, Schedule::CycleActive),
            (true, 0, 1_000_000, 0, Schedule::CycleActive),
            (false, 0, 1_000_000, t + 1_000_000, Schedule::NoClaims), // never an empty cycle
            (false, 0, 1_000_000, 0, Schedule::NoClaims),
            (false, 1, 1_000_000, t - 1, Schedule::Wait { seconds_left: 1 }),
            (false, 1, 1_000_000, t, Schedule::Begin),
            (false, 1, 0, 5, Schedule::Begin), // the very first cycle may start at once
            (false, u64::MAX, 1_000_000, t, Schedule::Begin),
        ];
        for (active, claims, started, now, want) in cases {
            v.cycle_active = active;
            v.open_claims_count = claims;
            v.cycle_started_at = started;
            assert_eq!(schedule(&v, now), want, "active={active} claims={claims} started={started} now={now}");
        }
    }

    #[test]
    fn eligibility_is_created_before_and_not_processed_this_cycle() {
        let mut v = vault();
        v.cycle_active = true;
        v.cycle_id = 4;
        let before = claim(3, 0); // created in an earlier cycle, never processed
        let earlier_processed = claim(1, 3); // processed in cycle 3, not 4
        let same_cycle = claim(4, 0); // created during this cycle: not eligible
        let later = claim(5, 0); // cannot exist, but must not be eligible
        let done = claim(2, 4); // already processed in this cycle (e.g. skipped or partly paid)
        let all = vec![same_cycle.clone(), done.clone(), before.clone(), later.clone(), earlier_processed.clone()];
        let got = eligible_unprocessed(&v, &all);
        let mut want = vec![before.address, earlier_processed.address];
        want.sort_by_key(|a| a.to_bytes());
        assert_eq!(got.iter().map(|c| c.address).collect::<Vec<_>>(), want);
        v.cycle_active = false;
        assert!(eligible_unprocessed(&v, &all).is_empty(), "no open cycle, nothing is settleable");
    }

    #[test]
    fn batches_are_at_most_six_in_a_stable_order() {
        let mut v = vault();
        v.cycle_active = true;
        let claims: Vec<ClaimView> = (0..14).map(|_| claim(1, 0)).collect();
        let e1 = eligible_unprocessed(&v, &claims);
        let mut shuffled = claims.clone();
        shuffled.reverse();
        let e2 = eligible_unprocessed(&v, &shuffled);
        assert_eq!(e1, e2, "input order does not matter");
        let b = batches(&e1);
        assert_eq!(b.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![6, 6, 2]);
        assert_eq!(MAX_SETTLE_BATCH, 6);
        let flat: Vec<_> = b.concat();
        assert_eq!(flat, e1);
        assert!(batches(&[]).is_empty());
        assert_eq!(batches(&e1[..6]).len(), 1);
        assert_eq!(batches(&e1[..7]).len(), 2);
    }

    #[test]
    fn triples_use_the_traders_associated_accounts() {
        let v = vault();
        let c = claim(1, 0);
        let [claim_addr, usdc, usdt] = triple(&c, &v.usdc_mint, &v.usdt_mint);
        assert_eq!(claim_addr, c.address);
        assert_eq!(usdc, associated_token_address(&c.trader_wallet, &v.usdc_mint));
        assert_eq!(usdt, associated_token_address(&c.trader_wallet, &v.usdt_mint));
        assert_ne!(usdc, usdt);
        // the same derivation the program uses
        assert_eq!(usdc, core_vault::utils::associated_token_address(&c.trader_wallet, &v.usdc_mint));
    }

    #[test]
    fn only_active_products_are_reconciled() {
        let p = |active| ProductView {
            id: Pubkey::new_unique(),
            registry: Pubkey::new_unique(),
            active,
            pause_reason: if active { 0 } else { 2 },
        };
        let all = vec![p(true), p(false), p(true)];
        let (send, skip) = products_to_reconcile(&all);
        assert_eq!((send.len(), skip.len()), (2, 1));
        assert!(send.iter().all(|x| x.active) && skip.iter().all(|x| !x.active));
    }
}
