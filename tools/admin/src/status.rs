//! `status`: read-only view of the vault and its products. Signs nothing.

use anchor_lang::{AccountDeserialize, Discriminator};
use anchor_spl::token::spl_token::{
    solana_program::program_pack::Pack,
    state::{Account as TokAcc, AccountState},
};
use core_vault::constants::{
    BOND_GLOBAL_CAP, OPEN_CLAIMS_CEILING, PAUSE_NONE, PAUSE_PLANNED_UPGRADE, PAUSE_RECONCILIATION_DEFICIT,
};
use core_vault::state::{ProductRegistry, VaultState};

use crate::admin_ix::{Keys, Side};
use crate::cluster::label_for_genesis;
use crate::error::{Error, Result};
use crate::fmt::{fmt_amount, group};
use crate::rpc::Rpc;

/// Unix seconds to `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days, no time crate needed).
pub fn fmt_unix(t: i64) -> String {
    if t <= 0 {
        return "never".into();
    }
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn pause_name(r: u8) -> &'static str {
    match r {
        PAUSE_NONE => "not paused",
        PAUSE_PLANNED_UPGRADE => "paused: planned upgrade",
        PAUSE_RECONCILIATION_DEFICIT => "paused: RECONCILIATION DEFICIT",
        _ => "paused: unknown reason",
    }
}

