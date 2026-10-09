//! Argument parsing and command dispatch. Everything goes through [`Host`], so the binary
//! is a few lines and the tests drive exactly the code the binary runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anchor_lang::prelude::Pubkey;
use solana_hash::Hash;
use solana_signature::Signature;

use crate::admin_ix::{Keys, ProductConfig, Side};
use crate::cluster::Cluster;
use crate::error::{Error, Result};
use crate::fmt::parse_amount;
use crate::host::Host;
use crate::inspect::inspect;
use crate::message::Expect;
use crate::nonce::{nonce_advance, nonce_create, NonceOpts};
use crate::plan::{plan, AdminSpec, Lifetime, PlanRequest};
use crate::send::{send_file, SendOpts};
use crate::sign::{add_signature, sign_file, SignOpts};
use crate::status::status;
use crate::txfile::TxFile;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    Val,
    Bool,
    Opt,
}

struct Parsed {
    pos: Vec<String>,
    vals: HashMap<String, Option<String>>,
}

impl Parsed {
    fn get(&self, n: &str) -> Option<&str> {
        self.vals.get(n).and_then(|v| v.as_deref())
    }
    fn has(&self, n: &str) -> bool {
        self.vals.contains_key(n)
    }
    fn need(&self, n: &str) -> Result<&str> {
        self.get(n).ok_or_else(|| Error(format!("--{n} is required")))
    }
}

fn parse(cmd: &str, args: &[String], spec: &[(&str, K)]) -> Result<Parsed> {
    let mut p = Parsed { pos: vec![], vals: HashMap::new() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let kind = spec.iter().find(|(n, _)| *n == name).map(|(_, k)| *k).ok_or_else(|| {
                Error(format!("unknown option --{name} for `{cmd}` (see `setl8-admin {cmd} --help`)"))
            })?;
            if p.vals.contains_key(name) {
                return Err(Error(format!("--{name} given twice")));
            }
            let val = match kind {
                K::Bool => {
                    if inline.is_some() {
                        return Err(Error(format!("--{name} takes no value")));
                    }
                    None
                }
                K::Val => match inline {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        Some(
                            args.get(i)
                                .filter(|v| !v.starts_with("--"))
                                .ok_or_else(|| Error(format!("--{name} needs a value")))?
                                .clone(),
                        )
                    }
                },
                K::Opt => match inline {
                    Some(v) => Some(v),
                    None => match args.get(i + 1).filter(|v| !v.starts_with("--")) {
                        Some(v) => {
                            i += 1;
                            Some(v.clone())
                        }
                        None => None,
                    },
                },
            };
            p.vals.insert(name.to_string(), val);
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(Error(format!("unknown option {a} for `{cmd}`")));
        } else {
            p.pos.push(a.clone());
        }
        i += 1;
    }
    Ok(p)
}

fn pk(name: &str, s: &str) -> Result<Pubkey> {
    s.parse().map_err(|_| Error(format!("--{name}: '{s}' is not a valid public key")))
}

fn keys_of(p: &Parsed) -> Result<Keys> {
    let mut k = Keys::compiled();
    if let Some(id) = p.get("program-id") {
        k = k.with_program(pk("program-id", id)?);
    }
    Ok(k)
}

fn expect_of(p: &Parsed, k: &Keys) -> Result<Expect> {
    Ok(Expect {
        fee_payer: match p.get("fee-payer") {
            Some(f) => pk("fee-payer", f)?,
            None => k.sl8,
        },
        nonce: p.get("nonce-account").map(|n| pk("nonce-account", n)).transpose()?,
    })
}

fn cluster_of(p: &Parsed) -> Result<Cluster> {
    Cluster::parse(p.need("cluster")?)
}

fn rpc_url_of(p: &Parsed, cluster: &Cluster) -> String {
    p.get("rpc").map(|s| s.to_string()).unwrap_or_else(|| cluster.default_rpc_url())
}

const USAGE: &str = "\
setl8-admin: signing ceremony tool for the core-vault 2-of-2 admin instructions

USAGE: setl8-admin <command> [options]

COMMANDS (offline unless marked RPC)
  plan <instruction> ...   build the UNSIGNED transaction file (RPC for nonce/pre-flight unless --offline)
  inspect <tx.json>        decode the file from its message bytes alone and show what it does
  sign <tx.json>           inspect, confirm by retyping the hash prefix, add ONE signature
  add-signature <tx.json>  attach a signature produced elsewhere (verified first)
  send <tx.json>           (RPC) check every signature, simulate, send, wait
  status                   (RPC, read-only) the vault, pools, reserve, claims, cycle, products
  nonce-create             (RPC) create the durable-nonce account the transactions use
  nonce-advance            (RPC) invalidate every outstanding signed transaction on a nonce
  help, version

