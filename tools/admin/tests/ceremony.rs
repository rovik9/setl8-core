//! The six admin instructions, end to end through the real command line:
//! plan -> inspect -> sign (SL8) -> inspect -> sign (ROV) -> send into LiteSVM, with every
//! step reading the file back from disk (two machines), asserting the exact state change.
#![cfg(feature = "localnet")]
mod common;

use anchor_lang::prelude::Pubkey;
use common::*;
use setl8_admin::admin_ix::{AdminIx, Keys, Product, Side, Tier};
use setl8_admin::constants::ata_address;
use solana_signer::Signer;

fn amount_args<'a>(side: &'a str, amount: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["--pool", side, "--amount", amount, "--recent-blockhash"];
    v.extend_from_slice(extra);
    v
}

#[test]
fn init_vault_ceremony_creates_the_vault_and_both_pools() {
    let w = World::bare();
    let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
    assert!(!w.exists(&w.keys.vault()));
    let (code, h) = w.ceremony("init-vault", &["--usdc-mint", &usdc, "--usdt-mint", &usdt, "--recent-blockhash"]);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    let vs = w.vault_state();
    assert_eq!((vs.usdc_mint, vs.usdt_mint), (w.usdc, w.usdt));
    assert_eq!((vs.usdc_pool, vs.usdt_pool), (w.pool(&w.usdc), w.pool(&w.usdt)));
    assert_eq!(vs.sl8_wallet, w.sl8.pubkey());
    assert!(w.exists(&w.pool(&w.usdc)) && w.exists(&w.pool(&w.usdt)));
    assert!(h.out.contains("Confirmed."));
}

#[test]
fn register_update_pause_reactivate_ceremonies_change_exactly_the_registry() {
    let w = World::with_vault();
    let product = Pubkey::new_unique();
    let cfg = write_file(&w.dir, "product.json", &product_json(&product, 6500));
    let cfg = w.s(&cfg);

    let (code, h) = w.ceremony("register-product", &["--config", &cfg, "--recent-blockhash"]);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    let r = w.registry(&product).expect("registered");
    assert_eq!((r.fee_split_bps, r.max_payout_count, r.active), (6500, 5, true));
    assert_eq!(r.challenge_sizes.len(), 2);
    assert_eq!((r.challenge_sizes[1].size, r.challenge_sizes[1].cost), (50_000_000_000, 400_000_000));
    assert_eq!(r.reset_price_bps, vec![100, 150]);

    let cfg2 = write_file(&w.dir, "product2.json", &product_json(&product, 7000));
    let (code, h) = w.ceremony("update-product-config", &["--config", &w.s(&cfg2), "--recent-blockhash"]);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    assert_eq!(w.registry(&product).unwrap().fee_split_bps, 7000);

    let p = product.to_string();
    let (code, h) = w.ceremony("pause-product", &["--product", &p, "--recent-blockhash"]);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    let r = w.registry(&product).unwrap();
    assert!(!r.active);
    assert_eq!(r.pause_reason, core_vault::constants::PAUSE_PLANNED_UPGRADE);

    let (code, h) = w.ceremony("reactivate-product", &["--product", &p, "--recent-blockhash"]);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    assert!(w.registry(&product).unwrap().active);
}

#[test]
fn withdraw_ceremony_moves_exactly_the_amount_to_sl8s_token_account() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let sl8_ata = ata_address(&w.sl8.pubkey(), &w.usdc);
    let (code, h) = w.ceremony("admin-withdraw", &amount_args("usdc", "750", &[]));
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 250 * M);
    assert_eq!(w.token_balance(&sl8_ata), 750 * M);
    assert_eq!(w.vault_state().marketing_withdrawn_usdc, 750 * M);
    let (_, i) = w.inspect("tx.json", &[]);
    assert!(i.out.contains("withdraw 750.000000 USDC from the USDC pool"), "{}", i.out);
}

#[test]
fn withdraw_usdt_pool_and_fractional_amount() {
    let w = World::with_vault();
    w.fill_pool(&w.usdt, 2_000 * M);
    let (code, h) = w.ceremony("admin-withdraw-marketing-funds", &amount_args("usdt", "1,250.5", &[]));
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdt)), 2_000 * M - 1_250_500_000);
    assert_eq!(w.token_balance(&ata_address(&w.sl8.pubkey(), &w.usdt)), 1_250_500_000);
}

#[test]
fn a_single_signature_never_succeeds_not_even_with_a_forged_second_one() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let (c, h) = w.plan("admin-withdraw", "tx.json", &amount_args("usdc", "100", &[]));
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    let (c, _) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 0);
    // the tool refuses to send with one signature and never contacts the node to send it
    let (code, h) = w.send("tx.json", &[]);
    assert_eq!(code, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("missing or invalid"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0);
    // and the program itself rejects it even if someone bypasses the tool
    let file = w.tx_file("tx.json");
    let bytes = file.message_bytes().unwrap();
    let sl8_sig: solana_signature::Signature = file.signatures[0].signature.parse().unwrap();
    let wire = setl8_admin::send::wire_transaction(&[sl8_sig, solana_signature::Signature::default()], &bytes);
    let t: solana_transaction::Transaction = bincode::deserialize(&wire).unwrap();
    let r = w.rpc.svm.borrow_mut().send_transaction(t);
    assert!(r.is_err(), "the program accepted a transaction with one admin signature");
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 1_000 * M);
}

