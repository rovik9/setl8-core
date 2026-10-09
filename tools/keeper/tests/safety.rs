//! The command line of the keeper: refusals, caps, secrecy and the four modes, driven through
//! `setl8_keeper::cli::execute` against the LiteSVM world. Only generated throwaway keys and the PUBLIC
//! localnet test admin keys are used; nothing here touches the network.
#![cfg(feature = "localnet")]
mod common;
use common::*;

use std::cell::{Cell, RefCell};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use anchor_lang::prelude::Pubkey;
use anchor_lang::Discriminator;
use base64::Engine;
use serde_json::Value;
use setl8_admin::constants::{COMPUTE_BUDGET_ID, DEVNET_GENESIS, MAINNET_GENESIS};
use setl8_keeper::chain::{CResult, Chain, ChainError, RawAccount, Sim, TxStatus};
use setl8_keeper::cli::{execute, Deps};
use setl8_keeper::log::Notifier;
use setl8_keeper::runner::NoSleep;
use setl8_keeper::safety::ADMIN_PUBKEYS;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const SECRET_URL: &str = "https://hooks.example.test/SECRETPATH123";

// ------------------------------------------------------------------------------------------ spy chain

/// A `Chain` that delegates to a `LiteChain` and counts every call, records every transaction handed to `send`,
/// and can run a hook the first time the genesis hash is asked for (i.e. right after start-up took its lock).
type Hook = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

#[derive(Clone)]
struct Spy {
    inner: LiteChain,
    calls: Rc<Cell<u32>>,
    txs: Rc<RefCell<Vec<Vec<u8>>>>,
    on_genesis: Hook,
    /// One entry per genesis hash call, front first: `true` = the node cannot be reached. Calls past the end succeed.
    genesis_script: Rc<RefCell<std::collections::VecDeque<bool>>>,
}

impl Spy {
    fn new(inner: &LiteChain) -> Spy {
        Spy {
            inner: inner.clone(),
            calls: Rc::new(Cell::new(0)),
            txs: Rc::new(RefCell::new(vec![])),
            on_genesis: Rc::new(RefCell::new(None)),
            genesis_script: Rc::new(RefCell::new(Default::default())),
        }
    }
    fn bump(&self) {
        self.calls.set(self.calls.get() + 1);
    }
}

impl Chain for Spy {
    fn genesis_hash(&self) -> CResult<String> {
        self.bump();
        let hook = self.on_genesis.borrow_mut().take();
        if let Some(mut h) = hook {
            h();
        }
        if self.genesis_script.borrow_mut().pop_front() == Some(true) {
            return Err(ChainError::Network("connection refused".into()));
        }
        self.inner.genesis_hash()
    }
    fn now(&self) -> CResult<i64> {
        self.bump();
        self.inner.now()
    }
    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>> {
        self.bump();
        self.inner.account(key)
    }
    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>> {
        self.bump();
        self.inner.accounts(keys)
    }
    fn claim_addresses(&self) -> CResult<Vec<Pubkey>> {
        self.bump();
        self.inner.claim_addresses()
    }
    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>> {
        self.bump();
        self.inner.registries()
    }
    fn balance(&self, key: &Pubkey) -> CResult<u64> {
        self.bump();
        self.inner.balance(key)
    }
    fn blockhash(&self) -> CResult<(Hash, u64)> {
        self.bump();
        self.inner.blockhash()
    }
    fn block_height(&self) -> CResult<u64> {
        self.bump();
        self.inner.block_height()
    }
    fn simulate(&self, tx: &[u8]) -> CResult<Sim> {
        self.bump();
        self.inner.simulate(tx)
    }
    fn send(&self, tx: &[u8]) -> CResult<String> {
        self.bump();
        self.txs.borrow_mut().push(tx.to_vec());
        self.inner.send(tx)
    }
    fn status(&self, signature: &str) -> CResult<Option<TxStatus>> {
        self.bump();
        self.inner.status(signature)
    }
}

// ------------------------------------------------------------------------------------------ notifier

struct TestNotifier {
    posts: Rc<RefCell<Vec<String>>>,
    attempts: Rc<Cell<u32>>,
    fail: Rc<Cell<bool>>,
}

impl Notifier for TestNotifier {
    fn post(&self, body: &str) -> Result<(), setl8_keeper::log::PostFailed> {
        self.attempts.set(self.attempts.get() + 1);
        if self.fail.get() {
            return Err(setl8_keeper::log::PostFailed);
        }
        self.posts.borrow_mut().push(body.to_string());
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------ the rig

/// One command-line "process": Deps built from the pieces below, everything captured.
struct Rig {
    spy: Spy,
    /// How many times the CLI asked for a chain connection.
    made: Rc<Cell<u32>>,
    log: Rc<RefCell<Vec<String>>>,
    err: Rc<RefCell<Vec<String>>>,
    posts: Rc<RefCell<Vec<String>>>,
    post_attempts: Rc<Cell<u32>>,
    fail_posts: Rc<Cell<bool>>,
    /// Every URL the CLI handed to `make_notifier`.
    notifier_urls: Rc<RefCell<Vec<String>>>,
    webhook_env: Option<String>,
    max_passes: Option<u32>,
    /// The chain's counters when this rig was made (clones of a `LiteChain` share them).
    sends0: u32,
    sims0: u32,
}

impl Rig {
    fn new(chain: &LiteChain) -> Rig {
        Rig {
            spy: Spy::new(chain),
            made: Rc::new(Cell::new(0)),
            log: Rc::new(RefCell::new(vec![])),
            err: Rc::new(RefCell::new(vec![])),
            posts: Rc::new(RefCell::new(vec![])),
            post_attempts: Rc::new(Cell::new(0)),
            fail_posts: Rc::new(Cell::new(false)),
            notifier_urls: Rc::new(RefCell::new(vec![])),
            webhook_env: None,
            max_passes: Some(1),
            sends0: chain.sends.get(),
            sims0: chain.simulations.get(),
        }
    }

    fn deps(&self) -> Deps {
        let spy = self.spy.clone();
        let made = self.made.clone();
        let (posts, attempts, fail, urls) =
            (self.posts.clone(), self.post_attempts.clone(), self.fail_posts.clone(), self.notifier_urls.clone());
        Deps {
            make_chain: Box::new(move |_url, _program| {
                made.set(made.get() + 1);
                Box::new(spy.clone())
            }),
            make_notifier: Box::new(move |url| {
                urls.borrow_mut().push(url.to_string());
                Box::new(TestNotifier { posts: posts.clone(), attempts: attempts.clone(), fail: fail.clone() })
            }),
            sleeper: Box::new(|| Box::new(NoSleep)),
            capture: Some(self.log.clone()),
            stdout_log: false,
            max_passes: self.max_passes,
            webhook_env: self.webhook_env.clone(),
            err: self.err.clone(),
            print_err: false,
        }
    }

    fn run<S: AsRef<str>>(&self, args: &[S]) -> i32 {
        let v: Vec<String> = args.iter().map(|s| s.as_ref().to_string()).collect();
        execute(&v, &self.deps())
    }

    fn lines(&self) -> Vec<Value> {
        self.log.borrow().iter().map(|l| serde_json::from_str(l).expect("every log line is JSON")).collect()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.lines().into_iter().filter(|l| l["event"] == name).collect()
    }
    fn alerts(&self) -> Vec<String> {
        self.lines()
            .iter()
            .filter(|l| l["level"] == "alert")
            .map(|l| format!("{}:{}", l["alert"].as_str().unwrap(), l["key"].as_str().unwrap()))
            .collect()
    }
    fn err_text(&self) -> String {
        self.err.borrow().join("\n")
    }
    /// Transactions this rig handed to the node.
    fn sends(&self) -> u32 {
        self.spy.inner.sends.get() - self.sends0
    }
    fn simulations(&self) -> u32 {
        self.spy.inner.simulations.get() - self.sims0
    }
    fn calls(&self) -> u32 {
        self.spy.calls.get()
    }
    /// The last `pass_done` event.
    fn last_pass(&self) -> Value {
        self.events("pass_done").pop().expect("a pass ran")
    }
    fn post_values(&self) -> Vec<Value> {
        self.posts.borrow().iter().map(|b| serde_json::from_str(b).unwrap()).collect()
    }
    fn everything(&self) -> Vec<(&'static str, String)> {
        vec![
            ("log lines", self.log.borrow().join("\n")),
            ("stderr", self.err_text()),
            ("webhook bodies", self.posts.borrow().join("\n")),
        ]
    }
}

fn labels(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x["label"].as_str().unwrap().to_string()))
        .collect()
}