INSTRUCTIONS for plan
  init-vault | register-product | update-product-config | pause-product |
  reactivate-product | admin-withdraw

Run `setl8-admin <command> --help` for a command's options.
The default build embeds the REAL admin public keys; a build with --features localnet embeds
the PUBLIC TEST keys and is for tests only.
";

const PLAN_HELP: &str = "\
setl8-admin plan <instruction> --cluster <devnet|mainnet|localnet|URL> --out tx.json [options]

INSTRUCTIONS AND THEIR ARGUMENTS
  init-vault               --usdc-mint <pk> --usdt-mint <pk>
  register-product         --config product.json
  update-product-config    --config product.json
  pause-product            --product <program id>
  reactivate-product       --product <program id>
  admin-withdraw           --pool <usdc|usdt> --amount <e.g. 1250.50> [--mint <pk>]
                           (alias: admin-withdraw-marketing-funds)

TRANSACTION LIFETIME (one is required)
  --nonce-account <pk>     use this durable nonce (recommended: the signers may be hours apart)
    --nonce-authority <pk>   its authority (default: the SL8 admin key)
    --nonce-blockhash <h>    the nonce's current value, for --offline
  --recent-blockhash [h]   a ~60-90 second blockhash, same-session use only (fetched if no value)

OTHER OPTIONS
  --fee-payer <pk>         default: the SL8 admin key (so exactly two signatures are needed)
  --compute-unit-limit <n> --priority-fee-microlamports <n>
  --rpc <url>              default: the cluster's public endpoint
  --offline                no network at all (needs --genesis-hash for localnet/URL clusters)
  --genesis-hash <hash>    for localnet / custom clusters
  --program-id <pk>        override the compiled-in program id (must then be given on every machine)