#[test]
fn the_plan_bytes_equal_what_the_existing_test_harness_builds() {
    // The same constructors tests-rs/tests/common/mod.rs uses.
    let k = Keys::compiled();
    let (usdc, usdt, product) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
    let prod = Product {
        product_program_id: product,
        fee_split_bps: 6500,
        challenge_sizes: vec![Tier { size: 10_000, cost: 100 }, Tier { size: 50_000, cost: 400 }],
        max_payout_count: 3,
        reset_price_bps: vec![100, 150, 450],
    };
    let sys = anchor_lang::solana_program::instruction::AccountMeta::new_readonly(
        anchor_lang::solana_program::system_program::ID,
        false,
    );
    let tiers =
        vec![si040::ChallengeSize { size: 10_000, cost: 100 }, si040::ChallengeSize { size: 50_000, cost: 400 }];
    let reg = k.registry(&product);

    let want = [
        (AdminIx::InitVault { usdc_mint: usdc, usdt_mint: usdt }, AdminIxDirect::init_vault(&k, usdc, usdt)),
        (
            AdminIx::RegisterProduct(prod.clone()),
            si040::register_product(
                k.program_id,
                k.sl8,
                k.rov,
                reg,
                std::slice::from_ref(&sys),
                si040::RegisterProductArgs {
                    product_program_id: product,
                    fee_split_bps: 6500,
                    challenge_sizes: tiers.clone(),
                    max_payout_count: 3,
                    reset_price_bps: vec![100, 150, 450],
                },
            ),
        ),
        (
            AdminIx::UpdateProductConfig(prod.clone()),
            si040::update_product_config(
                k.program_id,
                k.sl8,
                k.rov,
                reg,
                &[],
                si040::UpdateProductConfigArgs {
                    product_program_id: product,
                    challenge_sizes: tiers,
                    fee_split_bps: 6500,
                    max_payout_count: 3,
                    reset_price_bps: vec![100, 150, 450],
                },
            ),
        ),
        (
            AdminIx::PauseProduct { product },
            si040::pause_product(
                k.program_id,
                k.sl8,
                k.rov,
                reg,
                &[],
                si040::PauseProductArgs { product_program_id: product },
            ),
        ),
        (
            AdminIx::ReactivateProduct { product },
            si040::reactivate_product(
                k.program_id,
                k.sl8,
                k.rov,
                reg,
                &[],
                si040::ReactivateProductArgs { product_program_id: product },
            ),
        ),
        (
            AdminIx::Withdraw { pool: Side::Usdc, amount: 123_456_789, mint: usdc },
            AdminIxDirect::withdraw(&k, true, usdc, 123_456_789),
        ),
        (AdminIx::Withdraw { pool: Side::Usdt, amount: 1, mint: usdt }, AdminIxDirect::withdraw(&k, false, usdt, 1)),
    ];
    for (a, direct) in want {
        let got = a.build(&k);
        assert_eq!(got.program_id, direct.program_id, "{}", a.name());
        assert_eq!(got.data, direct.data, "{} data", a.name());
        assert_eq!(got.accounts, direct.accounts, "{} accounts", a.name());
    }
}

#[test]
fn compute_budget_and_priority_fee_are_allowed_and_shown() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 100 * M);
    let (code, h) = w.ceremony(
        "admin-withdraw",
        &amount_args("usdc", "10", &["--compute-unit-limit", "200000", "--priority-fee-microlamports", "7"]),
    );
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    let (_, i) = w.inspect("tx.json", &[]);
    assert!(i.out.contains("Compute limit .. 200000") && i.out.contains("Priority fee ... 7"), "{}", i.out);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 90 * M);
}

#[test]
fn a_different_fee_payer_needs_a_third_signature_and_must_be_declared() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 100 * M);
    let payer = solana_keypair::Keypair::new();
    w.rpc.svm.borrow_mut().airdrop(&payer.pubkey(), 1_000_000_000).unwrap();
    let payer_key = write_key(&w.dir, "payer.json", &payer);
    let pp = payer.pubkey().to_string();
    let (c, h) = w.plan("admin-withdraw", "tx.json", &amount_args("usdc", "10", &["--fee-payer", &pp]));
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert_eq!(w.tx_file("tx.json").required_signers.len(), 3);
    let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.out.contains("fee payer is") && h.out.contains("was expected"), "{}", h.out);
    for k in [&w.sl8_key, &w.rov_key, &payer_key] {
        let (c, h) = w.sign("tx.json", k, &["--fee-payer", &pp]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
    }
    let (c, h) = w.send("tx.json", &["--fee-payer", &pp]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 90 * M);
}
