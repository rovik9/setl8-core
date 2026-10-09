//! Cluster safety, the mainnet gate, the confirmation prompt, key-file handling, secrecy of
//! the output, the command line surface, and the withdrawal pre-flight.
#![cfg(feature = "localnet")]
mod common;

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AccountDeserialize, AccountSerialize};
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use setl8_admin::constants::{ata_address, DEVNET_GENESIS, MAINNET_GENESIS};
use setl8_admin::host::Host;
use solana_keypair::Keypair;
use solana_signer::Signer;

// ------------------------------------------------------------------ cluster binding

/// A withdraw bound to `genesis` (built with --offline so the node's own genesis is irrelevant).
fn plan_for_cluster(w: &World, cluster: &str, out: &str) {
    w.fill_pool(&w.usdc, 1_000 * M);
    let bh = w.rpc.svm.borrow().latest_blockhash().to_string();
    let out_path = w.s(&w.path(out));
    let mint = w.usdc.to_string();
    let mut args = vec![
        "plan",
        "admin-withdraw",
        "--cluster",
        cluster,
        "--out",
        &out_path,
        "--pool",
        "usdc",
        "--amount",
        "100",
        "--mint",
        &mint,
        "--recent-blockhash",
        &bh,
        "--offline",
    ];
    if cluster == "localnet" {
        args.extend_from_slice(&["--genesis-hash", LOCAL_GENESIS]);
    }
    let (c, h) = w.run(&[], &args);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
}

fn both_sign(w: &World, name: &str) {
    for k in [&w.sl8_key, &w.rov_key] {
        let (c, h) = w.sign(name, k, &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
    }
}

#[test]
fn a_devnet_transaction_is_refused_by_a_node_on_another_cluster() {
    let w = World::with_vault();
    plan_for_cluster(&w, "devnet", "tx.json");
    assert_eq!(w.tx_file("tx.json").genesis_hash, DEVNET_GENESIS);
    both_sign(&w, "tx.json");
    // the node is the local chain
    let (c, h) = w.send("tx.json", &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("is on unrecognised cluster") && h.err.contains("bound to devnet"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0);
    assert_eq!(w.rpc.simulations.get(), 0, "no simulation against the wrong cluster");
    // a node on MAINNET is refused too: devnet and mainnet cannot be confused
    w.set_genesis(MAINNET_GENESIS);
    let (c, h) = w.send("tx.json", &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("is on mainnet-beta") && h.err.contains("bound to devnet"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 1_000 * M);
}

#[test]
fn plan_refuses_a_node_whose_genesis_is_not_the_named_cluster() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    // --cluster devnet but the node answers with the local genesis
    let (c, h) = w.run(
        &[],
        &[
            "plan",
            "pause-product",
            "--cluster",
            "devnet",
            "--out",
            &w.s(&w.path("t.json")),
            "--product",
            &Pubkey::new_unique().to_string(),
            "--recent-blockhash",
        ],
    );
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("is on genesis") && h.err.contains("means"), "{}", h.err);
    assert!(!w.path("t.json").exists());
}

#[test]
fn nonce_commands_check_the_cluster_too() {
    let w = World::with_vault();
    let k = w.s(&w.sl8_key);
    let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", "devnet", "--keypair", &k]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("means genesis"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0);
}

#[test]
fn inspect_with_rpc_flags_a_node_on_another_cluster() {
    let w = World::with_vault();
    plan_for_cluster(&w, "devnet", "tx.json");
    let (c, h) = w.inspect("tx.json", &["--rpc", "http://x"]);
    assert_eq!(c, 2, "{}", h.out);
    assert!(h.out.contains("the node is on genesis") && h.out.contains("is bound to"), "{}", h.out);
}

// ------------------------------------------------------------------ mainnet gate

fn mainnet_ready(w: &World) {
    w.set_genesis(MAINNET_GENESIS);
    plan_for_cluster(w, "mainnet", "tx.json");
    assert_eq!(w.tx_file("tx.json").cluster, "mainnet-beta");
    both_sign(w, "tx.json");
}

#[test]
fn sending_to_mainnet_without_the_flag_is_refused() {
    let w = World::with_vault();
    mainnet_ready(&w);
    let (c, h) = w.run(&["MAINNET"], &["send", &w.s(&w.path("tx.json")), "--cluster", "mainnet"]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("--i-understand-this-is-mainnet"), "{}", h.err);
    assert!(h.prompts.is_empty(), "refused before any prompt");
    assert!(h.rpc_urls.is_empty(), "refused before the node was even contacted");
    assert_eq!((w.rpc.sends.get(), w.rpc.simulations.get()), (0, 0));
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 1_000 * M);
}

