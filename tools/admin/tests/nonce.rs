//! Durable nonces: signatures that stay valid for hours, and the way to revoke them.
#![cfg(feature = "localnet")]
mod common;

use anchor_lang::prelude::Pubkey;
use common::*;
use setl8_admin::constants::ata_address;
use setl8_admin::plan::parse_nonce_account;
use solana_signer::Signer;

/// Runs `nonce-create` as the SL8 key and returns the new nonce account.
fn make_nonce(w: &World) -> Pubkey {
    let k = w.s(&w.sl8_key);
    let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", "localnet", "--keypair", &k]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    let at = h.out.find("Nonce account: ").expect("nonce printed") + "Nonce account: ".len();
    let nonce: Pubkey = h.out[at..].split_whitespace().next().unwrap().parse().unwrap();
    w.expire_blockhash(); // a block later, as on a real chain
    nonce
}

fn withdraw_args<'a>(nonce: &'a str, amount: &'a str) -> Vec<&'a str> {
    vec!["--pool", "usdc", "--amount", amount, "--nonce-account", nonce]
}

#[test]
fn nonce_create_makes_a_rent_exempt_nonce_account_owned_by_sl8_and_writes_no_secret() {
    let w = World::with_vault();
    let before: Vec<_> = std::fs::read_dir(&w.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    let nonce = make_nonce(&w);
    let acc = w.rpc.svm.borrow().get_account(&nonce).expect("nonce account exists");
    assert_eq!(acc.owner, setl8_admin::constants::SYSTEM_PROGRAM_ID);
    assert_eq!(acc.data.len(), 80);
    assert_eq!(acc.lamports, w.rpc.svm.borrow().minimum_balance_for_rent_exemption(80));
    let (authority, _) = parse_nonce_account(&acc.data).unwrap();
    assert_eq!(authority, w.sl8.pubkey());
    let after: Vec<_> = std::fs::read_dir(&w.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before.len(), after.len(), "nonce-create must not write any file (the nonce key is never stored)");
}

#[test]
fn a_nonce_signed_transaction_still_sends_after_the_blockhash_window_has_passed() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let nonce = make_nonce(&w).to_string();
    let (c, h) = w.plan("admin-withdraw", "tx.json", &withdraw_args(&nonce, "100"));
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(!h.out.contains("WARNING: a recent blockhash"), "{}", h.out);
    let (c, _) = w.sign("tx.json", &w.sl8_key, &["--nonce-account", &nonce]);
    assert_eq!(c, 0);
    // hours pass: many blockhashes expire and the slot moves a long way
    for _ in 0..10 {
        w.expire_blockhash();
    }
    w.rpc.svm.borrow_mut().warp_to_slot(100_000);
    let (c, h) = w.sign("tx.json", &w.rov_key, &["--nonce-account", &nonce]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    let (c, h) = w.send("tx.json", &["--nonce-account", &nonce]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 900 * M);
    // the nonce moved on: replaying the same file does nothing
    let (c, _) = w.send("tx.json", &["--nonce-account", &nonce]);
    assert_ne!(c, 0);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 900 * M);
}

#[test]
fn a_recent_blockhash_transaction_dies_with_the_window() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let (c, h) = w.plan("admin-withdraw", "tx.json", &["--pool", "usdc", "--amount", "100", "--recent-blockhash"]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.out.contains("WARNING: a recent blockhash expires after about 60-90 seconds"), "{}", h.out);
    let (_, i) = w.inspect("tx.json", &[]);
    assert!(i.out.contains("RECENT BLOCKHASH: valid for only ~60-90 seconds"), "{}", i.out);
    w.sign("tx.json", &w.sl8_key, &[]);
    w.sign("tx.json", &w.rov_key, &[]);
    w.expire_blockhash();
    let (c, h) = w.send("tx.json", &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("the simulation failed") && h.err.contains("BlockhashNotFound"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0, "a failed simulation must stop the send");
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 1_000 * M);
}

#[test]
fn nonce_advance_revokes_a_signed_but_unsent_transaction() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let nonce = make_nonce(&w);
    let n = nonce.to_string();
    w.plan("admin-withdraw", "tx.json", &withdraw_args(&n, "100"));
    w.sign("tx.json", &w.sl8_key, &["--nonce-account", &n]);
    w.sign("tx.json", &w.rov_key, &["--nonce-account", &n]);
    // second thoughts: revoke
    let k = w.s(&w.sl8_key);
    let (c, h) = w.run_auto(&[], &["nonce-advance", "--cluster", "localnet", "--keypair", &k, "--nonce-account", &n]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.out.contains("INVALIDATES EVERY TRANSACTION ALREADY SIGNED"), "{}", h.out);
    w.expire_blockhash();
    let (c, h) = w.send("tx.json", &["--nonce-account", &n]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("BlockhashNotFound"), "{}", h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 1_000 * M);
    assert_eq!(w.token_balance(&ata_address(&w.sl8.pubkey(), &w.usdc)), 0);
    // a fresh plan on the advanced nonce works
    let (c, h) = w.ceremony("admin-withdraw", &withdraw_args(&n, "100"));
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 900 * M);
}

