//! Command line: argument parsing, the start-up refusals, and the four modes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anchor_lang::prelude::Pubkey;
use setl8_admin::admin_ix::Keys;
use setl8_admin::cluster::Cluster;
use solana_signer::Signer;

use crate::chain::{Chain, HttpChain};
use crate::config::Config;
use crate::log::{HttpNotifier, Logger, Notifier};
use crate::runner::{Keeper, NoSleep, PassReport, RealSleeper, Sleeper};
use crate::safety::{check_mainnet_gate, refuse_admin_key};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const WEBHOOK_ENV: &str = "SETL8_KEEPER_WEBHOOK";

const USAGE: &str = "\
setl8-keeper: keeps the core-vault payout cycle running. It calls only permissionless instructions
(reconcile_product, begin_heartbeat, settle_claims, finalize_heartbeat), holds no authority over funds,
and any number of keepers may run at once.

USAGE: setl8-keeper <mode> --cluster <devnet|mainnet|localnet|URL> [options]

MODES
  run       loop forever: one pass every --interval seconds
  once      one pass, then exit: 0 nothing to do or progress made, 10 an alert condition is present,
            20 hard failure (cron friendly)
  dry-run   print exactly what would be sent, send nothing (needs no funded key: --keypair or --fee-payer-pubkey)
  status    read-only health summary (JSON)

OPTIONS
  --cluster <c>                  required; the node's genesis hash must match (localnet: must not be a public cluster)
  --rpc <url>                    default: the cluster's public endpoint
  --keypair <file>               the fee payer (a fresh hot key; the ADMIN keys are refused); mode 600
  --fee-payer-pubkey <pk>        dry-run / status without a key file
  --program-id <pk>              override the compiled-in program id
  --interval <secs>              run: pause between passes (default 60)
  --priority-fee-microlamports N default 0 (then no ComputeBudget instruction is sent)
  --max-sends-per-run N          default 100        --max-fee-lamports-per-run N   default 100000000
  --min-balance-lamports N       fee-payer alert threshold (default 50000000)
  --max-retries N                transient failures (default 4)
  --claims-file <file>           claim addresses, one per line, for providers without getProgramAccounts
  --log-file <file>              append the JSON log lines
  --webhook <url>                POST alerts (prefer the SETL8_KEEPER_WEBHOOK environment variable: argv is visible)
  --lock-file <file>             stop two local copies from fighting (not needed for correctness)
  --i-understand-this-is-mainnet required for --cluster mainnet
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    Val,
    Bool,
}

const FLAGS: &[(&str, K)] = &[
    ("cluster", K::Val),
    ("rpc", K::Val),
    ("keypair", K::Val),
    ("fee-payer-pubkey", K::Val),
    ("program-id", K::Val),
    ("interval", K::Val),
    ("priority-fee-microlamports", K::Val),
    ("max-sends-per-run", K::Val),
    ("max-fee-lamports-per-run", K::Val),
    ("min-balance-lamports", K::Val),
    ("max-retries", K::Val),
    ("claims-file", K::Val),
    ("log-file", K::Val),
    ("webhook", K::Val),
    ("lock-file", K::Val),
    ("i-understand-this-is-mainnet", K::Bool),
];

fn parse(args: &[String]) -> Result<HashMap<String, Option<String>>, String> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let Some(rest) = a.strip_prefix("--") else { return Err(format!("unexpected argument '{a}'")) };
        let (name, inline) = match rest.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (rest, None),
        };
        let kind = FLAGS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, k)| *k)
            .ok_or_else(|| format!("unknown option --{name}"))?;
        if m.contains_key(name) {
            return Err(format!("--{name} given twice"));
        }
        let val = match kind {
            K::Bool => {
                if inline.is_some() {
                    return Err(format!("--{name} takes no value"));
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
                            .ok_or_else(|| format!("--{name} needs a value"))?
                            .clone(),
                    )
                }
            },
        };
        m.insert(name.to_string(), val);
        i += 1;
    }
    Ok(m)
}

/// What the command line needs from the outside, so tests can substitute all of it.
pub type ChainFactory = Box<dyn Fn(&str, &Pubkey) -> Box<dyn Chain>>;
pub type NotifierFactory = Box<dyn Fn(&str) -> Box<dyn Notifier>>;
pub type SleeperFactory = Box<dyn Fn() -> Box<dyn Sleeper>>;

pub struct Deps {
    pub make_chain: ChainFactory,
    pub make_notifier: NotifierFactory,
    pub sleeper: SleeperFactory,
    /// Captures every log line (tests); the real binary prints to stdout.
    pub capture: Option<Rc<RefCell<Vec<String>>>>,
    pub stdout_log: bool,
    /// `run`: stop after this many passes (tests). `None` = forever.
    pub max_passes: Option<u32>,
    /// The `SETL8_KEEPER_WEBHOOK` value, injected so tests do not touch the environment.
    pub webhook_env: Option<String>,
    /// Messages for the human (errors, usage); the real binary prints them to stderr.
    pub err: Rc<RefCell<Vec<String>>>,
    pub print_err: bool,
}