#[test]
fn the_flag_alone_is_not_enough_the_word_must_be_typed() {
    let w = World::with_vault();
    mainnet_ready(&w);
    let p = w.s(&w.path("tx.json"));
    for wrong in ["mainnet", "yes", "", "MAINNET ", "MAINNE"] {
        let (c, h) = w.run(&[wrong], &["send", &p, "--cluster", "mainnet", "--i-understand-this-is-mainnet"]);
        if wrong == "MAINNET " {
            assert_eq!(c, 0, "trailing whitespace is trimmed: {}{}", h.out, h.err); // sends: stop after this
            return;
        }
        assert_eq!(c, 2, "'{wrong}' accepted:\n{}{}", h.out, h.err);
        assert!(h.err.contains("you did not type MAINNET"), "{}", h.err);
        assert_eq!(w.rpc.sends.get(), 0, "'{wrong}'");
    }
}

#[test]
fn flag_and_word_together_send_to_mainnet() {
    let w = World::with_vault();
    mainnet_ready(&w);
    let (c, h) = w.run(
        &["MAINNET"],
        &["send", &w.s(&w.path("tx.json")), "--cluster", "mainnet", "--i-understand-this-is-mainnet"],
    );
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.out.contains("MAINNET-BETA") && h.out.contains("Confirmed."), "{}", h.out);
    assert_eq!(w.token_balance(&w.pool(&w.usdc)), 900 * M);
}

#[test]
fn signing_a_mainnet_transaction_shows_the_banner() {
    let w = World::with_vault();
    w.set_genesis(MAINNET_GENESIS);
    plan_for_cluster(&w, "mainnet", "tx.json");
    let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 0);
    assert!(h.out.contains("THIS TRANSACTION IS BOUND TO MAINNET-BETA"), "{}", h.out);
}

#[test]
fn nonce_commands_on_mainnet_need_the_gate_too() {
    let w = World::with_vault();
    w.set_genesis(MAINNET_GENESIS);
    let k = w.s(&w.sl8_key);
    let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", "mainnet", "--keypair", &k]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("--i-understand-this-is-mainnet"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), 0);
    let (c, h) = w.run_auto(
        &["MAINNET"],
        &["nonce-create", "--cluster", "mainnet", "--keypair", &k, "--i-understand-this-is-mainnet"],
    );
    assert_eq!(c, 0, "{}{}", h.out, h.err);
}

// ------------------------------------------------------------------ confirmation prompt

#[test]
fn a_wrong_confirmation_does_not_sign() {
    let w = World::with_vault();
    plan_for_cluster(&w, "localnet", "tx.json");
    let right = w.hash_of("tx.json");
    let before = std::fs::read_to_string(w.path("tx.json")).unwrap();
    for wrong in ["", "00000000", "zzzzzzzz", &right[..7], &right[1..9], "the hash"] {
        let (c, h) = w.sign_with_answer("tx.json", &w.sl8_key, wrong, &[]);
        assert_eq!(c, 2, "'{wrong}' accepted:\n{}{}", h.out, h.err);
        assert!(h.err.contains("not the start of the message hash"), "{}", h.err);
        assert_eq!(std::fs::read_to_string(w.path("tx.json")).unwrap(), before);
    }
    assert!(w.tx_file("tx.json").signatures.is_empty());
    // the prompt asks for 8 characters
    let (_, h) = w.sign_with_answer("tx.json", &w.sl8_key, "x", &[]);
    assert!(h.prompts[0].contains("retype the first 8 characters of the message hash"), "{:?}", h.prompts);
    // upper case and surrounding spaces are fine; a longer correct prefix is not what was asked but starts right
    let (c, _) = w.sign_with_answer("tx.json", &w.sl8_key, &format!(" {} ", right[..8].to_uppercase()), &[]);
    assert_eq!(c, 0);
}

