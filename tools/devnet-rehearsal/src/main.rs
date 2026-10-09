//! devnet-rehearsal: runs the whole core-vault protocol on a real cluster and records the results.
//! TEST SCAFFOLDING. The wrapper script scripts/devnet-rehearsal.sh pins the cluster to devnet;
//! this binary refuses any URL that contains "mainnet" and any node whose genesis hash differs
//! from --expect-genesis.

mod chain;
mod jrpc;
mod sector;
mod steps;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use setl8_admin::admin_ix::Keys;

use chain::Ctx;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let need = |n: &str| arg(&args, n).unwrap_or_else(|| die(&format!("{n} is required")));
    let url = need("--rpc-url");
    if url.to_lowercase().contains("mainnet") {
        die("refusing: the URL contains 'mainnet'");
    }
    let cluster = need("--cluster");
    if cluster != "devnet" && cluster != "localnet" {
        die("--cluster must be devnet (or localnet for the local self-test)");
    }
    let genesis = need("--expect-genesis");
    if genesis == setl8_admin::constants::MAINNET_GENESIS {
        die("refusing: that is the mainnet genesis hash");
    }
    let dir: PathBuf = need("--keys-dir").into();
    let work: PathBuf = arg(&args, "--work-dir").map(Into::into).unwrap_or_else(|| dir.join("rehearsal-work"));
    std::fs::create_dir_all(&work).unwrap_or_else(|_| die("cannot create the work dir"));
    let vault: Pubkey = need("--vault-program").parse().unwrap_or_else(|_| die("bad --vault-program"));
    let sector: Pubkey = need("--sector-program").parse().unwrap_or_else(|_| die("bad --sector-program"));

    let mut kp = HashMap::new();
    for n in [
        "sl8-test",
        "rov-test",
        "keeper",
        "usdc-mint",
        "usdt-mint",
        "freeze-authority",
        "trader1",
        "trader2",
        "trader3",
        "bonder1",
        "bonder2",
    ] {
        let l = setl8_admin::keyfile::load_keypair(&dir.join(format!("{n}.json")), false)
            .unwrap_or_else(|e| die(&format!("{n}: {e}")));
        kp.insert(n.to_string(), l.keypair);
    }
    use solana_signer::Signer;
    let usdc = kp["usdc-mint"].pubkey();
    let usdt = kp["usdt-mint"].pubkey();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let log_path = work.join(format!("rehearsal-{stamp}.jsonl"));
    let jsonl = std::fs::File::create(&log_path).unwrap_or_else(|_| die("cannot create the log"));
    let mut c = Ctx {
        rpc: jrpc::Jrpc::new(&url),
        url: url.clone(),
        cluster,
        genesis,
        dir,
        work: work.clone(),
        keys: Keys::compiled().with_program(vault),
        sector,
        usdc,
        usdt,
        kp,
        rows: vec![],
        jsonl,
        nonce: None,
        cus: vec![],
    };
    println!(
        "cluster {} | vault program {vault} | sector program {sector} | vault PDA {} | usdc {usdc} | usdt {usdt}",
        c.cluster,
        c.keys.vault()
    );
    println!("log: {}", log_path.display());

    type Step = fn(&mut Ctx) -> setl8_admin::error::Result<()>;
    let all: Vec<(&str, Step)> = vec![
        ("setup", steps::setup),
        ("nonce+init", steps::nonce_and_init),
        ("register", steps::register),
        ("fees", steps::fees),
        ("bonds", steps::bonds),
        ("payouts", steps::payouts),
        ("reconcile", steps::reconcile),
        ("heartbeat", steps::heartbeat),
        ("withdraw", steps::withdraw),
        ("revoke", steps::revoke),
        ("tool-safety", steps::tool_safety),
    ];
    let only = arg(&args, "--only");
    for (name, f) in all {
        if only.as_deref().map(|o| o != name).unwrap_or(false) {
            continue;
        }
        println!("\n===== {name} =====");
        if let Err(e) = f(&mut c) {
            c.record("ERR", name, "completes", &e.to_string(), false, None);
            break;
        }
    }
    let md = format!(
        "{}\n\n{}\n\nFinal status:\n```\n{}```\n",
        steps::results_markdown(&c),
        steps::cu_table(&c),
        steps::status_text(&c)
    );
    let md_path = work.join(format!("rehearsal-{stamp}.md"));
    std::fs::File::create(&md_path)
        .and_then(|mut f| f.write_all(md.as_bytes()))
        .unwrap_or_else(|_| die("cannot write the results"));
    let fails = c.rows.iter().filter(|r| !r.ok).count();
    println!("\n{} checks, {} failed. Results: {}", c.rows.len(), fails, md_path.display());
    std::process::exit(if fails == 0 { 0 } else { 1 });
}

fn die(m: &str) -> ! {
    eprintln!("error: {m}");
    std::process::exit(2);
}