product.json: {\"product_program_id\":\"<pk>\",\"fee_split_bps\":6500,
  \"challenge_sizes\":[{\"size\":10000000000,\"cost\":100000000}],
  \"max_payout_count\":5,\"reset_price_bps\":[100,150]}
";

const INSPECT_HELP: &str = "\
setl8-admin inspect <tx.json> [--fee-payer <pk>] [--nonce-account <pk>] [--program-id <pk>] [--rpc <url>]

Decodes the transaction from its message bytes ONLY and prints the cluster, program, instruction,
arguments, accounts, fee payer, vault PDA, signatures and the message hash. Exit code 0 only if
the transaction passes the allowlist and the file's metadata agrees with the message.
  --fee-payer      the fee payer YOU expect (default: the SL8 admin key)
  --nonce-account  the durable nonce YOU expect the message to advance
  --rpc            also check the node's genesis hash and the vault's own records (otherwise offline)
";

const SIGN_HELP: &str = "\
setl8-admin sign <tx.json> --keypair <file> [--out <file>] [--fee-payer <pk>] [--nonce-account <pk>]
                 [--program-id <pk>] [--allow-loose-perms]

Runs `inspect`, refuses anything that fails it, requires the key to be a required signer, shows the
message hash and asks you to retype its first 8 characters. Adds that one signature and nothing else.
The key file is only read; it must not be readable by group/others (chmod 600).
";

const ADD_SIG_HELP: &str = "\
setl8-admin add-signature <tx.json> --pubkey <pk> --signature <base58> [--out <file>]
                 [--fee-payer <pk>] [--nonce-account <pk>] [--program-id <pk>]

Attaches a signature produced elsewhere (a hardware wallet, another tool) after verifying it
against the message and the public key.
";

const SEND_HELP: &str = "\
setl8-admin send <tx.json> --cluster <devnet|mainnet|localnet|URL> [--rpc <url>]
                 [--i-understand-this-is-mainnet] [--fee-payer <pk>] [--nonce-account <pk>] [--program-id <pk>]

Refuses unless every required signature is present and valid, checks the node's genesis hash equals
the one inside the message, simulates, sends and waits. A mainnet-bound transaction also needs
--i-understand-this-is-mainnet and typing MAINNET.
";

const STATUS_HELP: &str = "\
setl8-admin status --cluster <devnet|mainnet|localnet|URL> [--rpc <url>] [--program-id <pk>]

Read-only. Run it before and after every admin action.
";

const NONCE_HELP: &str = "\
setl8-admin nonce-create  --cluster <c> --keypair <file> [--rpc <url>] [--allow-loose-perms] [--i-understand-this-is-mainnet]
setl8-admin nonce-advance --cluster <c> --keypair <file> --nonce-account <pk> [--rpc <url>] [--allow-loose-perms] [--i-understand-this-is-mainnet]

nonce-create makes a rent-exempt nonce account whose authority is the key you give (use the SL8 admin
key). Its own key is generated in memory, used once and discarded; nothing secret is written.
nonce-advance invalidates every transaction already signed against the nonce's current value: this is
how you revoke a signed-but-unsent authorisation.
";

fn plan_cmd(host: &mut dyn Host, args: &[String]) -> Result<()> {
    let spec = [
        ("cluster", K::Val),
        ("out", K::Val),
        ("config", K::Val),
        ("usdc-mint", K::Val),
        ("usdt-mint", K::Val),
        ("product", K::Val),
        ("pool", K::Val),
        ("amount", K::Val),
        ("mint", K::Val),
        ("nonce-account", K::Val),
        ("nonce-authority", K::Val),
        ("nonce-blockhash", K::Val),
        ("recent-blockhash", K::Opt),
        ("fee-payer", K::Val),
        ("compute-unit-limit", K::Val),
        ("priority-fee-microlamports", K::Val),
        ("rpc", K::Val),
        ("offline", K::Bool),
        ("genesis-hash", K::Val),
        ("program-id", K::Val),
    ];
    let p = parse("plan", args, &spec)?;
    let name =
        p.pos.first().ok_or_else(|| Error("plan needs an instruction name (see `setl8-admin plan --help`)".into()))?;
    if p.pos.len() > 1 {
        return Err(Error(format!("unexpected argument '{}'", p.pos[1])));
    }
    let keys = keys_of(&p)?;
    let cluster = cluster_of(&p)?;
    let out = PathBuf::from(p.need("out")?);
    let spec = match name.replace('_', "-").as_str() {
        "init-vault" => AdminSpec::InitVault {
            usdc_mint: pk("usdc-mint", p.need("usdc-mint")?)?,
            usdt_mint: pk("usdt-mint", p.need("usdt-mint")?)?,
        },
        n @ ("register-product" | "update-product-config") => {
            let path = p.need("config")?;
            let text = std::fs::read_to_string(path).map_err(|e| Error(format!("cannot read {path}: {}", e.kind())))?;
            let (prod, warnings) = ProductConfig::parse(&text)?.validate()?;
            for w in warnings {
                host.out(&format!("warning: {w}\n"));
            }
            if n == "register-product" {
                AdminSpec::Register(prod)
            } else {
                AdminSpec::Update(prod)
            }
        }
        "pause-product" => AdminSpec::Pause(pk("product", p.need("product")?)?),
        "reactivate-product" => AdminSpec::Reactivate(pk("product", p.need("product")?)?),
        "admin-withdraw" | "admin-withdraw-marketing-funds" => AdminSpec::Withdraw {
            pool: Side::parse(p.need("pool")?)?,
            amount: parse_amount(p.need("amount")?)?,
            mint: p.get("mint").map(|m| pk("mint", m)).transpose()?,
        },
        other => return Err(Error(format!("unknown instruction '{other}' (see `setl8-admin plan --help`)"))),
    };
    let fee_payer = match p.get("fee-payer") {
        Some(f) => pk("fee-payer", f)?,
        None => keys.sl8,
    };
    let lifetime = match (p.get("nonce-account"), p.has("recent-blockhash")) {
        (Some(n), false) => Lifetime::Nonce {
            account: pk("nonce-account", n)?,
            authority: match p.get("nonce-authority") {
                Some(a) => pk("nonce-authority", a)?,
                None => keys.sl8,
            },
            blockhash: p.get("nonce-blockhash").map(|h| h.parse::<Hash>().map_err(|_| Error("--nonce-blockhash is not a valid hash".into()))).transpose()?,
        },
        (None, true) => Lifetime::Recent(
            p.get("recent-blockhash").map(|h| h.parse::<Hash>().map_err(|_| Error("--recent-blockhash is not a valid hash".into()))).transpose()?,
        ),
        (Some(_), true) => return Err(Error("give --nonce-account OR --recent-blockhash, not both".into())),
        (None, false) => {
            return Err(Error("choose a transaction lifetime: --nonce-account <pk> (recommended) or --recent-blockhash (same-session only)".into()))
        }
    };
    let req = PlanRequest {
        keys,
        rpc_url: rpc_url_of(&p, &cluster),
        cluster,
        offline: p.has("offline"),
        genesis: p.get("genesis-hash").map(|s| s.to_string()),
        spec,
        fee_payer,
        lifetime,
        cu_limit: p
            .get("compute-unit-limit")
            .map(|v| v.parse::<u32>().map_err(|_| Error("--compute-unit-limit is not a number".into())))
            .transpose()?,
        cu_price: p
            .get("priority-fee-microlamports")
            .map(|v| v.parse::<u64>().map_err(|_| Error("--priority-fee-microlamports is not a number".into())))
            .transpose()?,
    };
    plan(host, &req, &out)?;
    Ok(())
}

fn inspect_cmd(host: &mut dyn Host, args: &[String]) -> Result<i32> {
    let spec = [("fee-payer", K::Val), ("nonce-account", K::Val), ("program-id", K::Val), ("rpc", K::Val)];
    let p = parse("inspect", args, &spec)?;
    let [path] = p.pos.as_slice() else { return Err(Error("inspect needs exactly one transaction file".into())) };
    let keys = keys_of(&p)?;
    let expect = expect_of(&p, &keys)?;
    let file = TxFile::load(Path::new(path))?;
    let rpc = match p.get("rpc") {
        Some(u) => Some(host.rpc(u)?),
        None => None,
    };
    let insp = inspect(&file, &keys, &expect, rpc.as_deref())?;
    host.out(&insp.render());
    if !insp.mismatches.is_empty() {
        host.err("error: the file's metadata disagrees with its message bytes\n");
    }
    Ok(if insp.is_clean() { 0 } else { 2 })
}

fn sign_cmd(host: &mut dyn Host, args: &[String]) -> Result<()> {
    let spec = [
        ("keypair", K::Val),
        ("out", K::Val),
        ("fee-payer", K::Val),
        ("nonce-account", K::Val),
        ("program-id", K::Val),
        ("allow-loose-perms", K::Bool),
    ];
    let p = parse("sign", args, &spec)?;
    let [path] = p.pos.as_slice() else { return Err(Error("sign needs exactly one transaction file".into())) };
    let keys = keys_of(&p)?;
    let opts = SignOpts { expect: expect_of(&p, &keys)?, keys, allow_loose_perms: p.has("allow-loose-perms") };
    let out = p.get("out").unwrap_or(path);
    sign_file(host, Path::new(path), Path::new(out), Path::new(p.need("keypair")?), &opts)
}

fn add_sig_cmd(host: &mut dyn Host, args: &[String]) -> Result<()> {
    let spec = [
        ("pubkey", K::Val),
        ("signature", K::Val),
        ("out", K::Val),
        ("fee-payer", K::Val),
        ("nonce-account", K::Val),
        ("program-id", K::Val),
    ];
    let p = parse("add-signature", args, &spec)?;
    let [path] = p.pos.as_slice() else { return Err(Error("add-signature needs exactly one transaction file".into())) };
    let keys = keys_of(&p)?;
    let opts = SignOpts { expect: expect_of(&p, &keys)?, keys, allow_loose_perms: false };
    let key = pk("pubkey", p.need("pubkey")?)?;
    let sig: Signature =
        p.need("signature")?.parse().map_err(|_| Error("--signature is not a valid base58 signature".into()))?;
    let out = p.get("out").unwrap_or(path);
    add_signature(host, Path::new(path), Path::new(out), &key, &sig, &opts)
}

fn send_cmd(host: &mut dyn Host, args: &[String]) -> Result<()> {
    let spec = [
        ("cluster", K::Val),
        ("rpc", K::Val),
        ("i-understand-this-is-mainnet", K::Bool),
        ("fee-payer", K::Val),
        ("nonce-account", K::Val),
        ("program-id", K::Val),
    ];
    let p = parse("send", args, &spec)?;
    let [path] = p.pos.as_slice() else { return Err(Error("send needs exactly one transaction file".into())) };
    let keys = keys_of(&p)?;
    let cluster = cluster_of(&p)?;
    let opts = SendOpts {
        expect: expect_of(&p, &keys)?,
        keys,
        rpc_url: rpc_url_of(&p, &cluster),
        mainnet_flag: p.has("i-understand-this-is-mainnet"),
    };
    send_file(host, Path::new(path), &opts).map(|_| ())
}

fn status_cmd(host: &mut dyn Host, args: &[String]) -> Result<()> {
    let spec = [("cluster", K::Val), ("rpc", K::Val), ("program-id", K::Val)];
    let p = parse("status", args, &spec)?;
    if !p.pos.is_empty() {
        return Err(Error(format!("unexpected argument '{}'", p.pos[0])));
    }
    let keys = keys_of(&p)?;
    let cluster = cluster_of(&p)?;
    let rpc = host.rpc(&rpc_url_of(&p, &cluster))?;
    let text = status(rpc.as_ref(), &keys)?;
    host.out(&text);
    Ok(())
}

fn nonce_cmd(host: &mut dyn Host, name: &str, args: &[String]) -> Result<()> {
    let spec = [
        ("cluster", K::Val),
        ("rpc", K::Val),
        ("keypair", K::Val),
        ("nonce-account", K::Val),
        ("allow-loose-perms", K::Bool),
        ("i-understand-this-is-mainnet", K::Bool),
    ];
    let p = parse(name, args, &spec)?;
    if !p.pos.is_empty() {
        return Err(Error(format!("unexpected argument '{}'", p.pos[0])));
    }
    let cluster = cluster_of(&p)?;
    let key_path = PathBuf::from(p.need("keypair")?);
    let o = NonceOpts {
        rpc_url: rpc_url_of(&p, &cluster),
        cluster,
        key_path: &key_path,
        allow_loose_perms: p.has("allow-loose-perms"),
        mainnet_flag: p.has("i-understand-this-is-mainnet"),
    };
    if name == "nonce-create" {
        nonce_create(host, &o).map(|_| ())
    } else {
        nonce_advance(host, &o, &pk("nonce-account", p.need("nonce-account")?)?)
    }
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// Runs one command line (without the program name). Returns the process exit code:
/// 0 ok, 1 error, 2 refused for safety, 70 internal error.
pub fn run(host: &mut dyn Host, args: &[String]) -> i32 {
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        host.out(USAGE);
        return 1;
    };
    let rest = &args[1..];
    let result: Result<i32> = (|| {
        if wants_help(rest) {
            host.out(match cmd {
                "plan" => PLAN_HELP,
                "inspect" => INSPECT_HELP,
                "sign" => SIGN_HELP,
                "add-signature" => ADD_SIG_HELP,
                "send" => SEND_HELP,
                "status" => STATUS_HELP,
                "nonce-create" | "nonce-advance" => NONCE_HELP,
                _ => USAGE,
            });
            return Ok(0);
        }
        match cmd {
            "help" | "--help" | "-h" => {
                host.out(USAGE);
                Ok(0)
            }
            "version" | "--version" | "-V" => {
                host.out(&format!(
                    "setl8-admin {VERSION} ({} admin keys)\n",
                    if cfg!(feature = "localnet") { "PUBLIC TEST" } else { "REAL" }
                ));
                Ok(0)
            }
            "plan" => plan_cmd(host, rest).map(|_| 0),
            "inspect" => inspect_cmd(host, rest),
            "sign" => sign_cmd(host, rest).map(|_| 0),
            "add-signature" => add_sig_cmd(host, rest).map(|_| 0),
            "send" => send_cmd(host, rest).map(|_| 0),
            "status" => status_cmd(host, rest).map(|_| 0),
            "nonce-create" | "nonce-advance" => nonce_cmd(host, cmd, rest).map(|_| 0),
            other => Err(Error(format!("unknown command '{other}' (try `setl8-admin help`)"))),
        }
    })();
    match result {
        Ok(code) => code,
        Err(e) => {
            host.err(&format!("error: {}\n", e.0));
            if e.0.starts_with("refused:") {
                2
            } else {
                1
            }
        }
    }
}

/// Like [`run`] but a panic becomes a fixed message: a panic payload can contain anything,
/// including data that came from a key file.
pub fn run_guarded(host: &mut dyn Host, args: &[String]) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(host, args))) {
        Ok(c) => c,
        Err(_) => {
            host.err("internal error: the tool stopped unexpectedly (details are withheld so no secret can leak). Nothing was sent.\n");
            70
        }
    }
}

/// Replaces the default panic message (which prints the payload) with the location only.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "unknown".into());
        eprintln!("internal error at {loc} (message withheld)");
    }));
}