// ------------------------------------------------------------------------------------------ temp files

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "setl8-keeper-safety-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Tmp(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    /// Created 0600 (so a secret is never briefly world readable), then set to `mode`.
    fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        use std::io::Write;
        let p = self.path(name);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p).unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }
    /// A solana-keygen style key file: a JSON array of the 64 bytes.
    fn key(&self, name: &str, kp: &Keypair, mode: u32) -> PathBuf {
        self.write(name, serde_json::to_string(&kp.to_bytes().to_vec()).unwrap().as_bytes(), mode)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

// ------------------------------------------------------------------------------------------ world helpers

/// `n` USDC claims of (10 + i) USDC each; the USDC pool holds 5,000 USDC, the USDT pool nothing.
fn world(n: u64) -> (Env, Vec<Pubkey>) {
    let mut e = Env::new();
    let usdc = e.usdc;
    let mut claims = vec![];
    for i in 0..n {
        claims.push(e.queue_claim(&usdc, (10 + i) * M).1);
    }
    e.fill_pool(&usdc, 5_000 * M);
    (e, claims)
}

fn funded(e: &Env) -> Keypair {
    let kp = Keypair::new();
    e.chain.svm.borrow_mut().airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    kp
}

/// `[mode, --cluster localnet, --keypair <key>, extra...]`
fn args(mode: &str, key: &Path, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> =
        [mode, "--cluster", "localnet", "--keypair", p(key)].iter().map(|s| s.to_string()).collect();
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

/// Everything a dry run must leave alone.
fn fingerprint(e: &Env, claims: &[Pubkey], payer: &Pubkey) -> Vec<u64> {
    let v = e.vault_state();
    let mut f = vec![
        v.cycle_id,
        v.cycle_active as u64,
        v.cycle_started_at as u64,
        v.open_claims_count,
        v.open_claims_total,
        v.cycle_eligible_count,
        v.cycle_processed_count,
        e.balance(&e.pool(&e.usdc)),
        e.balance(&e.pool(&e.usdt)),
        e.chain.balance(payer).unwrap(),
    ];
    for c in claims {
        let c = e.claim(c).expect("claim exists");
        f.push(c.owed);
        f.push(c.last_settled_cycle);
    }
    for t in 0..e.traders.len() {
        f.push(e.balance(&ata(&e.wallet(t), &e.usdc)));
        f.push(e.balance(&ata(&e.wallet(t), &e.usdt)));
    }
    f
}

fn full_cycle_labels(sector: &Pubkey, claims: usize) -> Vec<String> {
    let mut v = vec![format!("reconcile_product {sector}"), "begin_heartbeat".to_string()];
    let mut left = claims;
    while left > 0 {
        let n = left.min(6);
        v.push(format!("settle_claims x{n}"));
        left -= n;
    }
    v.push("finalize_heartbeat".to_string());
    v
}

// ------------------------------------------------------------------------------------------ secrecy helpers

fn hex(b: &[u8], upper: bool) -> String {
    b.iter().map(|x| if upper { format!("{x:02X}") } else { format!("{x:02x}") }).collect()
}

/// Every encoding of the secret key (and of its 32-byte seed) a log line could carry by accident.
/// The public half is not listed: the payer's address is logged on purpose.
fn needles(kp: &Keypair) -> Vec<(String, String)> {
    let all = kp.to_bytes();
    let mut out = vec![];
    for (what, b) in [("keypair", &all[..]), ("seed", &all[..32])] {
        let dec: Vec<String> = b.iter().map(|x| x.to_string()).collect();
        out.push((format!("{what} base58"), bs58::encode(b).into_string()));
        out.push((format!("{what} JSON array"), serde_json::to_string(&b.to_vec()).unwrap()));
        out.push((format!("{what} JSON array spaced"), format!("[{}]", dec.join(", "))));
        out.push((format!("{what} comma-joined decimals"), dec.join(",")));
        out.push((format!("{what} space-joined decimals"), dec.join(" ")));
        out.push((format!("{what} hex"), hex(b, false)));
        out.push((format!("{what} HEX"), hex(b, true)));
        out.push((format!("{what} base64"), base64::engine::general_purpose::STANDARD.encode(b)));
        out.push((format!("{what} base64 url-safe"), base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)));
    }
    out
}

/// The names of the encodings of the key found in `hay`.
fn leaked(kp: &Keypair, hay: &str) -> Vec<String> {
    needles(kp).into_iter().filter(|(_, n)| hay.contains(n.as_str())).map(|(name, _)| name).collect()
}

fn assert_no_secret(kp: &Keypair, haystacks: &[(&str, String)]) {
    for (hname, hay) in haystacks {
        let found = leaked(kp, hay);
        assert!(found.is_empty(), "the {found:?} of the key leaked into the {hname}");
    }
}

#[test]
fn the_secret_scanner_itself_finds_every_encoding_of_the_key() {
    let kp = Keypair::new();
    let all = needles(&kp);
    assert_eq!(all.len(), 18);
    for (name, needle) in &all {
        let hay = format!("{{\"event\":\"x\",\"note\":\"before {needle} after\"}}");
        assert!(leaked(&kp, &hay).contains(name), "{name} is not detected");
    }
    let other = Keypair::new();
    assert!(leaked(&other, &all.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>().join(" ")).is_empty());
    assert!(leaked(&kp, &format!("payer {}", kp.pubkey())).is_empty(), "the public key is not a secret");
}

fn assert_absent(needle: &str, haystacks: &[(&str, String)]) {
    for (hname, hay) in haystacks {
        assert!(!hay.contains(needle), "'{needle}' leaked into the {hname}");
    }
}

// ================================================================================================
// 1. admin keys are refused as the fee payer
// ================================================================================================

fn refusal_with_admin_key(admin: fn(&Env) -> Keypair, name: &str) {
    let e = Env::new();
    let kp = admin(&e);
    let tmp = Tmp::new();
    let key = tmp.key("admin.json", &kp, 0o600);
    for mode in ["once", "run", "dry-run", "status"] {
        let rig = Rig::new(&e.chain);
        let code = rig.run(&args(mode, &key, &[]));
        assert_eq!(code, 2, "{mode}");
        let msg = rig.err_text();
        assert!(msg.contains("refused"), "{mode}: {msg}");
        assert!(msg.contains(&format!("{name} admin (public test key)")), "{mode}: {msg}");
        assert!(msg.contains(&kp.pubkey().to_string()), "{mode}: the public key is named: {msg}");
        assert_eq!(rig.sends(), 0, "{mode}");
        assert_eq!(rig.simulations(), 0, "{mode}");
        assert_eq!(rig.made.get(), 0, "{mode}: no chain connection is even made");
        assert_eq!(rig.calls(), 0, "{mode}");
        assert!(rig.log.borrow().is_empty(), "{mode}: nothing is logged before the refusal");
        assert_no_secret(&kp, &rig.everything());
    }
}

#[test]
fn refuses_to_start_with_the_sl8_test_admin_key_as_the_fee_payer() {
    refusal_with_admin_key(|e| dup(&e.sl8), "SL8");
}

#[test]
fn refuses_to_start_with_the_rov_test_admin_key_as_the_fee_payer() {
    refusal_with_admin_key(|e| dup(&e.rov), "ROV");
}

#[test]
fn dry_run_and_status_refuse_every_admin_pubkey_given_as_the_fee_payer() {
    let e = Env::new();
    assert_eq!(ADMIN_PUBKEYS.len(), 4);
    for (name, pk) in ADMIN_PUBKEYS {
        for mode in ["dry-run", "status"] {
            let rig = Rig::new(&e.chain);
            let code = rig.run(&[mode, "--cluster", "localnet", "--fee-payer-pubkey", &pk.to_string()]);
            assert_eq!(code, 2, "{mode} {name}");
            let msg = rig.err_text();
            assert!(msg.contains("refused") && msg.contains(name), "{mode} {name}: {msg}");
            assert_eq!((rig.sends(), rig.simulations(), rig.calls(), rig.made.get()), (0, 0, 0, 0));
        }
    }
}

#[test]
fn the_compiled_test_admin_pubkeys_are_among_the_refused_ones() {
    let e = Env::new();
    let refused: Vec<Pubkey> = ADMIN_PUBKEYS.iter().map(|(_, k)| *k).collect();
    assert!(refused.contains(&e.sl8.pubkey()) && refused.contains(&e.rov.pubkey()));
    assert_eq!(e.sl8.pubkey().to_string(), "9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");
    assert_eq!(e.rov.pubkey().to_string(), "D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");
}

#[test]
fn a_fresh_key_is_accepted_and_named_as_the_payer_in_the_start_event() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 0);
    assert_eq!(rig.err_text(), "");
    let start = rig.events("start");
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["payer"], kp.pubkey().to_string());
    assert_eq!(start[0]["mode"], "once");
    assert_eq!(start[0]["cluster"], "localnet");
    assert_eq!(start[0]["program"], core_vault::ID.to_string());
    assert_eq!(start[0]["dry_run"], false);
    assert_eq!(start[0]["level"], "info");
    assert_eq!(rig.made.get(), 1);
}

// ================================================================================================
// 2. key file permissions and unreadable files
// ================================================================================================

#[test]
fn key_files_readable_by_group_or_others_are_refused_with_a_chmod_hint() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    for (mode, shown) in [(0o644, "644"), (0o640, "640"), (0o604, "604"), (0o777, "777")] {
        let key = tmp.key(&format!("k{shown}.json"), &kp, mode);
        for m in ["once", "run", "dry-run", "status"] {
            let rig = Rig::new(&e.chain);
            assert_eq!(rig.run(&args(m, &key, &[])), 2, "{m} mode {shown}");
            let msg = rig.err_text();
            assert!(msg.contains("refused"), "{msg}");
            assert!(msg.contains(&format!("has mode {shown}")), "{msg}");
            assert!(msg.contains(&format!("chmod 600 {}", p(&key))), "{msg}");
            assert_eq!((rig.sends(), rig.calls(), rig.made.get()), (0, 0, 0));
            assert_no_secret(&kp, &rig.everything());
        }
    }
}

#[test]
fn key_files_with_mode_600_and_400_are_accepted() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    for (mode, shown) in [(0o600, "600"), (0o400, "400"), (0o700, "700")] {
        let key = tmp.key(&format!("k{shown}.json"), &kp, mode);
        let rig = Rig::new(&e.chain);
        assert_eq!(rig.run(&args("once", &key, &[])), 0, "mode {shown}: {}", rig.err_text());
        assert_eq!(rig.err_text(), "");
        assert_eq!(rig.events("start")[0]["payer"], kp.pubkey().to_string());
    }
}

#[test]
fn a_missing_key_file_is_a_usage_error_naming_the_path() {
    let e = Env::new();
    let tmp = Tmp::new();
    let key = tmp.path("nope.json");
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 1);
    let msg = rig.err_text();
    assert!(msg.contains("cannot read key file") && msg.contains(p(&key)), "{msg}");
    assert_eq!((rig.sends(), rig.calls(), rig.made.get()), (0, 0, 0));
}

#[test]
fn a_directory_given_as_the_key_file_is_a_usage_error() {
    let e = Env::new();
    let tmp = Tmp::new();
    let dir = tmp.path("sub");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &dir, &[])), 1);
    assert!(rig.err_text().contains("cannot read key file"), "{}", rig.err_text());
    assert_eq!((rig.sends(), rig.calls(), rig.made.get()), (0, 0, 0));
}

#[test]
fn garbage_key_files_are_a_usage_error_and_their_contents_are_never_echoed() {
    let e = Env::new();
    let tmp = Tmp::new();
    let real = Keypair::new();
    let mut inconsistent = real.to_bytes();
    inconsistent[40] ^= 0xff; // the public half no longer matches the seed
    let short: Vec<u8> = real.to_bytes()[..63].to_vec();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("text", b"GARBAGE-MARKER-9f3a not a key at all".to_vec(), "GARBAGE-MARKER-9f3a"),
        ("empty", vec![], "is not a keypair file"),
        ("base58", bs58::encode(real.to_bytes()).into_string().into_bytes(), "is not a keypair file"),
        ("short", serde_json::to_vec(&short).unwrap(), "is not a keypair file"),
        ("inconsistent", serde_json::to_vec(&inconsistent.to_vec()).unwrap(), "is not a keypair file"),
        ("object", br#"{"secret":"GARBAGE-MARKER-9f3a"}"#.to_vec(), "GARBAGE-MARKER-9f3a"),
    ];
    for (name, bytes, marker) in cases {
        let key = tmp.write(&format!("{name}.json"), &bytes, 0o600);
        let rig = Rig::new(&e.chain);
        assert_eq!(rig.run(&args("once", &key, &[])), 1, "{name}");
        let msg = rig.err_text();
        assert!(msg.contains("is not a keypair file"), "{name}: {msg}");
        if marker != "is not a keypair file" {
            assert!(!msg.contains(marker), "{name}: the file contents were echoed: {msg}");
        }
        assert_no_secret(&real, &rig.everything());
        assert_eq!((rig.sends(), rig.calls(), rig.made.get()), (0, 0, 0), "{name}");
    }
}