#[test]
fn there_is_no_yes_flag_anywhere() {
    let w = World::with_vault();
    plan_for_cluster(&w, "localnet", "tx.json");
    let p = w.s(&w.path("tx.json"));
    let k = w.s(&w.sl8_key);
    for cmd in [
        vec!["sign", &p, "--keypair", &k, "--yes"],
        vec!["send", &p, "--cluster", "localnet", "--yes"],
        vec!["nonce-create", "--cluster", "localnet", "--keypair", &k, "--yes"],
        vec!["sign", &p, "--keypair", &k, "-y"],
    ] {
        let (c, h) = w.run(&[], &cmd);
        assert_eq!(c, 1, "{cmd:?}: {}{}", h.out, h.err);
        assert!(h.err.contains("unknown option"), "{}", h.err);
    }
    assert!(w.tx_file("tx.json").signatures.is_empty());
    assert!(!setl8_admin::cli::run_guarded(&mut w.host(), &["help".into()]).ne(&0));
    let mut h = w.host();
    setl8_admin::cli::run(&mut h, &["help".into()]);
    assert!(!h.out.contains("--yes"));
}

// ------------------------------------------------------------------ key files

#[cfg(unix)]
fn chmod(p: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_key_file_readable_by_others_is_refused() {
    let w = World::with_vault();
    plan_for_cluster(&w, "localnet", "tx.json");
    for mode in [0o644, 0o640, 0o604, 0o660, 0o777, 0o470] {
        chmod(&w.sl8_key, mode);
        let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
        assert_eq!(c, 2, "mode {mode:o} accepted:\n{}{}", h.out, h.err);
        assert!(h.err.contains("other users can read it") && h.err.contains("chmod 600"), "{}", h.err);
        assert!(h.prompts.is_empty(), "refused before asking");
        assert!(w.tx_file("tx.json").signatures.is_empty());
    }
    // the override works, with a warning
    chmod(&w.sl8_key, 0o644);
    let (c, h) = w.sign("tx.json", &w.sl8_key, &["--allow-loose-perms"]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.err.contains("WARNING") && h.err.contains("--allow-loose-perms"), "{}", h.err);
    // tight modes pass without any warning
    chmod(&w.rov_key, 0o400);
    let (c, h) = w.sign("tx.json", &w.rov_key, &[]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(!h.err.contains("WARNING"));
}

#[test]
fn nonce_commands_apply_the_same_key_file_rule() {
    let w = World::with_vault();
    chmod(&w.sl8_key, 0o644);
    let k = w.s(&w.sl8_key);
    let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", "localnet", "--keypair", &k]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("chmod 600"), "{}", h.err);
}

#[test]
fn bad_key_files_are_rejected_without_echoing_their_contents() {
    let w = World::with_vault();
    plan_for_cluster(&w, "localnet", "tx.json");
    let secret_word = "TOPSECRETWORD9876";
    let good = w.sl8.to_bytes();
    let mut with_300 = good.iter().map(|b| b.to_string()).collect::<Vec<_>>();
    with_300[5] = "300".into();
    let mut wrong_pub = good.to_vec();
    wrong_pub[40] ^= 1; // the public half no longer matches the seed
    let cases: Vec<(&str, String)> = vec![
        ("text.json", format!("{secret_word} 1 2 3")),
        ("range.json", format!("[{}]", with_300.join(","))),
        ("short.json", format!("[{}]", good[..40].iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","))),
        ("pubmismatch.json", format!("[{}]", wrong_pub.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","))),
        ("object.json", format!("{{\"key\":\"{secret_word}\"}}")),
        ("empty.json", String::new()),
    ];
    for (name, body) in cases {
        let p = write_file(&w.dir, name, &body);
        chmod(&p, 0o600);
        let (c, h) = w.sign("tx.json", &p, &[]);
        assert_eq!(c, 1, "{name}: {}{}", h.out, h.err);
        let all = h.all_output();
        assert!(all.contains("is not a keypair file"), "{name}: {all}");
        assert!(!all.contains(secret_word), "{name} leaked its contents");
        assert!(!all.contains("300"), "{name} leaked a value from the file");
    }
    // a missing file and a directory
    let (c, h) = w.sign("tx.json", &w.path("nope.json"), &[]);
    assert_eq!(c, 1);
    assert!(h.err.contains("cannot read key file"), "{}", h.err);
    let (c, _) = w.sign("tx.json", &w.dir, &[]);
    assert_ne!(c, 0, "a directory is not a key file");
}

// ------------------------------------------------------------------ no secrets in any output