impl Deps {
    pub fn real() -> Deps {
        Deps {
            make_chain: Box::new(|url, program| Box::new(HttpChain::new(url, *program))),
            make_notifier: Box::new(|url| Box::new(HttpNotifier::new(url))),
            sleeper: Box::new(|| Box::new(RealSleeper)),
            capture: None,
            stdout_log: true,
            max_passes: None,
            webhook_env: std::env::var(WEBHOOK_ENV).ok().filter(|s| !s.is_empty()),
            err: Rc::new(RefCell::new(vec![])),
            print_err: true,
        }
    }

    fn say(&self, s: &str) {
        self.err.borrow_mut().push(s.to_string());
        if self.print_err {
            eprintln!("{s}");
        }
    }

    /// Drop all pauses (tests).
    pub fn no_sleep(mut self) -> Deps {
        self.sleeper = Box::new(|| Box::new(NoSleep));
        self
    }
}

fn num<T: std::str::FromStr>(m: &HashMap<String, Option<String>>, name: &str) -> Result<Option<T>, String> {
    match m.get(name).and_then(|v| v.as_deref()) {
        None => Ok(None),
        Some(s) => {
            s.replace('_', "").parse::<T>().map(Some).map_err(|_| format!("--{name}: '{s}' is not a valid number"))
        }
    }
}