// ================================================================================================
// 3. the mainnet gate
// ================================================================================================

#[test]
fn mainnet_without_the_flag_is_refused_before_the_key_is_read_or_any_chain_call() {
    let e = Env::new();
    let tmp = Tmp::new();
    let missing = tmp.path("never-read.json");
    for cluster in ["mainnet", "mainnet-beta"] {
        for mode in ["once", "run", "dry-run", "status"] {
            let rig = Rig::new(&e.chain);
            let code = rig.run(&[mode, "--cluster", cluster, "--keypair", p(&missing)]);
            assert_eq!(code, 2, "{cluster} {mode}");
            let msg = rig.err_text();
            assert!(msg.contains("refused") && msg.contains("--i-understand-this-is-mainnet"), "{msg}");
            assert_eq!(rig.calls(), 0, "no chain call");
            assert_eq!(rig.made.get(), 0, "no chain connection");
            assert_eq!(rig.sends(), 0);
            assert!(rig.log.borrow().is_empty());
        }
    }
}

#[test]
fn mainnet_with_the_flag_is_still_refused_when_the_node_is_not_mainnet() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    assert_eq!(*e.chain.genesis.borrow(), LOCAL_GENESIS);
    for mode in ["once", "dry-run", "status"] {
        let rig = Rig::new(&e.chain);
        let code = rig.run(&[mode, "--cluster", "mainnet", "--keypair", p(&key), "--i-understand-this-is-mainnet"]);
        assert_eq!(code, 2, "{mode}");
        let msg = rig.err_text();
        assert!(
            msg.contains(&format!("refused: --cluster mainnet but the node's genesis hash is {LOCAL_GENESIS}")),
            "{msg}"
        );
        assert_eq!(rig.calls(), 1, "only the genesis hash was asked for");
        assert_eq!(rig.made.get(), 1);
        assert_eq!((rig.sends(), rig.simulations()), (0, 0));
        assert_eq!(rig.alerts(), vec!["genesis_mismatch:genesis"]);
        assert!(rig.events("pass_done").is_empty(), "no pass ran");
    }
}

#[test]
fn mainnet_with_the_flag_and_a_mainnet_genesis_is_accepted() {
    let e = Env::new();
    *e.chain.genesis.borrow_mut() = MAINNET_GENESIS.into();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&["once", "--cluster", "mainnet", "--keypair", p(&key), "--i-understand-this-is-mainnet"]);
    assert_eq!(code, 0, "{}", rig.err_text());
    assert_eq!(rig.events("start")[0]["cluster"], "mainnet-beta");
}

#[test]
fn the_mainnet_flag_takes_no_value_and_may_not_be_repeated() {
    let e = Env::new();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&["once", "--cluster", "mainnet", "--i-understand-this-is-mainnet=yes"]), 1);
    assert!(rig.err_text().contains("--i-understand-this-is-mainnet takes no value"), "{}", rig.err_text());
    let rig = Rig::new(&e.chain);
    assert_eq!(
        rig.run(&["once", "--cluster", "mainnet", "--i-understand-this-is-mainnet", "--i-understand-this-is-mainnet"]),
        1
    );
    assert!(rig.err_text().contains("--i-understand-this-is-mainnet given twice"));
    assert_eq!(rig.calls(), 0);
}

// ================================================================================================
// 4. genesis mismatch
// ================================================================================================

fn run_on(genesis: &str, cluster: &str) -> (Rig, i32) {
    let e = Env::new();
    *e.chain.genesis.borrow_mut() = genesis.into();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&["once", "--cluster", cluster, "--keypair", p(&key)]);
    (rig, code)
}

#[test]
fn devnet_against_a_local_genesis_is_refused() {
    let (rig, code) = run_on(LOCAL_GENESIS, "devnet");
    assert_eq!(code, 2);
    assert!(rig
        .err_text()
        .contains(&format!("refused: --cluster devnet but the node's genesis hash is {LOCAL_GENESIS}")));
    assert_eq!((rig.calls(), rig.sends()), (1, 0));
    assert_eq!(rig.alerts(), vec!["genesis_mismatch:genesis"]);
    let alert = &rig.events("genesis_mismatch")[0];
    assert_eq!((alert["genesis"].as_str().unwrap(), alert["cluster"].as_str().unwrap()), (LOCAL_GENESIS, "devnet"));
}

#[test]
fn devnet_against_a_mainnet_genesis_is_refused() {
    let (rig, code) = run_on(MAINNET_GENESIS, "devnet");
    assert_eq!(code, 2);
    assert!(rig.err_text().contains(&format!("the node's genesis hash is {MAINNET_GENESIS}")));
    assert_eq!((rig.calls(), rig.sends()), (1, 0));
}

#[test]
fn devnet_against_the_devnet_genesis_is_accepted() {
    let (rig, code) = run_on(DEVNET_GENESIS, "devnet");
    assert_eq!(code, 0, "{}", rig.err_text());
    assert_eq!(rig.events("start")[0]["cluster"], "devnet");
    assert_eq!(rig.err_text(), "");
}

#[test]
fn localnet_against_the_devnet_genesis_is_refused() {
    let (rig, code) = run_on(DEVNET_GENESIS, "localnet");
    assert_eq!(code, 2);
    assert!(rig.err_text().contains(&format!(
        "refused: --cluster localnet but the node is on a public cluster (genesis {DEVNET_GENESIS})"
    )));
    assert_eq!((rig.calls(), rig.sends()), (1, 0));
}

#[test]
fn localnet_against_the_mainnet_genesis_is_refused() {
    let (rig, code) = run_on(MAINNET_GENESIS, "localnet");
    assert_eq!(code, 2);
    assert!(rig.err_text().contains(&format!(
        "refused: --cluster localnet but the node is on a public cluster (genesis {MAINNET_GENESIS})"
    )));
    assert_eq!((rig.calls(), rig.sends()), (1, 0));
}

#[test]
fn localnet_against_a_local_genesis_is_accepted() {
    let (rig, code) = run_on(LOCAL_GENESIS, "localnet");
    assert_eq!(code, 0, "{}", rig.err_text());
}

#[test]
fn a_custom_url_cluster_is_refused_on_a_public_genesis_and_accepted_on_a_private_one() {
    let (rig, code) = run_on(DEVNET_GENESIS, "http://127.0.0.1:8899");
    assert_eq!(code, 2);
    assert!(rig.err_text().contains("but the node is on a public cluster"), "{}", rig.err_text());
    let (rig, code) = run_on(LOCAL_GENESIS, "http://127.0.0.1:8899");
    assert_eq!(code, 0, "{}", rig.err_text());
}