fn forms_of(kp: &Keypair) -> Vec<String> {
    let b = kp.to_bytes();
    vec![
        bs58::encode(b).into_string(),
        bs58::encode(&b[..32]).into_string(),
        b.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
        b[..32].iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
        b[..32].iter().map(|x| format!("{x:02x}")).collect::<String>(),
        b.iter().map(|x| format!("{x:02x}")).collect::<String>(),
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b),
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &b[..32]),
    ]
}

#[test]
fn no_command_output_or_file_ever_contains_key_material() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let secrets: Vec<String> = [&w.sl8, &w.rov].iter().flat_map(|k| forms_of(k)).collect();
    let mut seen = String::new();
    let mut take = |r: (i32, TestHost)| {
        seen.push_str(&r.1.all_output());
        r.0
    };
    // the whole ceremony, plus the failure paths that touch the key file
    assert_eq!(
        take(w.plan("admin-withdraw", "tx.json", &["--pool", "usdc", "--amount", "10", "--recent-blockhash"])),
        0
    );
    assert_eq!(take(w.inspect("tx.json", &[])), 0);
    take(w.sign_with_answer("tx.json", &w.sl8_key, "wrong!!!", &[])); // wrong confirmation
    assert_eq!(take(w.sign("tx.json", &w.sl8_key, &[])), 0);
    take(w.sign("tx.json", &w.sl8_key, &[])); // already signed
    assert_eq!(take(w.sign("tx.json", &w.rov_key, &[])), 0);
    assert_eq!(take(w.send("tx.json", &[])), 0);
    let k = w.s(&w.sl8_key);
    assert_eq!(take(w.run_auto(&[], &["nonce-create", "--cluster", "localnet", "--keypair", &k])), 0);
    take(w.run(&[], &["status", "--cluster", "localnet"]));
    take(w.run(&[], &["sign", "--help"]));
    for s in &secrets {
        assert!(!seen.contains(s.as_str()), "key material appeared in output: {}...", &s[..10]);
    }
    // nor in any file the tool wrote (everything in the dir except the two key files themselves)
    for e in std::fs::read_dir(&w.dir).unwrap() {
        let e = e.unwrap();
        if e.file_name() == "sl8.json" || e.file_name() == "rov.json" {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap();
        for s in &secrets {
            assert!(!text.contains(s.as_str()), "key material in {:?}", e.file_name());
        }
    }
}

struct PanicHost {
    err: String,
}
impl Host for PanicHost {
    fn out(&mut self, _s: &str) {
        panic!("KEYBYTES:[1,2,3,4] should never be printed");
    }
    fn err(&mut self, s: &str) {
        self.err.push_str(s);
    }
    fn prompt(&mut self, _q: &str) -> setl8_admin::error::Result<String> {
        Ok(String::new())
    }
    fn rpc(&mut self, _u: &str) -> setl8_admin::error::Result<Box<dyn setl8_admin::rpc::Rpc>> {
        unreachable!()
    }
}

#[test]
fn a_panic_is_reported_without_its_message() {
    let previous = std::panic::take_hook();
    setl8_admin::cli::install_panic_hook();
    let mut h = PanicHost { err: String::new() };
    let code = setl8_admin::cli::run_guarded(&mut h, &["version".to_string()]);
    std::panic::set_hook(previous);
    assert_eq!(code, 70);
    assert!(h.err.contains("internal error") && h.err.contains("Nothing was sent"), "{}", h.err);
    assert!(!h.err.contains("KEYBYTES"), "{}", h.err);
}

// ------------------------------------------------------------------ the command line surface