#[test]
fn only_the_nonce_authority_can_advance() {
    let w = World::with_vault();
    let nonce = make_nonce(&w).to_string();
    let k = w.s(&w.rov_key); // ROV is not the authority
    let (c, h) =
        w.run_auto(&[], &["nonce-advance", "--cluster", "localnet", "--keypair", &k, "--nonce-account", &nonce]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("the nonce authority is"), "{}", h.err);
}

#[test]
fn plan_refuses_a_bad_nonce_account() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    // does not exist
    let ghost = Pubkey::new_unique().to_string();
    let (c, h) = w.plan("admin-withdraw", "t.json", &withdraw_args(&ghost, "1"));
    assert_eq!(c, 1);
    assert!(h.err.contains("does not exist"), "{}", h.err);
    // not a system-owned account
    let tok = w.pool(&w.usdc).to_string();
    let (c, h) = w.plan("admin-withdraw", "t.json", &withdraw_args(&tok, "1"));
    assert_eq!(c, 1);
    assert!(h.err.contains("not owned by the System Program"), "{}", h.err);
    // authority is not who the plan expects
    let nonce = make_nonce(&w).to_string();
    let other = Pubkey::new_unique().to_string();
    let mut a = withdraw_args(&nonce, "1");
    a.extend_from_slice(&["--nonce-authority", &other]);
    let (c, h) = w.plan("admin-withdraw", "t.json", &a);
    assert_eq!(c, 1);
    assert!(h.err.contains("the nonce account's authority is"), "{}", h.err);
    // uninitialised: right owner and length, zero data
    let blank = Pubkey::new_unique();
    w.rpc
        .svm
        .borrow_mut()
        .set_account(
            blank,
            solana_account::Account {
                lamports: 2_000_000,
                data: vec![0; 80],
                owner: setl8_admin::constants::SYSTEM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let (c, h) = w.plan("admin-withdraw", "t.json", &withdraw_args(&blank.to_string(), "1"));
    assert_eq!(c, 1);
    assert!(h.err.contains("not initialised"), "{}", h.err);
    // wrong length
    let short = Pubkey::new_unique();
    w.rpc
        .svm
        .borrow_mut()
        .set_account(
            short,
            solana_account::Account {
                lamports: 2_000_000,
                data: vec![0; 10],
                owner: setl8_admin::constants::SYSTEM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let (c, h) = w.plan("admin-withdraw", "t.json", &withdraw_args(&short.to_string(), "1"));
    assert_eq!(c, 1);
    assert!(h.err.contains("a nonce account has 80"), "{}", h.err);
}

#[test]
fn an_offline_plan_with_a_supplied_nonce_value_needs_no_network_and_says_it_is_unverified() {
    let w = World::with_vault();
    let nonce = Pubkey::new_unique().to_string();
    let hash = solana_hash::Hash::new_from_array([9; 32]).to_string();
    let out = w.s(&w.path("off.json"));
    let mut h = w.host();
    h.no_network = true;
    let argv: Vec<String> = [
        "plan",
        "admin-withdraw",
        "--cluster",
        "localnet",
        "--out",
        &out,
        "--pool",
        "usdc",
        "--amount",
        "5",
        "--nonce-account",
        &nonce,
        "--nonce-blockhash",
        &hash,
        "--offline",
        "--genesis-hash",
        LOCAL_GENESIS,
        "--mint",
        &w.usdc.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let code = setl8_admin::cli::run_guarded(&mut h, &argv);
    assert_eq!(code, 0, "{}{}", h.out, h.err);
    assert!(h.rpc_urls.is_empty(), "offline plan must not even ask for an RPC client");
    assert!(
        h.out.contains("NOT verified against the chain") && h.out.contains("OFFLINE: no reserve pre-flight"),
        "{}",
        h.out
    );
    assert!(w.tx_file("off.json").nonce_account.is_some());
}

#[test]
fn plan_needs_a_lifetime_and_never_both() {
    let w = World::with_vault();
    let (c, h) = w.plan("admin-withdraw", "t.json", &["--pool", "usdc", "--amount", "1"]);
    assert_eq!(c, 1);
    assert!(h.err.contains("choose a transaction lifetime"), "{}", h.err);
    let n = Pubkey::new_unique().to_string();
    let (c, h) = w.plan(
        "admin-withdraw",
        "t.json",
        &["--pool", "usdc", "--amount", "1", "--nonce-account", &n, "--recent-blockhash"],
    );
    assert_eq!(c, 1);
    assert!(h.err.contains("not both"), "{}", h.err);
}
