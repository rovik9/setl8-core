//! Alert conditions, as pure functions of what was read from the chain.

use anchor_lang::prelude::Pubkey;
use core_vault::constants::{OPEN_CLAIMS_CEILING, PAUSE_RECONCILIATION_DEFICIT};
use serde_json::{json, Value};

use crate::model::World;

/// Alert when the open claims are ABOVE this share of the ceiling (bond exits are refused at 100%).
pub const CEILING_ALERT_PERCENT: u64 = 80;
/// A cycle open for longer than this is stuck (24 h).
pub const CYCLE_OPEN_MAX_SECS: i64 = 24 * 3600;
/// No cycle begun for longer than this while claims are open (6 days; the gap is 5).
pub const NO_CYCLE_MAX_SECS: i64 = 6 * 24 * 3600;
/// A claim whose destination stayed unusable through this many cycles.
pub const SKIPPED_CYCLES: u64 = 3;
/// Default fee-payer balance floor: 0.05 SOL.
pub const DEFAULT_MIN_PAYER_BALANCE: u64 = 50_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub kind: &'static str,
    /// What the alert is about (a pool, a product, a claim); with `kind` it identifies a repeat.
    pub key: String,
    pub detail: Value,
}

pub struct Thresholds {
    pub min_payer_balance: u64,
}

/// Is `total` strictly above 80% of the ceiling? (Integer arithmetic: no rounding at the boundary.)
pub fn above_ceiling_threshold(total: u64) -> bool {
    (total as u128) * 100 > (OPEN_CLAIMS_CEILING as u128) * (CEILING_ALERT_PERCENT as u128)
}

/// `spendable / total` as a decimal with 6 places, ROUNDED DOWN (so 99,999,999 of 100,000,000 reads 0.999999, never 1.000000).
pub fn ratio_string(spendable: u128, total: u128) -> String {
    let ppm = spendable * 1_000_000 / total.max(1);
    format!("{}.{:06}", ppm / 1_000_000, ppm % 1_000_000)
}