#[test]
fn every_command_has_help_and_the_error_paths_are_clean() {
    let w = World::with_vault();
    for cmd in ["plan", "inspect", "sign", "add-signature", "send", "status", "nonce-create", "nonce-advance"] {
        for flag in ["--help", "-h"] {
            let (c, h) = w.run(&[], &[cmd, flag]);
            assert_eq!(c, 0, "{cmd} {flag}");
            assert!(h.out.contains("setl8-admin"), "{cmd}: {}", h.out);
            assert!(h.err.is_empty());
        }
    }
    for g in [vec!["help"], vec!["--help"], vec!["-h"]] {
        let (c, h) = w.run(&[], &g);
        assert_eq!(c, 0);
        assert!(h.out.contains("COMMANDS"), "{}", h.out);
    }
    let (c, h) = w.run(&[], &["version"]);
    assert_eq!(c, 0);
    assert!(h.out.contains("PUBLIC TEST admin keys"), "{}", h.out);
    let (c, h) = w.run(&[], &[]);
    assert_eq!(c, 1);
    assert!(h.out.contains("USAGE"));
    let (c, h) = w.run(&[], &["frobnicate"]);
    assert_eq!(c, 1);
    assert!(h.err.contains("unknown command 'frobnicate'"), "{}", h.err);

    let t = w.s(&w.path("tx.json"));
    plan_for_cluster(&w, "localnet", "tx.json");
    let k = w.s(&w.sl8_key);
    let bad: Vec<(Vec<&str>, &str)> = vec![
        (vec!["plan"], "plan needs an instruction name"),
        (vec!["plan", "admin-withdraw", "--cluster", "localnet"], "--out is required"),
        (vec!["plan", "frob", "--cluster", "localnet", "--out", "x"], "unknown instruction 'frob'"),
        (vec!["plan", "admin-withdraw", "--cluster", "moon", "--out", "x"], "unknown cluster 'moon'"),
        (
            vec![
                "plan",
                "admin-withdraw",
                "--cluster",
                "localnet",
                "--out",
                "x",
                "--pool",
                "dai",
                "--amount",
                "1",
                "--recent-blockhash",
            ],
            "pool must be usdc or usdt",
        ),
        (
            vec![
                "plan",
                "admin-withdraw",
                "--cluster",
                "localnet",
                "--out",
                "x",
                "--pool",
                "usdc",
                "--amount",
                "1.0000001",
                "--recent-blockhash",
            ],
            "more than 6 decimals",
        ),
        (
            vec![
                "plan",
                "admin-withdraw",
                "--cluster",
                "localnet",
                "--out",
                "x",
                "--pool",
                "usdc",
                "--amount",
                "-1",
                "--recent-blockhash",
            ],
            "is not an amount",
        ),
        (
            vec![
                "plan",
                "pause-product",
                "--cluster",
                "localnet",
                "--out",
                "x",
                "--product",
                "zzz",
                "--recent-blockhash",
            ],
            "not a valid public key",
        ),
        (
            vec!["plan", "init-vault", "--cluster", "localnet", "--out", "x", "--recent-blockhash"],
            "--usdc-mint is required",
        ),
        (
            vec![
                "plan",
                "register-product",
                "--cluster",
                "localnet",
                "--out",
                "x",
                "--config",
                "/nonexistent.json",
                "--recent-blockhash",
            ],
            "cannot read",
        ),
        (
            vec!["plan", "pause-product", "--cluster", "localnet", "--out", "x", "--product", "--recent-blockhash"],
            "--product needs a value",
        ),
        (vec!["plan", "pause-product", "pause-product", "--cluster", "localnet", "--out", "x"], "unexpected argument"),
        (vec!["plan", "admin-withdraw", "--cluster", "localnet", "--cluster", "devnet"], "given twice"),
        (vec!["plan", "admin-withdraw", "--offline=1"], "takes no value"),
        (vec!["inspect"], "exactly one transaction file"),
        (vec!["inspect", &t, "--fee-payer", "bad"], "not a valid public key"),
        (vec!["sign", &t], "--keypair is required"),
        (vec!["sign", &t, "--keypair", &k, "--bogus"], "unknown option --bogus"),
        (vec!["add-signature", &t, "--pubkey", "bad", "--signature", "x"], "not a valid public key"),
        (vec!["send", &t], "--cluster is required"),
        (vec!["status"], "--cluster is required"),
        (vec!["status", "--cluster", "localnet", "extra"], "unexpected argument"),
        (vec!["nonce-create", "--cluster", "localnet"], "--keypair is required"),
        (vec!["nonce-advance", "--cluster", "localnet", "--keypair", &k], "--nonce-account is required"),
    ];
    for (args, needle) in bad {
        let (c, h) = w.run(&[], &args);
        assert_eq!(c, 1, "{args:?}: {}{}", h.out, h.err);
        assert!(h.err.contains(needle), "{args:?}: expected '{needle}' in '{}'", h.err);
        assert!(h.err.starts_with("error: "), "{}", h.err);
    }
}