pub fn status(rpc: &dyn Rpc, keys: &Keys) -> Result<String> {
    let mut o = String::new();
    let g = rpc.genesis_hash()?;
    o.push_str(&format!("Cluster ........ {} (genesis {g})\n", label_for_genesis(&g)));
    o.push_str(&format!("Program ........ {}\n", keys.program_id));
    o.push_str(&format!(
        "Admin keys ..... SL8 {}  ROV {}  ({} keys compiled into this build)\n",
        keys.sl8,
        keys.rov,
        if cfg!(feature = "localnet") { "PUBLIC TEST" } else { "REAL" }
    ));
    o.push_str(&format!("Vault PDA ...... {}\n", keys.vault()));
    let Some(acc) = rpc.account(&keys.vault())? else {
        o.push_str("\nThe vault does not exist on this cluster yet (init_vault has not run).\n");
        return Ok(o);
    };
    if acc.owner != keys.program_id {
        return Err(Error(format!("the vault account is owned by {}, not by the vault program", acc.owner)));
    }
    let mut d = acc.data.as_slice();
    let vs = VaultState::try_deserialize(&mut d)
        .map_err(|_| Error("the vault account does not decode as VaultState".into()))?;
    o.push_str(&format!(
        "SL8 wallet ..... {}{}\n",
        vs.sl8_wallet,
        if vs.sl8_wallet == keys.sl8 { "  (the SL8 admin key)" } else { "  <<<< NOT this build's SL8 admin key" }
    ));

    o.push_str("\nPools (admin_withdraw_marketing_funds keeps max(stored floor, ceil(25% of live)) in each pool):\n");
    for side in [Side::Usdc, Side::Usdt] {
        let (mint, pool_addr, floor, withdrawn) = match side {
            Side::Usdc => (vs.usdc_mint, vs.usdc_pool, vs.usdc_floor, vs.marketing_withdrawn_usdc),
            Side::Usdt => (vs.usdt_mint, vs.usdt_pool, vs.usdt_floor, vs.marketing_withdrawn_usdt),
        };
        o.push_str(&format!("  {} mint {mint}  pool {pool_addr}\n", side.name()));
        match rpc.account(&pool_addr)?.and_then(|a| TokAcc::unpack(&a.data).ok()) {
            None => o.push_str("    POOL ACCOUNT MISSING OR NOT A TOKEN ACCOUNT\n"),
            Some(t) => {
                let reserve = core_vault::utils::reserve(t.amount, floor).unwrap_or(0);
                let max = core_vault::utils::withdrawable(t.amount, floor).unwrap_or(0);
                o.push_str(&format!(
                    "    balance {}   frozen: {}\n    stored floor {}   reserve {}   admin_withdraw could take NOW: {}\n    withdrawn so far: {}\n",
                    fmt_amount(t.amount),
                    if t.state == AccountState::Frozen { "YES (the heartbeat treats it as empty)" } else { "no" },
                    fmt_amount(floor),
                    fmt_amount(reserve),
                    fmt_amount(max),
                    fmt_amount(withdrawn)
                ));
            }
        }
        let ata = keys.sl8_ata(&mint);
        match rpc.account(&ata)?.and_then(|a| TokAcc::unpack(&a.data).ok()) {
            None => o.push_str(&format!(
                "    SL8 token account {ata}: MISSING (deposits and withdrawals to SL8 fail until it exists)\n"
            )),
            Some(t) => o.push_str(&format!(
                "    SL8 token account {ata}: balance {}{}\n",
                fmt_amount(t.amount),
                if t.state == AccountState::Frozen { "  FROZEN" } else { "" }
            )),
        }
    }

    let headroom = OPEN_CLAIMS_CEILING.saturating_sub(vs.open_claims_total);
    o.push_str(&format!(
        "\nOpen claims ...... {} claims owing {}\nClaims ceiling ... {}   headroom {} ({}% used)\n",
        group(vs.open_claims_count),
        fmt_amount(vs.open_claims_total),
        fmt_amount(OPEN_CLAIMS_CEILING),
        fmt_amount(headroom),
        (vs.open_claims_total as u128 * 100 / OPEN_CLAIMS_CEILING as u128)
    ));
    o.push_str(&format!(
        "\nHeartbeat ........ cycle {}   {}   started {}\n  snapshot owed {}  available {}  eligible {}  processed {}\n",
        vs.cycle_id,
        if vs.cycle_active { "ACTIVE" } else { "idle" },
        fmt_unix(vs.cycle_started_at),
        fmt_amount(vs.cycle_owed_snapshot),
        fmt_amount(vs.cycle_available_snapshot),
        vs.cycle_eligible_count,
        vs.cycle_processed_count
    ));
    o.push_str(&format!(
        "\nBonds ............ principal open {} of {} cap; withdrawal fees retained {}\n",
        fmt_amount(vs.bond_principal_open_total),
        fmt_amount(BOND_GLOBAL_CAP),
        fmt_amount(vs.bond_withdrawal_fees_retained)
    ));

    let mut regs = rpc.program_accounts(
        &keys.program_id,
        &<ProductRegistry as Discriminator>::DISCRIMINATOR.try_into().expect("8-byte discriminator"),
    )?;
    regs.sort_by_key(|(k, _)| k.to_string());
    o.push_str(&format!("\nRegistered products ({}):\n", regs.len()));
    for (addr, a) in regs {
        let mut d = a.data.as_slice();
        match ProductRegistry::try_deserialize(&mut d) {
            Ok(r) => {
                o.push_str(&format!(
                    "  {}  registry {addr}\n    {}{}\n    fee split {} bps, {} tiers, max payouts {}, requests {} / {}\n",
                    r.product_program_id,
                    if r.active { "ACTIVE" } else { "PAUSED" },
                    if r.active { String::new() } else { format!(" ({}, since {})", pause_name(r.pause_reason), fmt_unix(r.paused_since)) },
                    r.fee_split_bps,
                    r.challenge_sizes.len(),
                    r.max_payout_count,
                    r.total_requests_emitted,
                    fmt_amount(r.total_requested_amount)
                ));
            }
            Err(_) => o.push_str(&format!("  {addr}: does not decode as a ProductRegistry\n")),
        }
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_times_format() {
        assert_eq!(fmt_unix(0), "never");
        assert_eq!(fmt_unix(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(fmt_unix(951_782_400), "2000-02-29 00:00:00 UTC");
    }
}