#[test]
fn the_genesis_is_checked_again_on_every_pass_and_run_stops_with_20_when_the_node_changes() {
    let (e, claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    // the node "becomes" devnet right before the first transaction is sent
    let g = e.chain.genesis.clone();
    *e.chain.before_send.borrow_mut() = Some(Box::new(move || *g.borrow_mut() = DEVNET_GENESIS.into()));
    let mut rig = Rig::new(&e.chain);
    rig.max_passes = Some(5);
    let code = rig.run(&args("run", &key, &[]));
    assert_eq!(code, 20);
    let passes = rig.events("pass_done");
    assert_eq!(passes.len(), 2, "pass 1 finishes its cycle, pass 2 refuses and the loop ends");
    assert_eq!(labels(&passes[0], "sent"), full_cycle_labels(&e.sector, 3));
    assert!(passes[0]["hard_failure"].is_null());
    assert!(passes[1]["hard_failure"]
        .as_str()
        .unwrap()
        .starts_with("refused: --cluster localnet but the node is on a public cluster"));
    assert_eq!(labels(&passes[1], "sent"), Vec::<String>::new());
    assert_eq!(rig.sends(), 4);
    assert_eq!(e.vault_state().cycle_id, 1);
    assert!(claims.iter().all(|c| e.claim(c).is_none()), "pass 1 paid all three");
}

/// A node that cannot be reached when the process STARTS is a hard failure (exit 20, "cannot read the genesis
/// hash"; a node that answers with ANOTHER cluster's genesis is a refusal, exit 2), and `run` does not start its
/// loop at all. The same outage in the middle of a `run` loop is only a failed pass (next test).
#[test]
fn a_node_that_cannot_be_reached_at_start_up_exits_20_with_the_genesis_read_error_and_sends_nothing() {
    let (e, _claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    for mode in ["once", "run"] {
        let rig = Rig::new(&e.chain);
        rig.spy.genesis_script.borrow_mut().extend([true; 100]);
        assert_eq!(rig.run(&args(mode, &key, &["--max-retries", "2"])), 20, "{mode}");
        assert_eq!(rig.err_text(), "error: cannot read the genesis hash: connection refused", "{mode}");
        assert_eq!(rig.spy.genesis_script.borrow().len(), 100 - 3, "one try and two retries: {mode}");
        assert_eq!((rig.sends(), rig.simulations()), (0, 0));
        assert!(rig.events("start").is_empty() && rig.events("pass_done").is_empty());
    }
    assert_eq!(e.vault_state().cycle_id, 0);
}

#[test]
fn a_node_that_goes_away_inside_a_run_loop_fails_that_pass_only_and_the_next_pass_runs_the_cycle() {
    let (e, claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.max_passes = Some(2);
    // call 1 is the start-up check (up); the first pass then fails both of its reads (1 try + 1 retry); then all is well
    rig.spy.genesis_script.borrow_mut().extend([false, true, true]);
    let code = rig.run(&args("run", &key, &["--interval", "1", "--max-retries", "1"]));
    assert_eq!(code, 0, "the last pass ran the cycle: {}", rig.err_text());
    let passes = rig.events("pass_done");
    assert_eq!(passes.len(), 2);
    assert_eq!(passes[0]["hard_failure"], "cannot read the genesis hash: connection refused");
    assert_eq!(passes[0]["level"], "error");
    assert_eq!(labels(&passes[0], "sent"), Vec::<String>::new());
    assert!(passes[1]["hard_failure"].is_null());
    assert_eq!(labels(&passes[1], "sent"), full_cycle_labels(&e.sector, 3));
    assert_eq!(rig.sends(), 4);
    assert!(claims.iter().all(|c| e.claim(c).is_none()));
}

// ================================================================================================
// 5. caps
// ================================================================================================

#[test]
fn max_sends_per_run_2_stops_after_reconcile_and_begin() {
    let (e, claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&args("once", &key, &["--max-sends-per-run", "2"]));
    assert_eq!(code, 20);
    assert_eq!(rig.sends(), 2, "exactly two transactions reached the node");
    let caps = rig.events("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-sends-per-run");
    assert_eq!(caps[0]["sends"], 2);
    assert_eq!(caps[0]["level"], "error");
    let done = rig.last_pass();
    assert_eq!(labels(&done, "sent"), vec![format!("reconcile_product {}", e.sector), "begin_heartbeat".to_string()]);
    assert_eq!(done["hard_failure"], "send cap reached (max-sends-per-run)");
    assert_eq!(done["sends_this_run"], 2);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (1, true, 0, 15));
    assert!(claims.iter().all(|c| e.claim(c).is_some()), "nobody was paid");
}

#[test]
fn max_sends_per_run_4_pays_exactly_two_batches() {
    let (e, _claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--max-sends-per-run", "4"])), 20);
    assert_eq!(rig.sends(), 4);
    assert_eq!(
        labels(&rig.last_pass(), "sent"),
        vec![
            format!("reconcile_product {}", e.sector),
            "begin_heartbeat".into(),
            "settle_claims x6".into(),
            "settle_claims x6".into()
        ]
    );
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count, vs.open_claims_count), (true, 12, 3));
    assert_eq!(rig.events("send_cap_reached").len(), 1);
}

#[test]
fn max_sends_per_run_0_sends_nothing_and_fails_hard() {
    let (e, _claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--max-sends-per-run", "0"])), 20);
    assert_eq!(rig.sends(), 0);
    assert_eq!(rig.last_pass()["hard_failure"], "send cap reached during the reconcile pass");
    assert_eq!(e.vault_state().cycle_id, 0);
}

#[test]
fn max_fee_lamports_per_run_5000_allows_exactly_one_transaction() {
    let (e, _claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--max-fee-lamports-per-run", "5000"])), 20);
    assert_eq!(rig.sends(), 1, "one 5,000 lamport transaction fits, the second would pass the cap");
    let caps = rig.events("send_cap_reached");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["cap"], "max-fee-lamports-per-run");
    assert_eq!(caps[0]["estimated_fees"], 5000);
    let done = rig.last_pass();
    assert_eq!(labels(&done, "sent"), vec![format!("reconcile_product {}", e.sector)]);
    assert_eq!(done["hard_failure"], "send cap reached before the cycle could begin");
    assert_eq!(e.vault_state().cycle_id, 0, "the cycle was not begun");
}

#[test]
fn max_fee_lamports_per_run_just_below_two_transactions_allows_one() {
    let (e, _claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--max-fee-lamports-per-run", "9999"])), 20);
    assert_eq!(rig.sends(), 1);
    let rig = Rig::new(&e.chain);
    assert_eq!(
        rig.run(&args("once", &key, &["--max-fee-lamports-per-run", "10000"])),
        20,
        "two fit, the third does not"
    );
    assert_eq!(rig.sends(), 2);
    assert_eq!(rig.events("send_cap_reached")[0]["estimated_fees"], 10000);
}

#[test]
fn a_fee_cap_that_fits_the_whole_cycle_lets_it_finish() {
    let (e, _claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    // reconcile + begin + 3 settle + finalize = 6 transactions of 5,000 lamports
    assert_eq!(rig.run(&args("once", &key, &["--max-fee-lamports-per-run", "30000", "--max-sends-per-run", "6"])), 0);
    assert_eq!(rig.sends(), 6);
    assert!(rig.events("send_cap_reached").is_empty());
    assert_eq!(e.vault_state().cycle_id, 1);
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn the_send_cap_covers_the_whole_life_of_a_run_loop() {
    let (e, _claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.max_passes = Some(3);
    assert_eq!(rig.run(&args("run", &key, &["--max-sends-per-run", "3", "--interval", "1"])), 20);
    assert_eq!(rig.sends(), 3, "three passes, three transactions in all");
    assert_eq!(rig.events("pass_done").len(), 3);
    assert_eq!(rig.events("send_cap_reached").len(), 3, "each pass runs into the same cap");
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count), (true, 6));
}

// ================================================================================================
// 6. no secrets in any output
// ================================================================================================

/// A whole cycle with one alert (USDT pool frozen), logged to stdout capture and a file, alerts to a webhook.
fn full_cycle_with_alert(webhook_in_env: bool) -> (Env, Rig, Keypair, String) {
    let (e, _claims) = world(15);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let logfile = tmp.path("keeper.log");
    let mut rig = Rig::new(&e.chain);
    let mut extra: Vec<&str> = vec!["--log-file", p(&logfile)];
    if webhook_in_env {
        rig.webhook_env = Some(SECRET_URL.to_string());
    } else {
        extra.extend(["--webhook", SECRET_URL]);
    }
    let code = rig.run(&args("once", &key, &extra));
    assert_eq!(code, 10, "{}", rig.err_text());
    let file = std::fs::read_to_string(&logfile).unwrap();
    (e, rig, kp, file)
}

fn check_no_secrets_in_a_full_cycle(webhook_in_env: bool) {
    let (e, rig, kp, file) = full_cycle_with_alert(webhook_in_env);
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_eq!(rig.sends(), 6);
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdt"]);
    // the log file holds exactly the lines that were captured
    assert_eq!(file, format!("{}\n", rig.log.borrow().join("\n")));
    let bodies = rig.post_values();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["alert"], "pool_frozen");
    assert_eq!(
        rig.notifier_urls.borrow().as_slice(),
        [SECRET_URL.to_string()],
        "the notifier got the URL, and only it"
    );
    let mut hay = rig.everything();
    hay.push(("log file", file));
    assert!(["log lines", "webhook bodies", "log file"].iter().all(|n| !hay
        .iter()
        .find(|(h, _)| h == n)
        .unwrap()
        .1
        .is_empty()));
    assert_no_secret(&kp, &hay);
    assert_absent(SECRET_URL, &hay);
    assert_absent("SECRETPATH123", &hay);
    assert_absent("hooks.example.test", &hay);
    // the public half is logged on purpose
    assert!(hay[0].1.contains(&kp.pubkey().to_string()));
}

#[test]
fn a_full_cycle_leaks_neither_the_key_nor_the_webhook_url_given_on_the_command_line() {
    check_no_secrets_in_a_full_cycle(false);
}

#[test]
fn a_full_cycle_leaks_neither_the_key_nor_the_webhook_url_given_through_the_environment() {
    check_no_secrets_in_a_full_cycle(true);
}

#[test]
fn a_failing_webhook_and_a_refused_start_leak_nothing_either() {
    let (e, _claims) = world(3);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.fail_posts.set(true);
    rig.webhook_env = Some(SECRET_URL.to_string());
    assert_eq!(rig.run(&args("once", &key, &[])), 10);
    assert_eq!(rig.post_attempts.get(), 1);
    assert_no_secret(&kp, &rig.everything());
    assert_absent("SECRETPATH123", &rig.everything());

    // a start the CLI refuses (admin key) with a webhook configured
    let admin = dup(&e.sl8);
    let akey = tmp.key("admin.json", &admin, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &akey, &["--webhook", SECRET_URL])), 2);
    assert_no_secret(&admin, &rig.everything());
    assert_absent("SECRETPATH123", &rig.everything());
    assert!(rig.notifier_urls.borrow().is_empty(), "refused before any notifier was built");
}

#[test]
fn the_webhook_on_the_command_line_wins_over_the_environment_and_neither_is_needed() {
    let e = Env::new();
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.webhook_env = Some("https://env.example.test/ENVSECRET".into());
    assert_eq!(rig.run(&args("once", &key, &["--webhook", "https://cli.example.test/CLISECRET"])), 10);
    assert_eq!(rig.notifier_urls.borrow().as_slice(), ["https://cli.example.test/CLISECRET".to_string()]);
    assert_absent("ENVSECRET", &rig.everything());
    assert_absent("CLISECRET", &rig.everything());

    let mut rig = Rig::new(&e.chain);
    rig.webhook_env = Some("https://env.example.test/ENVSECRET".into());
    assert_eq!(rig.run(&args("once", &key, &[])), 10);
    assert_eq!(rig.notifier_urls.borrow().as_slice(), ["https://env.example.test/ENVSECRET".to_string()]);

    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 10);
    assert!(rig.notifier_urls.borrow().is_empty());
    assert_eq!(rig.post_attempts.get(), 0);
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdt"], "the alert is still logged");
}

/// KNOWN BUG (keeper source, src/cli.rs + setl8_admin::cluster::Cluster::name): `--cluster <URL>` is accepted
/// as a custom cluster, and its full text, query string included, is written into the `start` event, every
/// alert line and every webhook body as the `cluster` field ("custom (https://...?api-key=...)"). RPC URLs
/// commonly carry an API key. Everything else (`--rpc`, the webhook URL) is kept out of the output. Run with
/// `--ignored` to see it fail; it passes once the cluster label no longer echoes the URL (for example only its host).
#[test]
fn a_custom_cluster_url_with_an_api_key_must_not_appear_in_the_output() {
    let e = Env::new();
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.webhook_env = Some(SECRET_URL.to_string());
    let code =
        rig.run(&["once", "--cluster", "https://rpc.example.test/v2/?api-key=SECRETKEY789", "--keypair", p(&key)]);
    assert_eq!(code, 10, "{}", rig.err_text());
    assert_absent("SECRETKEY789", &rig.everything());
}

#[test]
fn the_rpc_url_is_not_written_to_any_output() {
    let e = Env::new();
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.webhook_env = Some(SECRET_URL.to_string());
    let code = rig.run(&args("once", &key, &["--rpc", "https://rpc.example.test/v2/?api-key=SECRETKEY789"]));
    assert_eq!(code, 10, "{}", rig.err_text());
    assert_absent("SECRETKEY789", &rig.everything());
    assert_absent("rpc.example.test", &rig.everything());
}

// ================================================================================================
// 7. webhook behaviour
// ================================================================================================

#[test]
fn a_failing_webhook_does_not_stop_the_cycle_and_is_logged_without_the_url() {
    let (e, claims) = world(3);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    rig.fail_posts.set(true);
    let code = rig.run(&args("once", &key, &["--webhook", SECRET_URL]));
    assert_eq!(code, 10);
    // the cycle completed in full
    assert_eq!(labels(&rig.last_pass(), "sent"), full_cycle_labels(&e.sector, 3));
    assert_eq!(rig.sends(), 4);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.open_claims_count), (1, false, 0));
    assert!(claims.iter().all(|c| e.claim(c).is_none()));
    // exactly one delivery was attempted and it is reported as a warning
    assert_eq!(rig.post_attempts.get(), 1);
    assert!(rig.posts.borrow().is_empty());
    let failed = rig.events("webhook_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["level"], "warn");
    assert_eq!(failed[0]["note"], "delivery failed; the keeper carries on");
    let keys: Vec<&String> = failed[0].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "ts, level, event, note and nothing else: {keys:?}");
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdt"]);
    assert_absent("SECRETPATH123", &rig.everything());
    assert_eq!(rig.last_pass()["alerts"], serde_json::json!(["pool_frozen"]));
}