#[test]
fn product_config_errors_and_warnings_surface_in_plan() {
    let w = World::with_vault();
    let p = Pubkey::new_unique();
    let cfg = |name: &str, body: String| w.s(&write_file(&w.dir, name, &body));
    let plan = |c: &str| {
        w.run(
            &[],
            &[
                "plan",
                "register-product",
                "--cluster",
                "localnet",
                "--out",
                &w.s(&w.path("r.json")),
                "--config",
                c,
                "--recent-blockhash",
            ],
        )
    };
    let tiers33 = format!(
        r#"{{"product_program_id":"{p}","fee_split_bps":100,"challenge_sizes":[{}],"max_payout_count":1,"reset_price_bps":[]}}"#,
        vec![r#"{"size":1,"cost":1}"#; 33].join(",")
    );
    let (c, h) = plan(&cfg("a.json", tiers33));
    assert_eq!(c, 1);
    assert!(h.err.contains("33 challenge sizes"), "{}", h.err);
    let (c, h) = plan(&cfg(
        "b.json",
        format!(
            r#"{{"product_program_id":"{p}","fee_split_bps":10001,"challenge_sizes":[],"max_payout_count":1,"reset_price_bps":[]}}"#
        ),
    ));
    assert_eq!(c, 1);
    assert!(h.err.contains("above 10,000"), "{}", h.err);
    let (c, h) = plan(&cfg(
        "c.json",
        format!(
            r#"{{"product_program_id":"{p}","fee_split_bps":1,"challenge_sizes":[],"max_payout_count":1,"reset_price_bps":[1,2,3,4,5,6,7,8,9]}}"#
        ),
    ));
    assert_eq!(c, 1);
    assert!(h.err.contains("9 reset phases"), "{}", h.err);
    let (c, h) = plan(&cfg("d.json", "{ nope".into()));
    assert_eq!(c, 1);
    assert!(h.err.contains("product config is not valid"), "{}", h.err);
    let (c, h) = plan(&cfg(
        "e.json",
        format!(
            r#"{{"product_program_id":"{p}","fee_split_bps":1,"challenge_sizes":[{{"size":5,"cost":0}}],"max_payout_count":0,"reset_price_bps":[20000]}}"#
        ),
    ));
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    for w_ in ["cost 0", "max_payout_count is 0", "above 100%"] {
        assert!(h.out.contains(w_), "missing warning '{w_}': {}", h.out);
    }
}

// ------------------------------------------------------------------ withdrawal pre-flight

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

fn wd(w: &World, side: &str, amount: &str, extra: &[&str]) -> (i32, TestHost) {
    let mut a = vec!["--pool", side, "--amount", amount, "--recent-blockhash"];
    a.extend_from_slice(extra);
    w.plan("admin-withdraw", "p.json", &a)
}

#[test]
fn plan_refuses_an_amount_above_what_the_25_percent_reserve_leaves() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let (c, h) = wd(&w, "usdc", "750", &[]);
    assert_eq!(c, 0, "exactly 75% is allowed: {}{}", h.out, h.err);
    assert!(
        h.out.contains("pre-flight: the USDC pool holds 1,000.000000, reserve 250.000000, at most 750.000000"),
        "{}",
        h.out
    );
    let (c, h) = wd(&w, "usdc", "750.000001", &[]);
    assert_eq!(c, 2, "one base unit more must be refused: {}{}", h.out, h.err);
    assert!(h.err.contains("at most 750.000000 can be withdrawn now, you asked for 750.000001"), "{}", h.err);
    // a stored floor above 25% binds
    set_vault(&w, |vs| vs.usdc_floor = 900 * M);
    let (c, _) = wd(&w, "usdc", "100.000001", &[]);
    assert_eq!(c, 2);
    let (c, h) = wd(&w, "usdc", "100", &[]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    // the rounded-UP reserve: 1,000.000001 keeps 250.000001
    w.fill_pool(&w.usdc, 1_000 * M + 1);
    set_vault(&w, |vs| vs.usdc_floor = 0);
    let (c, _) = wd(&w, "usdc", "750.000001", &[]);
    assert_eq!(c, 2);
    let (c, _) = wd(&w, "usdc", "750", &[]);
    assert_eq!(c, 0);
}

#[test]
fn plan_pre_flight_refusals() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    let (c, h) = wd(&w, "usdc", "0", &[]);
    assert_eq!((c, h.err.contains("the amount is zero")), (2, true), "{}{}", h.out, h.err);
    // wrong --mint
    let other = Pubkey::new_unique().to_string();
    let (c, h) = wd(&w, "usdc", "1", &["--mint", &other]);
    assert_eq!(c, 1);
    assert!(h.err.contains("is not the vault's USDC mint"), "{}", h.err);
    // the USDT mint passed for the USDC pool
    let (c, h) = wd(&w, "usdc", "1", &["--mint", &w.usdt.to_string()]);
    assert_eq!(c, 1, "{}", h.err);
    // frozen pool
    w.set_token_state(&w.pool(&w.usdc), &w.usdc, &w.keys.vault(), 1_000 * M, AccountState::Frozen);
    let (c, h) = wd(&w, "usdc", "1", &[]);
    assert_eq!(c, 2);
    assert!(h.err.contains("FROZEN by the issuer"), "{}", h.err);
    w.fill_pool(&w.usdc, 1_000 * M);
    // SL8 destination missing / frozen / wrong owner
    let dest = ata_address(&w.sl8.pubkey(), &w.usdc);
    w.set_token_state(&dest, &w.usdc, &w.sl8.pubkey(), 0, AccountState::Frozen);
    let (c, h) = wd(&w, "usdc", "1", &[]);
    assert_eq!(c, 2);
    assert!(h.err.contains("is frozen"), "{}", h.err);
    w.set_token(&dest, &w.usdc, &Pubkey::new_unique(), 0);
    let (c, h) = wd(&w, "usdc", "1", &[]);
    assert_eq!(c, 2);
    assert!(h.err.contains("not a USDC token account owned by the SL8 admin key"), "{}", h.err);
    w.rpc.svm.borrow_mut().set_account(dest, solana_account::Account::default()).unwrap();
    let (c, h) = wd(&w, "usdc", "1", &[]);
    assert_eq!(c, 2);
    assert!(h.err.contains("does not exist yet") && h.err.contains("DEPLOY-CHECKLIST"), "{}", h.err);
    // no vault at all
    let bare = World::bare();
    let (c, h) = bare.plan("admin-withdraw", "p.json", &["--pool", "usdc", "--amount", "1", "--recent-blockhash"]);
    assert_eq!(c, 1);
    assert!(h.err.contains("does not exist on this cluster"), "{}", h.err);
    // offline needs --mint and says there was no pre-flight
    let (c, h) = w.plan(
        "admin-withdraw",
        "p.json",
        &[
            "--pool",
            "usdc",
            "--amount",
            "1",
            "--recent-blockhash",
            &w.rpc.svm.borrow().latest_blockhash().to_string(),
            "--offline",
            "--genesis-hash",
            LOCAL_GENESIS,
        ],
    );
    assert_eq!(c, 1);
    assert!(h.err.contains("--offline needs --mint"), "{}", h.err);
}