/// An exclusive lock file, removed on drop. Not used for correctness (state is on chain).
struct LockFile(PathBuf);
impl LockFile {
    fn take(p: &Path) -> Result<LockFile, String> {
        use std::io::Write;
        let mut f =
            std::fs::OpenOptions::new().write(true).create_new(true).open(p).map_err(|_| {
                format!("another keeper holds the lock file {} (remove it if that is stale)", p.display())
            })?;
        let _ = write!(f, "{}", std::process::id());
        Ok(LockFile(p.to_path_buf()))
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// Runs the command line (without the program name). Exit codes: 0 / 10 / 20 as documented, 1 usage, 2 refused.
pub fn execute(args: &[String], deps: &Deps) -> i32 {
    let Some(mode) = args.first().map(|s| s.as_str()) else {
        eprintln!("{USAGE}");
        return 1;
    };
    if matches!(mode, "help" | "--help" | "-h") {
        println!("{USAGE}");
        return 0;
    }
    if matches!(mode, "version" | "--version" | "-V") {
        println!(
            "setl8-keeper {VERSION} ({} admin keys compiled in)",
            if cfg!(feature = "localnet") { "PUBLIC TEST" } else { "REAL" }
        );
        return 0;
    }
    if !matches!(mode, "run" | "once" | "dry-run" | "status") {
        deps.say(&format!("error: unknown mode '{mode}' (run, once, dry-run, status)"));
        return 1;
    }
    if args[1..].iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let m = match parse(&args[1..]) {
        Ok(m) => m,
        Err(e) => {
            deps.say(&format!("error: {e}"));
            return 1;
        }
    };
    match start(mode, &m, deps) {
        Ok(code) => code,
        Err((code, msg)) => {
            deps.say(&format!("error: {msg}"));
            code
        }
    }
}

type StartErr = (i32, String);

fn usage<T>(m: impl Into<String>) -> Result<T, StartErr> {
    Err((1, m.into()))
}
fn refuse<T>(m: impl Into<String>) -> Result<T, StartErr> {
    Err((2, m.into()))
}

fn start(mode: &str, m: &HashMap<String, Option<String>>, deps: &Deps) -> Result<i32, StartErr> {
    let get = |n: &str| m.get(n).and_then(|v| v.clone());
    let cluster =
        Cluster::parse(&get("cluster").ok_or((1, "--cluster is required".to_string()))?).map_err(|e| (1, e.0))?;
    check_mainnet_gate(&cluster, m.contains_key("i-understand-this-is-mainnet")).map_err(|e| (2, e.0))?;

    let mut keys = Keys::compiled();
    if let Some(p) = get("program-id") {
        keys = keys.with_program(p.parse().map_err(|_| (1, "--program-id is not a valid public key".to_string()))?);
    }

    let dry = mode == "dry-run";
    let readonly = dry || mode == "status";
    // ---- the fee-payer key: never an admin key, never readable by others
    let (signer, payer) = match (get("keypair"), get("fee-payer-pubkey")) {
        (Some(path), _) => {
            let loaded = setl8_admin::keyfile::load_keypair(Path::new(&path), false)
                .map_err(|e| (if e.0.starts_with("refused") { 2 } else { 1 }, e.0))?;
            let pk = loaded.keypair.pubkey();
            refuse_admin_key(&pk, &keys).map_err(|e| (2, e.0))?;
            (if readonly { None } else { Some(loaded.keypair) }, pk)
        }
        (None, Some(pk)) if readonly => {
            let pk: Pubkey = pk.parse().map_err(|_| (1, "--fee-payer-pubkey is not a valid public key".to_string()))?;
            refuse_admin_key(&pk, &keys).map_err(|e| (2, e.0))?;
            (None, pk)
        }
        (None, Some(_)) => return usage("--fee-payer-pubkey is for dry-run and status only; run/once need --keypair"),
        (None, None) => {
            return usage(if readonly { "give --keypair or --fee-payer-pubkey" } else { "--keypair is required" })
        }
    };

    let url = get("rpc").unwrap_or_else(|| cluster.default_rpc_url());
    let chain = (deps.make_chain)(&url, &keys.program_id);

    let mut cfg = Config {
        dry_run: dry,
        claims_file: get("claims_file").or_else(|| get("claims-file")).map(PathBuf::from),
        ..Config::default()
    };
    macro_rules! n {
        ($flag:literal, $field:ident, $t:ty) => {
            if let Some(v) = num::<$t>(m, $flag).map_err(|e| (1, e))? {
                cfg.$field = v;
            }
        };
    }
    n!("priority-fee-microlamports", priority_fee_micro, u64);
    n!("max-sends-per-run", max_sends, u32);
    n!("max-fee-lamports-per-run", max_fee_lamports, u64);
    n!("min-balance-lamports", min_payer_balance, u64);
    n!("max-retries", max_retries, u32);
    let interval: u64 = num(m, "interval").map_err(|e| (1, e))?.unwrap_or(60);

    let mut log = Logger::new();
    // `status` prints one JSON document and nothing else on stdout (log lines still go to --log-file)
    log.stdout = deps.stdout_log && mode != "status";
    log.capture = deps.capture.clone();
    log.file = get("log-file").map(PathBuf::from);
    if let Some(w) = get("webhook").or_else(|| deps.webhook_env.clone()) {
        log.notifier = Some((deps.make_notifier)(&w));
    }

    let _lock = match get("lock-file") {
        Some(p) => Some(LockFile::take(Path::new(&p)).map_err(|e| (2, e))?),
        None => None,
    };

    let mut keeper = Keeper::new(chain, signer, payer, keys, cluster, cfg, log);
    keeper.sleeper = (deps.sleeper)();
    // the cluster must be the one the node serves, before anything else happens
    match keeper.verify_genesis() {
        Ok(()) => {}
        // the node serves another cluster: a refusal. The node cannot be asked at all: a hard failure.
        Err(crate::runner::GenesisError::Mismatch(m)) => return refuse(m),
        Err(crate::runner::GenesisError::Unreadable(m)) => return Err((20, m)),
    }
    keeper.log.event("info", "start", serde_json::json!({"mode": mode, "cluster": crate::runner::cluster_label(&keeper.cluster), "program": keeper.keys.program_id.to_string(), "payer": payer.to_string(), "dry_run": dry}));

    match mode {
        "status" => match keeper.status_report() {
            Ok(v) => {
                print_json(&v);
                Ok(if v["alerts"].as_array().map(|a| !a.is_empty()).unwrap_or(false) { 10 } else { 0 })
            }
            Err(e) => Err((20, e)),
        },
        "once" | "dry-run" => {
            let rep = keeper.pass();
            Ok(finish(&mut keeper, &rep))
        }
        _ => {
            let mut passes = 0u32;
            loop {
                let rep = keeper.pass();
                finish(&mut keeper, &rep);
                if rep.hard_failure.as_deref().map(|f| f.starts_with("refused")).unwrap_or(false) {
                    return Ok(20);
                }
                passes += 1;
                if deps.max_passes.map(|n| passes >= n).unwrap_or(false) {
                    return Ok(rep.exit_code());
                }
                keeper.sleeper.sleep(std::time::Duration::from_secs(interval));
            }
        }
    }
}

fn finish<C: Chain>(k: &mut Keeper<C>, rep: &PassReport) -> i32 {
    k.log.event(
        if rep.hard_failure.is_some() { "error" } else { "info" },
        "pass_done",
        serde_json::json!({
            "sent": rep.sent.iter().map(|s| serde_json::json!({"label": s.label, "signature": s.signature, "units": s.units})).collect::<Vec<_>>(),
            "would_send": rep.would_send,
            "progress": rep.progress,
            "alerts": rep.alerts.iter().map(|a| a.kind).collect::<Vec<_>>(),
            "hard_failure": rep.hard_failure,
            "idle": rep.idle,
            "sends_this_run": k.stats.sends,
        }),
    );
    rep.exit_code()
}