#[test]
fn a_failing_webhook_is_retried_for_the_same_alert_on_the_next_pass() {
    let (e, _claims) = world(3);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.fail_posts.set(true);
    rig.max_passes = Some(3);
    assert_eq!(rig.run(&args("run", &key, &["--webhook", SECRET_URL, "--interval", "1"])), 10);
    assert_eq!(rig.post_attempts.get(), 3, "a delivery that failed is not counted as sent: tried on every pass");
    assert_eq!(rig.events("webhook_failed").len(), 3);
}

#[test]
fn a_working_webhook_gets_exactly_one_post_per_distinct_alert_over_several_passes() {
    let (e, _claims) = world(3);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut rig = Rig::new(&e.chain);
    rig.max_passes = Some(3);
    // a payer floor above its balance makes a second, distinct alert
    let code = rig.run(&args(
        "run",
        &key,
        &["--webhook", SECRET_URL, "--interval", "1", "--min-balance-lamports", "100_000_000_000_000"],
    ));
    assert_eq!(code, 10);
    assert_eq!(rig.events("pass_done").len(), 3);
    // every pass logs both alerts ...
    let mut logged = rig.alerts();
    logged.sort();
    assert_eq!(
        logged,
        vec!["payer_balance_low:payer"; 3].into_iter().chain(vec!["pool_frozen:usdt"; 3]).collect::<Vec<_>>()
    );
    // ... but each is posted once
    let bodies = rig.post_values();
    assert_eq!(bodies.len(), 2);
    let mut kinds: Vec<String> =
        bodies.iter().map(|b| format!("{}:{}", b["alert"].as_str().unwrap(), b["key"].as_str().unwrap())).collect();
    kinds.sort();
    assert_eq!(kinds, vec!["payer_balance_low:payer", "pool_frozen:usdt"]);
    let frozen = bodies.iter().find(|b| b["alert"] == "pool_frozen").unwrap();
    assert_eq!(frozen["cluster"], "localnet");
    assert_eq!(frozen["pool"], "usdt");
    assert_eq!(frozen["balance"], 0);
    let low = bodies.iter().find(|b| b["alert"] == "payer_balance_low").unwrap();
    assert_eq!(low["threshold_lamports"], 100_000_000_000_000u64);
    assert!(rig.events("webhook_failed").is_empty());
    assert_eq!(rig.post_attempts.get(), 2);
    // the first pass did the cycle; the other two had nothing to do
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_eq!(rig.events("idle").len(), 2);
}

#[test]
fn a_new_process_posts_a_standing_alert_again_because_the_dedup_is_per_run() {
    // the dedup memory lives in the process (alert + subject, one hour window): a second `once` is a new run
    let e = Env::new();
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    for _ in 0..2 {
        let rig = Rig::new(&e.chain);
        assert_eq!(rig.run(&args("once", &key, &["--webhook", SECRET_URL])), 10);
        assert_eq!(rig.posts.borrow().len(), 1, "a new process posts again");
    }
}

// ================================================================================================
// 8. dry-run sends nothing
// ================================================================================================

fn expected_would_send(sector: &Pubkey) -> Vec<String> {
    vec![
        format!("reconcile_product {sector}"),
        "begin_heartbeat".into(),
        "settle_claims x6 (after begin)".into(),
        "settle_claims x6 (after begin)".into(),
        "settle_claims x3 (after begin)".into(),
        "finalize_heartbeat (after the claims)".into(),
    ]
}

#[test]
fn dry_run_with_a_funded_key_sends_and_simulates_nothing_and_names_the_calls_in_order() {
    let (e, claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let before = fingerprint(&e, &claims, &kp.pubkey());
    assert_eq!(before[0], 0, "cycle 0");
    let rig = Rig::new(&e.chain);
    let code = rig.run(&args("dry-run", &key, &[]));
    assert_eq!(code, 0, "{}", rig.err_text());
    assert_eq!((rig.sends(), rig.simulations()), (0, 0));
    assert!(rig.spy.txs.borrow().is_empty());
    assert!(e.chain.landed.borrow().is_empty());
    assert_eq!(
        fingerprint(&e, &claims, &kp.pubkey()),
        before,
        "nothing on chain changed, not even the payer's lamports"
    );
    let would: Vec<String> =
        rig.events("would_send").iter().map(|l| l["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(would, expected_would_send(&e.sector));
    let done = rig.last_pass();
    assert_eq!(labels(&done, "sent"), Vec::<String>::new());
    assert_eq!(
        labels(&done, "would_send"),
        vec![
            format!("reconcile_product {}", e.sector),
            "begin_heartbeat".to_string(),
            "settle_claims x6 (after begin)".into(),
            "settle_claims x6 (after begin)".into(),
            "settle_claims x3 (after begin)".into(),
            "finalize_heartbeat".into(),
        ]
    );
    assert_eq!(done["progress"], false);
    assert_eq!(done["sends_this_run"], 0);
    assert_eq!(rig.events("start")[0]["dry_run"], true);
    // each would_send names the vault program and how many instructions, never a signature
    let first = &rig.events("would_send")[0];
    assert_eq!(first["instructions"][0]["program"], core_vault::ID.to_string());
    assert_eq!(first["instructions"][0]["accounts"], 3);
    assert_eq!(first["instructions"][0]["data_bytes"], 8 + 32);
    assert!(rig.events("sent").is_empty());
}

#[test]
fn dry_run_with_a_never_funded_pubkey_sends_nothing_and_exits_10_for_the_empty_payer() {
    let (e, claims) = world(15);
    let ghost = Pubkey::new_unique();
    assert_eq!(e.chain.balance(&ghost).unwrap(), 0);
    let before = fingerprint(&e, &claims, &ghost);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&["dry-run", "--cluster", "localnet", "--fee-payer-pubkey", &ghost.to_string()]);
    assert_eq!(code, 10, "{}", rig.err_text());
    assert_eq!((rig.sends(), rig.simulations()), (0, 0));
    assert_eq!(fingerprint(&e, &claims, &ghost), before);
    let would: Vec<String> =
        rig.events("would_send").iter().map(|l| l["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(would, expected_would_send(&e.sector));
    assert_eq!(rig.alerts(), vec!["payer_balance_low:payer"]);
    assert_eq!(rig.events("start")[0]["payer"], ghost.to_string());
    assert_eq!(e.vault_state().cycle_id, 0);
}

#[test]
fn dry_run_with_a_key_file_never_signs_or_writes_a_transaction() {
    // dry-run drops the key right after reading its public half: no transaction is ever built
    let (e, _claims) = world(7);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("dry-run", &key, &[])), 0);
    let would: Vec<String> =
        rig.events("would_send").iter().map(|l| l["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        would,
        vec![
            format!("reconcile_product {}", e.sector),
            "begin_heartbeat".to_string(),
            "settle_claims x6 (after begin)".into(),
            "settle_claims x1 (after begin)".into(),
            "finalize_heartbeat (after the claims)".into(),
        ]
    );
    assert_eq!(rig.spy.txs.borrow().len(), 0, "no transaction was even built");
    assert!(rig.calls() > 0, "the chain was read");
}

#[test]
fn dry_run_with_a_cycle_already_open_lists_the_remaining_batches_and_the_finalize() {
    let (e, claims) = world(15);
    // another keeper has done reconcile, begin and the first batch of six
    let mut other = e.keeper_with(setl8_keeper::config::Config { max_sends: 3, ..Default::default() });
    let r = other.pass();
    assert_eq!(r.hard_failure.as_deref(), Some("send cap reached (max-sends-per-run)"));
    assert_eq!(e.vault_state().cycle_processed_count, 6);
    let ghost = Pubkey::new_unique();
    let before =
        fingerprint(&e, &claims[6..].iter().filter(|c| e.claim(c).is_some()).cloned().collect::<Vec<_>>(), &ghost);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&["dry-run", "--cluster", "localnet", "--fee-payer-pubkey", &ghost.to_string()]);
    assert_eq!(code, 10);
    assert_eq!((rig.sends(), rig.simulations()), (0, 0));
    let would: Vec<String> =
        rig.events("would_send").iter().map(|l| l["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(would, vec!["settle_claims x6", "settle_claims x3", "finalize_heartbeat"]);
    assert_eq!(
        labels(&rig.last_pass(), "would_send"),
        vec!["settle_claims x6", "settle_claims x3", "finalize_heartbeat"],
        "no reconcile and no begin: a cycle is open"
    );
    let after =
        fingerprint(&e, &claims[6..].iter().filter(|c| e.claim(c).is_some()).cloned().collect::<Vec<_>>(), &ghost);
    assert_eq!(after, before);
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (1, true, 6, 15));
}

#[test]
fn dry_run_with_nothing_to_do_prints_nothing_to_send() {
    let e = Env::new();
    let ghost = Pubkey::new_unique();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&["dry-run", "--cluster", "localnet", "--fee-payer-pubkey", &ghost.to_string()]), 10);
    assert!(rig.events("would_send").is_empty());
    assert_eq!(rig.events("idle")[0]["reason"], "no_open_claims");
    assert_eq!((rig.sends(), rig.simulations()), (0, 0));
}

#[test]
fn dry_run_waits_out_the_gap_and_says_so() {
    let (e, _claims) = world(3);
    let mut k = e.keeper();
    assert!(e.drive(&mut k, 5).iter().all(|r| r.hard_failure.is_none()));
    assert_eq!(e.vault_state().cycle_id, 1);
    // new claim after the cycle, the next cycle may not begin for 432,000 s from the start of the last one
    let mut e = e;
    let usdc = e.usdc;
    e.queue_claim(&usdc, 10 * M);
    e.advance(GAP - 100);
    let rig = Rig::new(&e.chain);
    let ghost = Pubkey::new_unique();
    assert_eq!(rig.run(&["dry-run", "--cluster", "localnet", "--fee-payer-pubkey", &ghost.to_string()]), 10);
    assert!(rig.events("would_send").is_empty());
    let idle = rig.events("idle");
    assert_eq!(idle[0]["reason"], "gap_not_over");
    assert_eq!(idle[0]["seconds_left"], 100, "the first cycle began at T0 and the clock stands at T0 + 432,000 - 100");
}

// ================================================================================================
// 9. modes and exit codes
// ================================================================================================

#[test]
fn once_with_nothing_to_do_exits_0_and_sends_nothing() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 0);
    assert_eq!(rig.sends(), 0);
    let idle = rig.events("idle");
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0]["reason"], "no_open_claims");
    let done = rig.last_pass();
    assert_eq!(done["idle"], "no open claims: no cycle is begun (an empty cycle would only burn the 5-day slot)");
    assert_eq!(done["progress"], false);
    assert_eq!(done["hard_failure"], Value::Null);
    assert_eq!(done["alerts"], serde_json::json!([]));
    assert_eq!(e.vault_state().cycle_id, 0);
}