pub fn evaluate(w: &World, th: &Thresholds, skipped_claims: &[Pubkey]) -> Vec<Alert> {
    let mut out = vec![];
    let v = &w.vault;

    for (name, pool) in [("usdc", w.usdc), ("usdt", w.usdt)] {
        if pool.frozen {
            out.push(Alert {
                kind: "pool_frozen",
                key: name.into(),
                detail: json!({"pool": name, "balance": pool.amount, "note": "the issuer froze this pool; the heartbeat counts it as empty (SR-03)"}),
            });
        }
    }

    for p in &w.products {
        if !p.active && p.pause_reason == PAUSE_RECONCILIATION_DEFICIT {
            out.push(Alert {
                kind: "product_auto_paused",
                key: p.id.to_string(),
                detail: json!({"product": p.id.to_string(), "reason_code": p.pause_reason, "reason": "reconciliation deficit: the sector's tally disagrees with the vault"}),
            });
        }
    }

    if v.cycle_active && w.now.saturating_sub(v.cycle_started_at) > CYCLE_OPEN_MAX_SECS {
        out.push(Alert {
            kind: "cycle_open_too_long",
            key: v.cycle_id.to_string(),
            detail: json!({"cycle_id": v.cycle_id, "open_secs": w.now - v.cycle_started_at, "processed": v.cycle_processed_count, "eligible": v.cycle_eligible_count}),
        });
    }

    if !v.cycle_active
        && v.open_claims_count > 0
        && v.cycle_started_at > 0
        && w.now.saturating_sub(v.cycle_started_at) > NO_CYCLE_MAX_SECS
    {
        out.push(Alert {
            kind: "no_cycle_for_too_long",
            key: "idle".into(),
            detail: json!({"secs_since_last_cycle_started": w.now - v.cycle_started_at, "open_claims": v.open_claims_count}),
        });
    }

    if above_ceiling_threshold(v.open_claims_total) {
        out.push(Alert {
            kind: "claims_near_ceiling",
            key: "ceiling".into(),
            detail: json!({
                "open_claims_total": v.open_claims_total,
                "ceiling": OPEN_CLAIMS_CEILING,
                "percent": (v.open_claims_total as u128 * 100 / OPEN_CLAIMS_CEILING as u128) as u64,
                "at_ceiling": v.open_claims_total >= OPEN_CLAIMS_CEILING,
                "note": "bond exits and new payout requests are refused at the ceiling",
            }),
        });
    }

    let spendable = w.usdc.spendable() as u128 + w.usdt.spendable() as u128;
    if v.open_claims_total > 0 && spendable < v.open_claims_total as u128 {
        out.push(Alert {
            kind: "coverage_below_one",
            key: "coverage".into(),
            detail: json!({
                "spendable_pools": spendable.to_string(),
                "open_claims_total": v.open_claims_total,
                "ratio": ratio_string(spendable, v.open_claims_total as u128),
            }),
        });
    }

    if w.payer_balance < th.min_payer_balance {
        out.push(Alert {
            kind: "payer_balance_low",
            key: "payer".into(),
            detail: json!({"balance_lamports": w.payer_balance, "threshold_lamports": th.min_payer_balance}),
        });
    }

    for c in skipped_claims {
        out.push(Alert {
            kind: "claim_skipped_repeatedly",
            key: c.to_string(),
            detail: json!({"claim": c.to_string(), "cycles": SKIPPED_CYCLES, "note": "the trader's associated token accounts are missing, frozen or re-owned; the claim stays owed"}),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PoolView, ProductView, VaultView};

    fn world() -> World {
        World {
            now: 10_000_000,
            vault: VaultView {
                usdc_mint: Pubkey::new_unique(),
                usdt_mint: Pubkey::new_unique(),
                usdc_pool: Pubkey::new_unique(),
                usdt_pool: Pubkey::new_unique(),
                open_claims_count: 2,
                open_claims_total: 1_000,
                cycle_id: 3,
                cycle_started_at: 9_950_000,
                cycle_active: false,
                cycle_owed_snapshot: 0,
                cycle_available_snapshot: 0,
                cycle_eligible_count: 0,
                cycle_processed_count: 0,
            },
            usdc: PoolView { amount: 5_000, frozen: false },
            usdt: PoolView { amount: 5_000, frozen: false },
            products: vec![],
            payer_balance: 1_000_000_000,
        }
    }

    fn th() -> Thresholds {
        Thresholds { min_payer_balance: DEFAULT_MIN_PAYER_BALANCE }
    }

    fn kinds(w: &World) -> Vec<&'static str> {
        evaluate(w, &th(), &[]).iter().map(|a| a.kind).collect()
    }

    #[test]
    fn a_healthy_vault_raises_nothing() {
        assert!(kinds(&world()).is_empty());
    }

    #[test]
    fn frozen_pools() {
        let mut w = world();
        w.usdc.frozen = true;
        let a = evaluate(&w, &th(), &[]);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].kind, a[0].key.as_str()), ("pool_frozen", "usdc"));
        w.usdt.frozen = true;
        assert_eq!(evaluate(&w, &th(), &[]).iter().filter(|a| a.kind == "pool_frozen").count(), 2);
    }

    #[test]
    fn auto_paused_products_only_for_the_reconciliation_reason() {
        let mut w = world();
        let p = |active, reason| ProductView {
            id: Pubkey::new_unique(),
            registry: Pubkey::new_unique(),
            active,
            pause_reason: reason,
        };
        w.products = vec![p(true, 0), p(false, 1), p(false, 2)];
        let a = evaluate(&w, &th(), &[]);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "product_auto_paused");
        assert_eq!(a[0].detail["reason_code"], 2);
    }

    #[test]
    fn cycle_open_boundary_is_strictly_more_than_24_hours() {
        let mut w = world();
        w.vault.cycle_active = true;
        w.vault.cycle_started_at = w.now - CYCLE_OPEN_MAX_SECS;
        assert!(kinds(&w).is_empty(), "exactly 24 h is not yet an alert");
        w.vault.cycle_started_at -= 1;
        assert_eq!(kinds(&w), vec!["cycle_open_too_long"]);
        w.vault.cycle_active = false;
        w.vault.open_claims_count = 0;
        assert!(kinds(&w).is_empty());
    }

    #[test]
    fn no_cycle_boundary_is_strictly_more_than_6_days_with_claims_open() {
        let mut w = world();
        w.vault.cycle_started_at = w.now - NO_CYCLE_MAX_SECS;
        assert!(kinds(&w).is_empty());
        w.vault.cycle_started_at -= 1;
        assert_eq!(kinds(&w), vec!["no_cycle_for_too_long"]);
        w.vault.open_claims_count = 0;
        assert!(kinds(&w).is_empty(), "nothing owed, nothing to alert about");
        w.vault.open_claims_count = 1;
        w.vault.cycle_started_at = 0;
        assert!(kinds(&w).is_empty(), "never started: no reference time");
        w.vault.cycle_started_at = w.now - NO_CYCLE_MAX_SECS - 5;
        w.vault.cycle_active = true;
        assert!(!kinds(&w).contains(&"no_cycle_for_too_long"), "an open cycle is the other alert");
    }

    #[test]
    fn the_ceiling_alert_is_strictly_above_80_percent() {
        let mut w = world();
        w.usdc.amount = u64::MAX / 4;
        w.usdt.amount = u64::MAX / 4;
        let eighty = OPEN_CLAIMS_CEILING / 5 * 4;
        assert_eq!(eighty, 2_000_000_000_000);
        w.vault.open_claims_total = eighty - 1;
        assert!(kinds(&w).is_empty());
        w.vault.open_claims_total = eighty;
        assert!(kinds(&w).is_empty(), "exactly 80% is not above 80%");
        w.vault.open_claims_total = eighty + 1;
        assert_eq!(kinds(&w), vec!["claims_near_ceiling"]);
        w.vault.open_claims_total = OPEN_CLAIMS_CEILING;
        let a = evaluate(&w, &th(), &[]);
        assert_eq!(a[0].detail["at_ceiling"], true);
        assert_eq!(a[0].detail["percent"], 100);
        assert!(above_ceiling_threshold(eighty + 1) && !above_ceiling_threshold(eighty));
    }

    #[test]
    fn coverage_below_one_counts_only_spendable_pools() {
        let mut w = world();
        w.vault.open_claims_total = 10_000;
        assert!(kinds(&w).is_empty(), "pools exactly cover the claims");
        w.vault.open_claims_total = 10_001;
        let a = evaluate(&w, &th(), &[]);
        assert_eq!(a[0].kind, "coverage_below_one");
        assert_eq!(a[0].detail["ratio"], "0.999900");
        w.vault.open_claims_total = 6_000;
        w.usdt.frozen = true; // 5,000 spendable < 6,000 owed
        assert!(kinds(&w).contains(&"coverage_below_one"));
        w.vault.open_claims_total = 0;
        w.vault.open_claims_count = 0;
        w.usdc.amount = 0;
        assert!(!kinds(&w).contains(&"coverage_below_one"), "nothing owed");
    }

    #[test]
    fn the_ratio_is_floored_not_rounded() {
        assert_eq!(ratio_string(99_999_999, 100_000_000), "0.999999");
        assert_eq!(ratio_string(10_000, 10_001), "0.999900");
        assert_eq!(ratio_string(1, 3), "0.333333");
        assert_eq!(ratio_string(0, 5), "0.000000");
        assert_eq!(ratio_string(7, 7), "1.000000");
    }

    #[test]
    fn payer_balance_boundary() {
        let mut w = world();
        w.payer_balance = DEFAULT_MIN_PAYER_BALANCE;
        assert!(kinds(&w).is_empty());
        w.payer_balance -= 1;
        assert_eq!(kinds(&w), vec!["payer_balance_low"]);
    }

    #[test]
    fn repeatedly_skipped_claims() {
        let w = world();
        let c = Pubkey::new_unique();
        let a = evaluate(&w, &th(), &[c]);
        assert_eq!((a[0].kind, a[0].key.clone()), ("claim_skipped_repeatedly", c.to_string()));
    }
}