#[test]
fn inspect_with_rpc_checks_the_mint_and_the_amount_against_the_chain() {
    let w = World::with_vault();
    w.fill_pool(&w.usdc, 1_000 * M);
    // a hand-built withdraw of 900 USDC (above the 750 limit) for the right mint
    let (c, _) = w.run(
        &[],
        &[
            "plan",
            "admin-withdraw",
            "--cluster",
            "localnet",
            "--out",
            &w.s(&w.path("ok.json")),
            "--pool",
            "usdc",
            "--amount",
            "700",
            "--recent-blockhash",
        ],
    );
    assert_eq!(c, 0);
    let (c, h) = w.inspect("ok.json", &["--rpc", "http://x"]);
    assert_eq!(c, 0, "{}", h.out);
    assert!(
        h.out.contains("the mint is the vault's USDC mint") && h.out.contains("at most 750.000000 out right now"),
        "{}",
        h.out
    );
    // now the pool drops, and the same file is no longer executable
    w.fill_pool(&w.usdc, 100 * M);
    let (c, h) = w.inspect("ok.json", &["--rpc", "http://x"]);
    assert_eq!(c, 2);
    assert!(h.out.contains("above what the 25% reserve leaves"), "{}", h.out);
    // offline inspection says so
    let (_, h) = w.inspect("ok.json", &[]);
    assert!(h.out.contains("OFFLINE inspection"), "{}", h.out);
}