#[test]
fn once_that_makes_progress_exits_0_and_reports_what_it_sent() {
    let (e, claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 0, "{}", rig.err_text());
    let done = rig.last_pass();
    assert_eq!(labels(&done, "sent"), full_cycle_labels(&e.sector, 3));
    assert_eq!(done["progress"], true);
    assert_eq!(done["sends_this_run"], 4);
    assert_eq!(rig.sends(), 4);
    assert_eq!(rig.events("sent").len(), 4);
    assert_eq!(done["level"], "info");
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.open_claims_count, vs.open_claims_total), (1, false, 0, 0));
    assert!(claims.iter().all(|c| e.claim(c).is_none()));
    // 10 + 11 + 12 USDC left the pool
    assert_eq!(e.balance(&e.pool(&e.usdc)), 5_000 * M - 33 * M);
}

#[test]
fn once_with_an_alert_present_exits_10_even_though_it_made_progress() {
    let (e, _claims) = world(3);
    e.freeze(&e.usdt, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 10);
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdt"]);
    assert_eq!(e.vault_state().cycle_id, 1);
    assert_eq!(rig.last_pass()["alerts"], serde_json::json!(["pool_frozen"]));
}

#[test]
fn once_with_an_alert_and_nothing_to_do_exits_10() {
    let e = Env::new();
    e.freeze(&e.usdc, true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 10);
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdc"]);
    assert_eq!(rig.sends(), 0);
}

#[test]
fn once_against_a_program_with_no_vault_is_a_hard_failure_20() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let bogus = Pubkey::new_unique();
    let rig = Rig::new(&e.chain);
    let code = rig.run(&args("once", &key, &["--program-id", &bogus.to_string()]));
    assert_eq!(code, 20);
    assert_eq!(rig.sends(), 0);
    let done = rig.last_pass();
    assert!(done["hard_failure"].as_str().unwrap().contains("does not exist on this cluster"), "{done}");
    assert_eq!(done["level"], "error");
    assert_eq!(rig.events("cannot_read_chain").len(), 1);
    assert_eq!(rig.events("start")[0]["program"], bogus.to_string());
}

#[test]
fn run_against_a_program_with_no_vault_keeps_looping_and_exits_20() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let bogus = Pubkey::new_unique();
    let mut rig = Rig::new(&e.chain);
    rig.max_passes = Some(3);
    assert_eq!(rig.run(&args("run", &key, &["--program-id", &bogus.to_string(), "--interval", "5"])), 20);
    assert_eq!(rig.events("pass_done").len(), 3, "a read failure is not a refusal: the loop goes on");
}

#[test]
fn usage_errors_exit_1_and_send_nothing() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let k = p(&key);
    let ghost = Pubkey::new_unique().to_string();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["frobnicate", "--cluster", "localnet", "--keypair", k],
            "unknown mode 'frobnicate' (run, once, dry-run, status)",
        ),
        (vec!["once", "--keypair", k], "--cluster is required"),
        (vec!["once", "--cluster", "localnet"], "--keypair is required"),
        (vec!["run", "--cluster", "localnet"], "--keypair is required"),
        (vec!["dry-run", "--cluster", "localnet"], "give --keypair or --fee-payer-pubkey"),
        (vec!["status", "--cluster", "localnet"], "give --keypair or --fee-payer-pubkey"),
        (vec!["once", "--cluster", "testnet", "--keypair", k], "unknown cluster 'testnet'"),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--max-sends-per-run", "many"],
            "--max-sends-per-run: 'many' is not a valid number",
        ),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--max-sends-per-run", "-5"],
            "--max-sends-per-run: '-5' is not a valid number",
        ),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--max-fee-lamports-per-run", "1.5"],
            "is not a valid number",
        ),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--interval", "soon"],
            "--interval: 'soon' is not a valid number",
        ),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--priority-fee-microlamports", "x"],
            "--priority-fee-microlamports: 'x' is not a valid number",
        ),
        (vec!["once", "--cluster", "localnet", "--keypair", k, "--min-balance-lamports", ""], "is not a valid number"),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--max-retries", "99999999999"],
            "--max-retries: '99999999999' is not a valid number",
        ),
        (
            vec!["once", "--cluster", "localnet", "--keypair", k, "--program-id", "not-a-key"],
            "--program-id is not a valid public key",
        ),
        (
            vec!["status", "--cluster", "localnet", "--fee-payer-pubkey", "not-a-key"],
            "--fee-payer-pubkey is not a valid public key",
        ),
        (
            vec!["once", "--cluster", "localnet", "--fee-payer-pubkey", &ghost],
            "--fee-payer-pubkey is for dry-run and status only; run/once need --keypair",
        ),
    ];
    for (a, msg) in cases {
        let rig = Rig::new(&e.chain);
        assert_eq!(rig.run(&a), 1, "{a:?}");
        assert!(rig.err_text().contains(msg), "{a:?}: {}", rig.err_text());
        assert_eq!((rig.sends(), rig.simulations()), (0, 0), "{a:?}");
        assert!(rig.events("pass_done").is_empty(), "{a:?}");
    }
}

#[test]
fn a_bad_number_with_a_lock_file_does_not_leave_the_lock_behind() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let lock = tmp.path("keeper.lock");
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock), "--max-retries", "x"])), 1);
    assert!(!lock.exists());
}

#[test]
fn status_through_the_cli_exits_0_when_healthy_and_sends_and_simulates_nothing() {
    let (e, claims) = world(5);
    let ghost_key = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &ghost_key, 0o600);
    let before = fingerprint(&e, &claims, &ghost_key.pubkey());
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("status", &key, &[])), 0, "{}", rig.err_text());
    assert_eq!((rig.sends(), rig.simulations()), (0, 0));
    assert_eq!(fingerprint(&e, &claims, &ghost_key.pubkey()), before);
    assert_eq!(rig.events("start")[0]["mode"], "status");
    assert!(rig.events("pass_done").is_empty(), "status runs no pass");
    assert!(rig.events("would_send").is_empty());
}

#[test]
fn status_through_the_cli_exits_10_with_an_alert_and_20_when_the_vault_cannot_be_read() {
    let e = Env::new();
    e.freeze(&e.usdt, true);
    let ghost = Pubkey::new_unique().to_string();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&["status", "--cluster", "localnet", "--fee-payer-pubkey", &ghost]), 10);
    assert_eq!(rig.alerts(), vec!["pool_frozen:usdt", "payer_balance_low:payer"]);
    assert_eq!(rig.sends(), 0);

    let rig = Rig::new(&e.chain);
    let bogus = Pubkey::new_unique().to_string();
    assert_eq!(rig.run(&["status", "--cluster", "localnet", "--fee-payer-pubkey", &ghost, "--program-id", &bogus]), 20);
    assert!(rig.err_text().contains("does not exist on this cluster"), "{}", rig.err_text());
}

#[test]
fn status_report_has_exact_values_and_sends_nothing() {
    let (e, claims) = world(3);
    e.freeze(&e.usdt, true);
    let mut k = e.keeper();
    let before = e.chain.sends.get();
    let v = k.status_report().unwrap();
    assert_eq!(e.chain.sends.get(), before);
    assert_eq!(e.chain.simulations.get(), 0);
    assert_eq!(v["cluster"], "localnet");
    assert_eq!(v["program"], core_vault::ID.to_string());
    assert_eq!(v["vault"], e.vault().to_string());
    assert_eq!(v["payer"], k.payer.to_string());
    assert_eq!(v["payer_balance_lamports"], 10_000_000_000u64);
    assert_eq!(v["chain_time"], T0);
    assert_eq!(
        v["cycle"],
        serde_json::json!({"id": 0, "active": false, "started_at": 0, "eligible": 0, "processed": 0})
    );
    assert_eq!(v["next"], "Begin");
    assert_eq!(v["earliest_begin"], 0);
    assert_eq!(v["open_claims"], serde_json::json!({"count": 3, "total": 33 * M}));
    assert_eq!(
        v["pools"],
        serde_json::json!({"usdc": {"amount": 5_000 * M, "frozen": false}, "usdt": {"amount": 0, "frozen": true}})
    );
    assert_eq!(v["coverage_ratio"], "151.515151", "rounded DOWN");
    assert_eq!(v["claims_seen"], claims.len());
    assert_eq!(v["alerts"], serde_json::json!([{"kind": "pool_frozen", "key": "usdt"}]));
    assert_eq!(v["products"], serde_json::json!([{"id": e.sector.to_string(), "active": true, "pause_reason": 0}]));
}

#[test]
fn status_report_without_getprogramaccounts_still_answers_with_claims_seen_null() {
    let (e, _claims) = world(2);
    e.chain.claims_unsupported.set(true);
    let mut k = e.keeper();
    let v = k.status_report().unwrap();
    assert_eq!(v["claims_seen"], Value::Null);
    assert_eq!(v["open_claims"]["count"], 2);
}

// ================================================================================================
// 10. the --claims-file fallback
// ================================================================================================

