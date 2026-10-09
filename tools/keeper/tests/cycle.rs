//! A full payout cycle driven only by the keeper, against the real program in LiteSVM.
#![cfg(feature = "localnet")]
mod common;
use common::*;
use core_vault::constants::MAX_SETTLE_BATCH;

/// 14 trader claims (USDC and USDT buyers), one bond claim, one wallet with a missing ATA and one with a frozen ATA.
#[test]
fn the_keeper_runs_a_whole_cycle_alone() {
    let mut e = Env::new();
    let (usdc, usdt) = (e.usdc, e.usdt);
    let mut claims = vec![];
    for i in 0..14u64 {
        let mint = if i % 2 == 0 { usdc } else { usdt };
        let (t, c) = e.queue_claim(&mint, (10 + i) * M);
        claims.push((t, c, (10 + i) * M));
    }
    let (bt, bond_claim) = e.queue_bond_claim(&usdc);
    let bond_owed = e.claim(&bond_claim).unwrap().owed;
    // one wallet loses its USDC account, one has its USDT account frozen: both claims are skipped
    let missing = claims[3].0;
    let frozen = claims[8].0;
    e.delete_account(&ata(&e.wallet(missing), &usdc));
    e.freeze_ata(&e.wallet(frozen), &usdt);
    e.fill_pool(&usdc, 2_000 * M);
    e.fill_pool(&usdt, 1_500 * M);
    e.advance(GAP); // the gap since the bond clock jump is long over: a first cycle may begin at once anyway
    let count = e.vault_state().open_claims_count;
    assert_eq!(count, 15);

    let before: Vec<(u64, u64)> = (0..e.traders.len())
        .map(|t| (e.balance(&ata(&e.wallet(t), &usdc)), e.balance(&ata(&e.wallet(t), &usdt))))
        .collect();
    let mut k = e.keeper();
    let reports = e.drive(&mut k, 5);
    let first = &reports[0];
    assert!(first.hard_failure.is_none(), "{:?}", first.hard_failure);
    let labels: Vec<&str> = first.sent.iter().map(|s| s.label.as_str()).collect();
    // reconcile for the one product, then begin, then 15 claims in batches of at most 6 (6+6+3), then finalize
    assert!(labels[0].starts_with("reconcile_product"), "{labels:?}");
    assert_eq!(labels[1], "begin_heartbeat");
    assert_eq!(labels.iter().filter(|l| l.starts_with("settle_claims")).count(), 3, "{labels:?}");
    assert_eq!(*labels.last().unwrap(), "finalize_heartbeat");
    assert!(labels.iter().filter(|l| l.starts_with("settle_claims x")).all(|l| l.ends_with("x6") || l.ends_with("x3")));
    assert_eq!(MAX_SETTLE_BATCH, 6);

    let vs = e.vault_state();
    assert!(!vs.cycle_active, "the cycle is closed");
    assert_eq!(vs.cycle_id, 1);
    assert_eq!(vs.cycle_processed_count, vs.cycle_eligible_count);
    assert_eq!(vs.cycle_eligible_count, 15);

    // every payment equals the program's own plan; the ratio is 1 (the pools cover the claims)
    let after: Vec<(u64, u64)> = (0..e.traders.len())
        .map(|t| (e.balance(&ata(&e.wallet(t), &usdc)), e.balance(&ata(&e.wallet(t), &usdt))))
        .collect();
    let mut paid_total = 0u64;
    for (t, c, owed) in &claims {
        let got = (after[*t].0 - before[*t].0) + (after[*t].1 - before[*t].1);
        if *t == missing || *t == frozen {
            assert_eq!(got, 0, "a skipped claim is not paid");
            assert_eq!(e.claim(c).unwrap().owed, *owed, "and stays owed");
            assert_eq!(e.claim(c).unwrap().last_settled_cycle, 1, "but counts as processed");
        } else {
            assert_eq!(got, *owed, "paid in full");
            assert!(e.claim(c).is_none(), "and closed");
            paid_total += owed;
        }
    }
    let bond_got = (after[bt].0 - before[bt].0) + (after[bt].1 - before[bt].1);
    assert_eq!(bond_got, bond_owed);
    assert!(e.claim(&bond_claim).is_none(), "the bond claim is paid and closed");
    paid_total += bond_owed;

    let skipped_owed = claims[3].2 + claims[8].2;
    assert_eq!(vs.open_claims_count, 2);
    assert_eq!(vs.open_claims_total, skipped_owed);
    assert_eq!(e.open_claims().len(), 2);
    let pools = e.balance(&e.pool(&usdc)) + e.balance(&e.pool(&usdt));
    assert_eq!(pools, 3_500 * M - paid_total, "the pools lost exactly what was paid (they were set to 3,500 in total)");
}