#[test]
fn inspect_with_rpc_flags_a_mint_that_is_not_the_vaults() {
    let w = World::with_vault();
    use setl8_admin::admin_ix::{AdminIx, Side};
    use setl8_admin::message::{build_instructions, Parts};
    let evil_mint = Pubkey::new_unique();
    let p = Parts {
        fee_payer: w.keys.sl8,
        blockhash: w.rpc.svm.borrow().latest_blockhash(),
        nonce: None,
        cu_limit: None,
        cu_price: None,
        genesis: LOCAL_GENESIS.to_string(),
        admin: AdminIx::Withdraw { pool: Side::Usdc, amount: 5, mint: evil_mint },
    };
    let msg =
        solana_message::Message::new_with_blockhash(&build_instructions(&w.keys, &p), Some(&w.keys.sl8), &p.blockhash);
    let signers: Vec<Pubkey> = msg.account_keys.iter().take(2).copied().collect();
    let desc = setl8_admin::inspect::description_text(&p.admin);
    setl8_admin::txfile::TxFile::new(&msg.serialize(), "localnet", LOCAL_GENESIS, &desc, &signers, None)
        .save(&w.path("m.json"))
        .unwrap();
    let (c, h) = w.inspect("m.json", &[]);
    assert_eq!(c, 0, "offline it is a well-formed transaction (the mint cannot be known offline):\n{}", h.out);
    let (c, h) = w.inspect("m.json", &["--rpc", "http://x"]);
    assert_eq!(c, 2, "{}", h.out);
    assert!(h.out.contains("is not the vault's USDC mint"), "{}", h.out);
}

#[test]
fn inspect_with_rpc_knows_if_the_vault_or_product_already_exists() {
    let w = World::with_vault();
    let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
    let (c, _) = w.run(
        &[],
        &[
            "plan",
            "init-vault",
            "--cluster",
            "localnet",
            "--out",
            &w.s(&w.path("i.json")),
            "--usdc-mint",
            &usdc,
            "--usdt-mint",
            &usdt,
            "--recent-blockhash",
        ],
    );
    assert_eq!(c, 0);
    let (c, h) = w.inspect("i.json", &["--rpc", "http://x"]);
    assert_eq!(c, 2);
    assert!(h.out.contains("the vault already exists"), "{}", h.out);
    let product = Pubkey::new_unique();
    let cfg = w.s(&write_file(&w.dir, "p.json", &product_json(&product, 100)));
    let (c, _) = w.run(
        &[],
        &[
            "plan",
            "register-product",
            "--cluster",
            "localnet",
            "--out",
            &w.s(&w.path("r.json")),
            "--config",
            &cfg,
            "--recent-blockhash",
        ],
    );
    assert_eq!(c, 0);
    let (c, h) = w.inspect("r.json", &["--rpc", "http://x"]);
    assert_eq!(c, 0, "{}", h.out);
    assert!(h.out.contains("not registered yet"), "{}", h.out);
    let (c, _) = w.run(
        &[],
        &[
            "plan",
            "pause-product",
            "--cluster",
            "localnet",
            "--out",
            &w.s(&w.path("pa.json")),
            "--product",
            &product.to_string(),
            "--recent-blockhash",
        ],
    );
    assert_eq!(c, 0);
    let (c, h) = w.inspect("pa.json", &["--rpc", "http://x"]);
    assert_eq!(c, 2);
    assert!(h.out.contains("not registered"), "{}", h.out);
}

#[test]
fn nonce_commands_need_the_hash_confirmation_too() {
    let w = World::with_vault();
    let k = w.s(&w.sl8_key);
    for wrong in ["", "deadbeef", "x"] {
        let (c, h) = w.run(&[wrong], &["nonce-create", "--cluster", "localnet", "--keypair", &k]);
        assert_eq!(c, 2, "'{wrong}': {}{}", h.out, h.err);
        assert!(h.err.contains("not the start of the message hash"), "{}", h.err);
        assert_eq!(w.rpc.sends.get(), 0);
    }
    let nonce = Pubkey::new_unique();
    // a real nonce so that nonce-advance reaches the confirmation
    let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", "localnet", "--keypair", &k]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    let _ = nonce;
    let at = h.out.find("Nonce account: ").unwrap() + "Nonce account: ".len();
    let real = h.out[at..].split_whitespace().next().unwrap().to_string();
    w.expire_blockhash();
    let sends = w.rpc.sends.get();
    let (c, h) =
        w.run(&["nope"], &["nonce-advance", "--cluster", "localnet", "--keypair", &k, "--nonce-account", &real]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("not the start of the message hash"), "{}", h.err);
    assert_eq!(w.rpc.sends.get(), sends, "nothing was sent");
}