#[test]
fn without_getprogramaccounts_the_cycle_fails_loudly_and_names_the_claims_file() {
    let (e, claims) = world(3);
    e.chain.claims_unsupported.set(true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 20);
    let want = "this RPC provider does not support getProgramAccounts (getProgramAccounts is disabled by this provider); pass --claims-file <list of claim addresses>";
    let done = rig.last_pass();
    assert_eq!(done["hard_failure"], want);
    assert_eq!(rig.events("cannot_load_claims")[0]["error"], want);
    assert_eq!(rig.events("cannot_load_claims")[0]["level"], "error");
    // the claims source is checked before anything is sent: nothing went out, no cycle was begun, nobody was paid
    assert!(labels(&done, "sent").is_empty());
    assert_eq!(rig.sends(), 0);
    assert!(claims.iter().all(|c| e.claim(c).is_some()));
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn a_claims_file_with_comments_and_blank_lines_completes_the_cycle() {
    let (e, claims) = world(15);
    e.chain.claims_unsupported.set(true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let mut text = String::from("# claim addresses for the keeper\n\n");
    for (i, c) in claims.iter().enumerate() {
        match i % 3 {
            0 => text.push_str(&format!("{c}\n")),
            1 => text.push_str(&format!("   {c}   \n\n")),
            _ => text.push_str(&format!("# claim number {i}\n{c}\r\n")),
        }
    }
    text.push_str("\n# end\n");
    let list = tmp.write("claims.txt", text.as_bytes(), 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--claims-file", p(&list)])), 0, "{}", rig.err_text());
    assert_eq!(labels(&rig.last_pass(), "sent"), full_cycle_labels(&e.sector, 15));
    let vs = e.vault_state();
    assert_eq!((vs.cycle_id, vs.cycle_active, vs.open_claims_count), (1, false, 0));
    assert!(claims.iter().all(|c| e.claim(c).is_none()));
    assert_eq!(e.balance(&e.pool(&e.usdc)), 5_000 * M - (10..25).sum::<u64>() * M);
}

#[test]
fn a_claims_file_is_also_used_with_the_equals_form_and_ignores_unknown_addresses() {
    let (e, claims) = world(2);
    e.chain.claims_unsupported.set(true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let stranger = Pubkey::new_unique();
    let text = format!("{}\n{}\n{}\n", claims[0], stranger, claims[1]);
    let list = tmp.write("claims.txt", text.as_bytes(), 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[&format!("--claims-file={}", p(&list))])), 0, "{}", rig.err_text());
    assert_eq!(labels(&rig.last_pass(), "sent"), full_cycle_labels(&e.sector, 2));
}

#[test]
fn a_claims_file_that_misses_a_claim_ends_in_a_loud_stuck_cycle_not_a_silent_one() {
    let (e, claims) = world(3);
    e.chain.claims_unsupported.set(true);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let list = tmp.write("claims.txt", format!("{}\n{}\n", claims[0], claims[1]).as_bytes(), 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--claims-file", p(&list)])), 20);
    let stuck = rig.events("cycle_stuck");
    assert_eq!(stuck.len(), 1);
    assert_eq!(
        stuck[0]["error"],
        "the cycle expects 3 claims processed, 2 are, and none is left that this keeper can settle (quarantined: 0)"
    );
    let vs = e.vault_state();
    assert_eq!((vs.cycle_active, vs.cycle_processed_count, vs.cycle_eligible_count), (true, 2, 3));
    assert!(e.claim(&claims[2]).is_some());
    // listing it later finishes the same cycle: the keeper keeps no state
    let list2 = tmp.write("claims2.txt", format!("{}\n", claims[2]).as_bytes(), 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--claims-file", p(&list2)])), 0, "{}", rig.err_text());
    assert_eq!(labels(&rig.last_pass(), "sent"), vec!["settle_claims x1", "finalize_heartbeat"]);
    assert!(!e.vault_state().cycle_active);
}

#[test]
fn a_garbage_line_in_the_claims_file_fails_and_echoes_at_most_its_first_50_characters() {
    let (e, claims) = world(3);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let garbage = format!("{}TAIL-SECRET-MARKER{}", "g".repeat(50), "z".repeat(40));
    let list = tmp.write("claims.txt", format!("# c\n{}\n{garbage}\n", claims[0]).as_bytes(), 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--claims-file", p(&list)])), 20);
    let want = format!("--claims-file: '{}' is not a public key", "g".repeat(50));
    assert_eq!(rig.last_pass()["hard_failure"], want);
    let hay = rig.everything();
    assert_absent("TAIL-SECRET-MARKER", &hay);
    assert_absent(&"g".repeat(51), &hay);
    assert!(hay[0].1.contains(&want));
    assert_eq!(
        rig.sends(),
        0,
        "the claims source is checked BEFORE anything is sent: no cycle is begun that cannot be settled"
    );
}

#[test]
fn an_unreadable_claims_file_fails_without_naming_what_is_in_it() {
    let (e, _claims) = world(1);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let missing = tmp.path("absent.txt");
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--claims-file", p(&missing)])), 20);
    assert_eq!(rig.last_pass()["hard_failure"], "cannot read the --claims-file");
}

/// Surprising but harmless today: `dry-run` swallows a claims-file/enumeration failure. It prints reconcile and
/// begin, then silently stops: no batch, no finalize, no error line, exit 0. A reader of the dry-run output
/// cannot tell the claims could not be read.
#[test]
fn dry_run_silently_drops_the_batches_when_claims_cannot_be_listed() {
    let (e, _claims) = world(3);
    e.chain.claims_unsupported.set(true);
    let kp = funded(&e);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&["dry-run", "--cluster", "localnet", "--fee-payer-pubkey", &kp.pubkey().to_string()]);
    assert_eq!(code, 0);
    let would: Vec<String> =
        rig.events("would_send").iter().map(|l| l["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(would, vec![format!("reconcile_product {}", e.sector), "begin_heartbeat".to_string()]);
    assert!(rig.events("cannot_load_claims").is_empty(), "no error is logged");
    assert_eq!(rig.err_text(), "");
}

// ================================================================================================
// 11. the lock file
// ================================================================================================

#[test]
fn a_second_start_with_the_same_lock_file_is_refused_while_the_first_holds_it() {
    let (e, _claims) = world(3);
    let kp = funded(&e);
    let kp2 = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let key2 = tmp.key("hot2.json", &kp2, 0o600);
    let lock = tmp.path("keeper.lock");

    let inner_code = Rc::new(Cell::new(-1));
    let inner_err = Rc::new(RefCell::new(String::new()));
    let inner_sends = Rc::new(Cell::new(u32::MAX));
    let lock_during = Rc::new(RefCell::new(None::<String>));
    let rig = Rig::new(&e.chain);
    {
        let (chain, lock, key2) = (e.chain.clone(), lock.clone(), key2.clone());
        let (c, er, s, held) = (inner_code.clone(), inner_err.clone(), inner_sends.clone(), lock_during.clone());
        *rig.spy.on_genesis.borrow_mut() = Some(Box::new(move || {
            // the first process has taken its lock and is about to read the genesis hash
            *held.borrow_mut() = std::fs::read_to_string(&lock).ok();
            let second = Rig::new(&chain);
            c.set(second.run(&args("once", &key2, &["--lock-file", p(&lock)])));
            *er.borrow_mut() = second.err_text();
            s.set(second.sends());
            assert!(lock.exists(), "the refused second start must not remove the first one's lock");
        }));
    }
    let code = rig.run(&args("once", &key, &["--lock-file", p(&lock)]));
    assert_eq!(code, 0, "{}", rig.err_text());
    assert_eq!(inner_code.get(), 2);
    assert!(
        inner_err.borrow().contains(&format!("another keeper holds the lock file {}", p(&lock))),
        "{}",
        inner_err.borrow()
    );
    assert!(inner_err.borrow().contains("remove it if that is stale"));
    assert_eq!(inner_sends.get(), 0);
    assert_eq!(
        lock_during.borrow().as_deref(),
        Some(std::process::id().to_string().as_str()),
        "the lock holds the pid"
    );
    // the first run completed its cycle and removed its lock
    assert_eq!(e.vault_state().cycle_id, 1);
    assert!(!lock.exists(), "the lock file is removed when the run ends");
}

#[test]
fn a_stale_lock_file_refuses_the_start_and_is_left_untouched() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let lock = tmp.write("keeper.lock", b"99999", 0o644);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock)])), 2);
    assert!(rig.err_text().contains("another keeper holds the lock file"));
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), "99999");
    assert_eq!(rig.sends(), 0);
    assert!(rig.events("start").is_empty());
}

#[test]
fn the_lock_file_is_removed_after_success_after_a_hard_failure_and_after_a_genesis_refusal() {
    let tmp = Tmp::new();
    let lock = tmp.path("keeper.lock");

    // success
    let (e, _claims) = world(3);
    let kp = funded(&e);
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock)])), 0);
    assert!(!lock.exists());

    // hard failure (no vault under that program id)
    let rig = Rig::new(&e.chain);
    let bogus = Pubkey::new_unique().to_string();
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock), "--program-id", &bogus])), 20);
    assert!(!lock.exists());

    // genesis refusal
    *e.chain.genesis.borrow_mut() = DEVNET_GENESIS.into();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock)])), 2);
    assert!(!lock.exists());

    // the lock can be taken again by the next start
    *e.chain.genesis.borrow_mut() = LOCAL_GENESIS.into();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock)])), 0);
    assert!(!lock.exists());
}

#[test]
fn a_refused_key_never_creates_the_lock_file() {
    let e = Env::new();
    let tmp = Tmp::new();
    let key = tmp.key("admin.json", &dup(&e.sl8), 0o600);
    let lock = tmp.path("keeper.lock");
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &["--lock-file", p(&lock)])), 2);
    assert!(!lock.exists());
}

// ================================================================================================
// 12. only the four permissionless instructions are ever built
// ================================================================================================

fn decode_all(rig: &Rig) -> Vec<Transaction> {
    rig.spy
        .txs
        .borrow()
        .iter()
        .map(|b| bincode::deserialize::<Transaction>(b).expect("a well formed transaction"))
        .collect()
}

fn check_only_permissionless(priority_fee: Option<&str>) {
    let (e, _claims) = world(15);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    let extra: Vec<&str> = match priority_fee {
        Some(f) => vec!["--priority-fee-microlamports", f],
        None => vec![],
    };
    assert_eq!(rig.run(&args("once", &key, &extra)), 0, "{}", rig.err_text());
    let txs = decode_all(&rig);
    assert_eq!(txs.len(), 6);
    let allowed: [(&str, [u8; 8]); 4] = [
        ("reconcile_product", core_vault::instruction::ReconcileProduct::DISCRIMINATOR.try_into().unwrap()),
        ("begin_heartbeat", core_vault::instruction::BeginHeartbeat::DISCRIMINATOR.try_into().unwrap()),
        ("settle_claims", core_vault::instruction::SettleClaims::DISCRIMINATOR.try_into().unwrap()),
        ("finalize_heartbeat", core_vault::instruction::FinalizeHeartbeat::DISCRIMINATOR.try_into().unwrap()),
    ];
    let admin: Vec<Pubkey> = ADMIN_PUBKEYS.iter().map(|(_, k)| *k).collect();
    let mut seen = vec![];
    for tx in &txs {
        let m = &tx.message;
        // the fee payer is the only signer and the transaction is signed correctly
        assert_eq!(m.header.num_required_signatures, 1);
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(m.account_keys[0], kp.pubkey());
        assert!(tx.verify_with_results().into_iter().all(|ok| ok), "signature verifies");
        for k in &m.account_keys {
            assert!(!admin.contains(k), "an admin key appears in a transaction: {k}");
        }
        let mut vault_ixs = 0;
        for ix in &m.instructions {
            let program = m.account_keys[ix.program_id_index as usize];
            if program == COMPUTE_BUDGET_ID {
                assert!(priority_fee.is_some(), "ComputeBudget only with a priority fee");
                assert!(ix.accounts.is_empty());
                continue;
            }
            assert_eq!(program, core_vault::ID, "only the vault program is called");
            vault_ixs += 1;
            let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
            let name = allowed
                .iter()
                .find(|(_, d)| *d == disc)
                .unwrap_or_else(|| panic!("discriminator {disc:?} is not one of the four"))
                .0;
            seen.push(name);
            // the caller is the first account and the only signer in the instruction
            assert_eq!(m.account_keys[ix.accounts[0] as usize], kp.pubkey());
            for &a in &ix.accounts {
                assert!(!m.is_signer(a as usize) || a == 0, "only the fee payer signs");
                assert!(!admin.contains(&m.account_keys[a as usize]));
            }
        }
        assert_eq!(vault_ixs, 1, "one vault instruction per transaction");
        match priority_fee {
            None => assert_eq!(m.instructions.len(), 1),
            Some(f) => {
                assert_eq!(m.instructions.len(), 3);
                let (limit, price) = (&m.instructions[0], &m.instructions[1]);
                assert_eq!(m.account_keys[limit.program_id_index as usize], COMPUTE_BUDGET_ID);
                assert_eq!(m.account_keys[price.program_id_index as usize], COMPUTE_BUDGET_ID);
                assert_eq!(limit.data[0], 2);
                let units = u32::from_le_bytes(limit.data[1..5].try_into().unwrap());
                assert!((1_000..=1_400_000).contains(&units), "{units}");
                assert_eq!(price.data, [vec![3u8], f.parse::<u64>().unwrap().to_le_bytes().to_vec()].concat());
            }
        }
    }
    assert_eq!(
        seen,
        vec![
            "reconcile_product",
            "begin_heartbeat",
            "settle_claims",
            "settle_claims",
            "settle_claims",
            "finalize_heartbeat"
        ]
    );
    assert!(e.chain.landed.borrow().len() == 6);
    assert_eq!(
        setl8_keeper::ixs::ALLOWED,
        ["reconcile_product", "begin_heartbeat", "settle_claims", "finalize_heartbeat"]
    );
}

