//! `status` decodes the real accounts: exact numbers, a frozen pool, the claims ceiling headroom.
#![cfg(feature = "localnet")]
mod common;

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AccountDeserialize, AccountSerialize};
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use setl8_admin::admin_ix::{AdminIx, Product, Tier};
use setl8_admin::constants::ata_address;
use solana_signer::Signer;

fn set_vault(w: &World, f: impl FnOnce(&mut core_vault::state::VaultState)) {
    let key = w.keys.vault();
    let acc = w.rpc.svm.borrow().get_account(&key).unwrap();
    let mut vs = core_vault::state::VaultState::try_deserialize(&mut acc.data.as_slice()).unwrap();
    f(&mut vs);
    let mut data = vec![];
    vs.try_serialize(&mut data).unwrap();
    data.resize(acc.data.len(), 0);
    w.rpc.svm.borrow_mut().set_account(key, solana_account::Account { data, ..acc }).unwrap();
}

fn register(w: &World, id: Pubkey, fee: u16) {
    let p = Product {
        product_program_id: id,
        fee_split_bps: fee,
        challenge_sizes: vec![Tier { size: 10_000_000_000, cost: 100_000_000 }],
        max_payout_count: 4,
        reset_price_bps: vec![100],
    };
    w.send_direct(AdminIx::RegisterProduct(p).build(&w.keys));
}

fn status(w: &World) -> String {
    let (c, h) = w.run(&[], &["status", "--cluster", "localnet"]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.prompts.is_empty());
    assert_eq!(w.rpc.sends.get(), 0, "status never sends");
    h.out
}

#[test]
fn status_reports_the_exact_numbers() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_234_567_891); // 1,234.567891
    w.fill_pool(&w.usdt, 80 * M);
    w.set_token(&ata_address(&w.sl8.pubkey(), &w.usdc), &w.usdc, &w.sl8.pubkey(), 42_500_000);
    w.rpc
        .svm
        .borrow_mut()
        .set_account(ata_address(&w.sl8.pubkey(), &w.usdt), solana_account::Account::default())
        .unwrap();
    set_vault(&w, |vs| {
        vs.usdc_floor = 400 * M;
        vs.marketing_withdrawn_usdc = 5 * M;
        vs.open_claims_count = 7;
        vs.open_claims_total = 2_400_000 * M;
        vs.cycle_id = 3;
        vs.cycle_active = true;
        vs.cycle_started_at = 1_700_000_000;
        vs.cycle_owed_snapshot = 2_000_000 * M;
        vs.cycle_available_snapshot = 1_314_567_891;
        vs.cycle_eligible_count = 6;
        vs.cycle_processed_count = 2;
        vs.bond_principal_open_total = 123_456_789;
        vs.bond_withdrawal_fees_retained = 7_000;
    });
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    register(&w, a, 6500);
    register(&w, b, 7000);
    w.send_direct(AdminIx::PauseProduct { product: b }.build(&w.keys));
    let out = status(&w);
    for want in [
        &format!("Vault PDA ...... {}", w.keys.vault()),
        "SL8 wallet ..... ", // then the SL8 key
        "balance 1,234.567891   frozen: no",
        "stored floor 400.000000   reserve 400.000000   admin_withdraw could take NOW: 834.567891",
        "withdrawn so far: 5.000000",
        "SL8 token account ",
        "balance 42.500000",
        "balance 80.000000   frozen: no",
        "stored floor 0.000000   reserve 20.000000   admin_withdraw could take NOW: 60.000000",
        "MISSING (deposits and withdrawals to SL8 fail until it exists)",
        "Open claims ...... 7 claims owing 2,400,000.000000",
        "Claims ceiling ... 2,500,000.000000   headroom 100,000.000000 (96% used)",
        "cycle 3   ACTIVE   started 2023-11-14 22:13:20 UTC",
        "snapshot owed 2,000,000.000000  available 1,314.567891  eligible 6  processed 2",
        "principal open 123.456789 of 600,000.000000 cap; withdrawal fees retained 0.007000",
        "Registered products (2):",
        &format!("{a}  registry {}", w.keys.registry(&a)),
        "fee split 6500 bps, 1 tiers, max payouts 4, requests 0 / 0.000000",
        "PAUSED (paused: planned upgrade, since 2023-11-14 22:13:20 UTC)",
        "ACTIVE",
        "Cluster ........ unrecognised cluster",
    ] {
        assert!(out.contains(want), "missing '{want}' in:\n{out}");
    }
    assert!(out.contains(&w.sl8.pubkey().to_string()));
}

#[test]
fn status_shows_a_frozen_pool_and_a_pool_at_the_claims_ceiling() {
    let w = World::with_vault();
    w.set_token_state(&w.pool(&w.usdt), &w.usdt, &w.keys.vault(), 500 * M, AccountState::Frozen);
    w.fill_pool(&w.usdc, 100 * M);
    set_vault(&w, |vs| vs.open_claims_total = core_vault::constants::OPEN_CLAIMS_CEILING);
    let out = status(&w);
    assert!(out.contains("balance 500.000000   frozen: YES (the heartbeat treats it as empty)"), "{out}");
    assert!(out.contains("headroom 0.000000 (100% used)"), "{out}");
    assert!(out.contains("Claims ceiling ... 2,500,000.000000"), "{out}");
}

#[test]
fn status_names_a_reconciliation_pause() {
    let w = World::with_vault();
    let a = Pubkey::new_unique();
    register(&w, a, 100);
    let key = w.keys.registry(&a);
    let acc = w.rpc.svm.borrow().get_account(&key).unwrap();
    let mut r = core_vault::state::ProductRegistry::try_deserialize(&mut acc.data.as_slice()).unwrap();
    r.active = false;
    r.pause_reason = core_vault::constants::PAUSE_RECONCILIATION_DEFICIT;
    r.paused_since = 1_700_000_000;
    r.total_requests_emitted = 3;
    r.total_requested_amount = 9_876_543;
    let mut data = vec![];
    r.try_serialize(&mut data).unwrap();
    data.resize(acc.data.len(), 0);
    w.rpc.svm.borrow_mut().set_account(key, solana_account::Account { data, ..acc }).unwrap();
    let out = status(&w);
    assert!(out.contains("PAUSED (paused: RECONCILIATION DEFICIT, since"), "{out}");
    assert!(out.contains("requests 3 / 9.876543"), "{out}");
}

#[test]
fn status_before_init_and_without_a_network() {
    let w = World::bare();
    let out = status(&w);
    assert!(out.contains("The vault does not exist on this cluster yet"), "{out}");
    let mut h = w.host();
    h.no_network = true;
    let code = setl8_admin::cli::run_guarded(&mut h, &["status".into(), "--cluster".into(), "localnet".into()]);
    assert_eq!(code, 1);
    assert!(h.err.contains("no network"), "{}", h.err);
}

#[test]
fn status_uses_the_clusters_public_endpoint_unless_told_otherwise() {
    let w = World::with_vault();
    let (_, h) = w.run(&[], &["status", "--cluster", "devnet"]);
    assert_eq!(h.rpc_urls, vec!["https://api.devnet.solana.com".to_string()]);
    let (_, h) = w.run(&[], &["status", "--cluster", "devnet", "--rpc", "http://my-node:8899"]);
    assert_eq!(h.rpc_urls, vec!["http://my-node:8899".to_string()]);
}