#[test]
fn a_full_cycle_builds_only_the_four_permissionless_instructions_signed_by_the_fee_payer_alone() {
    check_only_permissionless(None);
}

#[test]
fn with_a_priority_fee_the_only_extra_instructions_are_compute_budget_ones() {
    check_only_permissionless(Some("7"));
}

#[test]
fn settle_instructions_list_only_claims_and_their_owners_token_accounts() {
    let (e, claims) = world(6);
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let wallets: Vec<Pubkey> = (0..6).map(|i| e.wallet(i)).collect();
    let rig = Rig::new(&e.chain);
    assert_eq!(rig.run(&args("once", &key, &[])), 0);
    let txs = decode_all(&rig);
    let settle = &txs[2].message;
    assert_eq!(settle.instructions.len(), 1);
    let ix = &settle.instructions[0];
    assert_eq!(
        &ix.data[..],
        core_vault::instruction::SettleClaims::DISCRIMINATOR,
        "no arguments after the discriminator"
    );
    let keys: Vec<Pubkey> = ix.accounts.iter().map(|&a| settle.account_keys[a as usize]).collect();
    assert_eq!(keys.len(), 7 + 3 * 6);
    let mut from_tx: Vec<Pubkey> = keys[7..].chunks(3).map(|c| c[0]).collect();
    let mut want = claims.clone();
    from_tx.sort_by_key(|k| k.to_bytes());
    want.sort_by_key(|k| k.to_bytes());
    assert_eq!(from_tx, want);
    for t in keys[7..].chunks(3) {
        let owner = (0..6).find(|&i| claims[i] == t[0]).unwrap();
        assert_eq!(t[1], ata(&wallets[owner], &e.usdc));
        assert_eq!(t[2], ata(&wallets[owner], &e.usdt));
    }
}

// ================================================================================================
// 13. help, version and the shape of the command line
// ================================================================================================

fn quiet_rig() -> Rig {
    Rig::new(&Env::new().chain)
}

#[test]
fn unknown_modes_options_duplicates_and_missing_values_are_usage_errors() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["frobnicate"], "error: unknown mode 'frobnicate' (run, once, dry-run, status)"),
        (vec!["--cluster", "localnet"], "error: unknown mode '--cluster' (run, once, dry-run, status)"),
        (vec!["Once"], "error: unknown mode 'Once' (run, once, dry-run, status)"),
        (vec!["once", "--bogus", "1"], "error: unknown option --bogus"),
        (vec!["once", "--bogus=1"], "error: unknown option --bogus"),
        (vec!["once", "extra"], "error: unexpected argument 'extra'"),
        (vec!["once", "-x"], "error: unexpected argument '-x'"),
        (vec!["once", "--cluster", "localnet", "--cluster", "devnet"], "error: --cluster given twice"),
        (vec!["once", "--cluster=localnet", "--cluster", "devnet"], "error: --cluster given twice"),
        (vec!["once", "--keypair"], "error: --keypair needs a value"),
        (vec!["once", "--keypair", "--cluster", "localnet"], "error: --keypair needs a value"),
        (vec!["once", "--cluster", "localnet", "--webhook"], "error: --webhook needs a value"),
        (vec!["once", "--cluster", "localnet", "--lock-file", "--keypair"], "error: --lock-file needs a value"),
        (vec!["once", "--i-understand-this-is-mainnet=1"], "error: --i-understand-this-is-mainnet takes no value"),
    ];
    for (a, msg) in cases {
        let rig = quiet_rig();
        assert_eq!(rig.run(&a), 1, "{a:?}");
        assert_eq!(rig.err.borrow().as_slice(), [msg.to_string()], "{a:?}");
        assert_eq!((rig.made.get(), rig.calls(), rig.sends()), (0, 0, 0), "{a:?}");
    }
}

#[test]
fn the_equals_form_and_the_space_form_are_the_same_option() {
    let e = Env::new();
    let kp = funded(&e);
    let tmp = Tmp::new();
    let key = tmp.key("hot.json", &kp, 0o600);
    let rig = Rig::new(&e.chain);
    let code = rig.run(&[
        "once".to_string(),
        "--cluster=localnet".to_string(),
        format!("--keypair={}", p(&key)),
        "--max-sends-per-run=7".to_string(),
    ]);
    assert_eq!(code, 0, "{}", rig.err_text());
    assert_eq!(rig.events("start")[0]["payer"], kp.pubkey().to_string());
}

#[test]
fn no_arguments_is_a_usage_error_and_help_and_version_exit_0_in_process() {
    let rig = quiet_rig();
    assert_eq!(rig.run::<&str>(&[]), 1);
    for a in [
        vec!["--help"],
        vec!["help"],
        vec!["-h"],
        vec!["--version"],
        vec!["-V"],
        vec!["version"],
        vec!["once", "--help"],
        vec!["run", "-h"],
        vec!["dry-run", "--cluster", "localnet", "--help"],
        vec!["status", "--bogus", "--help"],
    ] {
        let rig = quiet_rig();
        assert_eq!(rig.run(&a), 0, "{a:?}");
        assert_eq!(rig.err_text(), "", "{a:?}");
        assert_eq!((rig.made.get(), rig.calls()), (0, 0));
    }
}

fn binary() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_setl8-keeper"));
    c.env_remove("SETL8_KEEPER_WEBHOOK");
    c
}

fn out(o: &std::process::Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn the_help_text_mentions_every_mode_and_every_option() {
    let mut texts = vec![];
    for a in [&["--help"][..], &["help"], &["-h"], &["once", "--help"], &["status", "--keypair", "--help"]] {
        let o = binary().args(a).output().unwrap();
        assert_eq!(o.status.code(), Some(0), "{a:?}");
        let (so, se) = out(&o);
        assert_eq!(se, "", "{a:?}");
        texts.push(so);
    }
    assert!(texts.iter().all(|t| *t == texts[0]), "every spelling prints the same text");
    let help = &texts[0];
    for mode in ["run ", "once ", "dry-run ", "status "] {
        assert!(help.lines().any(|l| l.trim_start().starts_with(mode)), "mode '{mode}' is described: {help}");
    }
    assert!(help.contains("USAGE: setl8-keeper <mode> --cluster"));
    for opt in [
        "--cluster",
        "--rpc",
        "--keypair",
        "--fee-payer-pubkey",
        "--program-id",
        "--interval",
        "--priority-fee-microlamports",
        "--max-sends-per-run",
        "--max-fee-lamports-per-run",
        "--min-balance-lamports",
        "--max-retries",
        "--claims-file",
        "--log-file",
        "--webhook",
        "--lock-file",
        "--i-understand-this-is-mainnet",
    ] {
        assert!(help.contains(opt), "option {opt} is documented");
    }
    assert!(help.contains("SETL8_KEEPER_WEBHOOK"));
    assert!(help.contains("0 nothing to do or progress made, 10 an alert condition is present"));
    assert!(help.contains("20 hard failure"));
}

#[test]
fn version_prints_the_version_and_which_admin_keys_are_compiled_in() {
    for a in ["--version", "-V", "version"] {
        let o = binary().arg(a).output().unwrap();
        assert_eq!(o.status.code(), Some(0));
        let (so, se) = out(&o);
        assert_eq!(so, format!("setl8-keeper {} (PUBLIC TEST admin keys compiled in)\n", env!("CARGO_PKG_VERSION")));
        assert_eq!(se, "");
    }
    assert_eq!(setl8_keeper::cli::VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn the_binary_exits_1_with_the_usage_on_stderr_for_no_arguments_and_for_an_unknown_mode() {
    let o = binary().output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let (so, se) = out(&o);
    assert_eq!(so, "");
    assert!(se.contains("USAGE: setl8-keeper <mode>"));

    let o = binary().arg("frobnicate").output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let (so, se) = out(&o);
    assert_eq!(so, "");
    assert_eq!(se.trim(), "error: unknown mode 'frobnicate' (run, once, dry-run, status)");
}

#[test]
fn the_real_binary_refuses_an_admin_key_without_printing_it_and_without_touching_the_network() {
    let e = Env::new();
    let tmp = Tmp::new();
    for (name, kp) in [("sl8", dup(&e.sl8)), ("rov", dup(&e.rov))] {
        let key = tmp.key(&format!("{name}.json"), &kp, 0o600);
        let o = binary().args(args("once", &key, &[])).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{name}");
        let (so, se) = out(&o);
        assert!(se.contains("refused") && se.contains("admin"), "{se}");
        assert_eq!(so, "", "nothing on stdout: no start event was logged");
        assert_no_secret(&kp, &[("stdout", so), ("stderr", se)]);
    }
}

#[test]
fn the_real_binary_refuses_loose_key_files_garbage_and_mainnet_without_the_flag() {
    let tmp = Tmp::new();
    let kp = Keypair::new();
    let loose = tmp.key("loose.json", &kp, 0o644);
    let o = binary().args(args("once", &loose, &[])).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    let (so, se) = out(&o);
    assert!(se.contains("chmod 600"), "{se}");
    assert_no_secret(&kp, &[("stdout", so), ("stderr", se)]);

    let junk = tmp.write("junk.json", b"JUNK-MARKER-1234", 0o600);
    let o = binary().args(args("once", &junk, &[])).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let (so, se) = out(&o);
    assert!(se.contains("is not a keypair file") && !se.contains("JUNK-MARKER-1234") && so.is_empty(), "{se}");

    let o = binary().args(["once", "--cluster", "mainnet", "--keypair", p(&tmp.path("none.json"))]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    let (_, se) = out(&o);
    assert!(se.contains("--i-understand-this-is-mainnet"), "{se}");
}

// ================================================================================================
// 14. (skipped) a panic inside the run: the CLI offers no injection point; `main` wraps `execute` in
//     catch_unwind and prints a fixed message with exit code 70, which can only be tested by crashing the binary.
// ================================================================================================
