//! The rehearsal context: keys, transaction building and sending, recording of results.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_spl::token::spl_token::{self, solana_program::program_pack::Pack, state::Account as TokAcc};
use base64::Engine;
use serde_json::{json, Value};
use setl8_admin::admin_ix::Keys;
use setl8_admin::error::{Error, Result};
use setl8_admin::host::Host;
use setl8_admin::rpc::{HttpRpc, Rpc};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;

use crate::jrpc::{Jrpc, RpcErr};

pub const ATA_PROGRAM: Pubkey = anchor_lang::prelude::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// (program error if any, logs, compute units, transaction size)
pub type Sim = (Option<Value>, Vec<String>, Option<u64>, usize);

#[derive(Clone, Debug, Default)]
pub struct TxInfo {
    pub sig: String,
    pub cu: Option<u64>,
    pub fee: Option<u64>,
    pub size: usize,
    pub ms: u128,
    pub logs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub step: String,
    pub what: String,
    pub expected: String,
    pub actual: String,
    pub ok: bool,
    pub sig: String,
    pub cu: Option<u64>,
    pub fee: Option<u64>,
    pub size: usize,
    pub ms: u128,
}

/// The scripted-confirmation host the admin tool runs under: it answers the "retype the first
/// 8 characters of the message hash" prompt from the hash the tool itself just printed.
pub struct DrvHost {
    pub out: String,
    pub err: String,
    pub rpc_calls: usize,
}

impl Host for DrvHost {
    fn out(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn err(&mut self, s: &str) {
        self.err.push_str(s);
    }
    fn prompt(&mut self, q: &str) -> Result<String> {
        if q.contains("retype the first 8 characters") {
            let at = self.out.rfind("Message SHA-256: ").ok_or_else(|| Error("refused: no hash was printed".into()))?;
            return Ok(self.out[at + 17..at + 25].to_string());
        }
        Err(Error(format!("refused: unexpected prompt in the rehearsal: {q}")))
    }
    fn rpc(&mut self, url: &str) -> Result<Box<dyn Rpc>> {
        self.rpc_calls += 1;
        Ok(Box::new(HttpRpc::new(url)))
    }
}

pub struct Ctx {
    pub rpc: Jrpc,
    pub url: String,
    pub cluster: String,
    pub genesis: String,
    pub dir: PathBuf,
    pub work: PathBuf,
    pub keys: Keys,
    pub sector: Pubkey,
    pub usdc: Pubkey,
    pub usdt: Pubkey,
    pub kp: HashMap<String, Keypair>,
    pub rows: Vec<Row>,
    pub jsonl: std::fs::File,
    pub nonce: Option<Pubkey>,
    /// (instruction, vault-program CU from the logs, whole-transaction CU)
    pub cus: Vec<(String, u64, u64)>,
    /// `--extra-claims N`: how many extra traders (generated in memory, never written anywhere) the extra-claims step adds.
    pub extra_claims: usize,
}

pub fn ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[wallet.as_ref(), spl_token::ID.as_ref(), mint.as_ref()], &ATA_PROGRAM).0
}

impl Ctx {
    pub fn key(&self, name: &str) -> &Keypair {
        self.kp.get(name).unwrap_or_else(|| panic!("key {name} not loaded"))
    }
    pub fn pk(&self, name: &str) -> Pubkey {
        self.key(name).pubkey()
    }

    pub fn latest_blockhash(&self) -> Result<Hash> {
        let r = self
            .rpc
            .call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))
            .map_err(|e| Error(e.to_string()))?;
        r["value"]["blockhash"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error("no blockhash".into()))
    }

    pub fn account(&self, k: &Pubkey) -> Result<Option<(u64, Pubkey, Vec<u8>)>> {
        let r = self
            .rpc
            .call("getAccountInfo", json!([k.to_string(), {"encoding": "base64", "commitment": "confirmed"}]))
            .map_err(|e| Error(e.to_string()))?;
        let v = &r["value"];
        if v.is_null() {
            return Ok(None);
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(v["data"][0].as_str().unwrap_or(""))
            .map_err(|_| Error("bad account data".into()))?;
        Ok(Some((
            v["lamports"].as_u64().unwrap_or(0),
            v["owner"].as_str().unwrap_or("").parse().unwrap_or_default(),
            data,
        )))
    }

    pub fn exists(&self, k: &Pubkey) -> bool {
        matches!(self.account(k), Ok(Some(_)))
    }

    pub fn token(&self, k: &Pubkey) -> Option<TokAcc> {
        self.account(k).ok().flatten().and_then(|(_, _, d)| TokAcc::unpack(&d).ok())
    }

    pub fn balance(&self, k: &Pubkey) -> u64 {
        self.token(k).map(|t| t.amount).unwrap_or(0)
    }

    fn build(&self, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) -> Result<(Vec<u8>, String, usize)> {
        let bh = self.latest_blockhash()?;
        let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &bh);
        let bytes = msg.serialize();
        let n = msg.header.num_required_signatures as usize;
        let mut tx = vec![n as u8];
        let mut first = String::new();
        for (i, k) in msg.account_keys.iter().take(n).enumerate() {
            let kp = std::iter::once(&payer)
                .chain(signers.iter())
                .find(|c| c.pubkey() == *k)
                .ok_or_else(|| Error(format!("no key for signer {k}")))?;
            let sig = kp.try_sign_message(&bytes).map_err(|_| Error("sign failed".into()))?;
            if i == 0 {
                first = sig.to_string();
            }
            tx.extend_from_slice(sig.as_ref());
        }
        tx.extend_from_slice(&bytes);
        let size = tx.len();
        Ok((tx, first, size))
    }

    fn b64(tx: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(tx)
    }

    /// Waits for `sig` to be confirmed. Returns Err(program error JSON) if it landed failed.
    fn wait(&self, sig: &str, timeout_s: u64) -> std::result::Result<(), String> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(timeout_s) {
            if let Ok(r) = self.rpc.call("getSignatureStatuses", json!([[sig], {"searchTransactionHistory": false}])) {
                let st = &r["value"][0];
                if !st.is_null() {
                    if !st["err"].is_null() {
                        return Err(st["err"].to_string());
                    }
                    let c = st["confirmationStatus"].as_str().unwrap_or("");
                    if c == "confirmed" || c == "finalized" {
                        return Ok(());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(600));
        }
        Err("timeout: not confirmed".into())
    }

    /// CU, fee and logs of a landed transaction (retries while the node indexes it).
    pub fn meta(&self, sig: &str) -> (Option<u64>, Option<u64>, Vec<String>) {
        for _ in 0..30 {
            if let Ok(r) = self.rpc.call(
                "getTransaction",
                json!([sig, {"encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}]),
            ) {
                if !r.is_null() {
                    let m = &r["meta"];
                    let logs = m["logMessages"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    return (m["computeUnitsConsumed"].as_u64(), m["fee"].as_u64(), logs);
                }
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        (None, None, vec![])
    }

    /// Sends and waits; the transaction must succeed.
    pub fn send_ok(&mut self, ixs: &[Instruction], payer: &str, signers: &[&str]) -> Result<TxInfo> {
        let payer_kp = self.key(payer);
        let sk: Vec<&Keypair> = signers.iter().map(|n| self.key(n)).collect();
        let (tx, sig, size) = self.build(ixs, payer_kp, &sk)?;
        let t0 = Instant::now();
        let got = self
            .rpc
            .call(
                "sendTransaction",
                json!([Self::b64(&tx), {"encoding": "base64", "preflightCommitment": "confirmed"}]),
            )
            .map_err(|e| Error(format!("send failed: {} {}", e.message, summarize_logs(&e))))?;
        let sig = got.as_str().unwrap_or(&sig).to_string();
        self.wait(&sig, 90).map_err(|e| Error(format!("tx {sig} did not succeed: {e}")))?;
        let ms = t0.elapsed().as_millis();
        let (cu, fee, logs) = self.meta(&sig);
        Ok(TxInfo { sig, cu, fee, size, ms, logs })
    }

    /// Sends WITHOUT preflight (so a failing transaction really lands) and returns the on-chain error.
    pub fn send_fail_onchain(
        &mut self,
        ixs: &[Instruction],
        payer: &str,
        signers: &[&str],
    ) -> Result<(TxInfo, String)> {
        let payer_kp = self.key(payer);
        let sk: Vec<&Keypair> = signers.iter().map(|n| self.key(n)).collect();
        let (tx, sig, size) = self.build(ixs, payer_kp, &sk)?;
        let t0 = Instant::now();
        let got = self
            .rpc
            .call("sendTransaction", json!([Self::b64(&tx), {"encoding": "base64", "skipPreflight": true}]))
            .map_err(|e| Error(e.to_string()))?;
        let sig = got.as_str().unwrap_or(&sig).to_string();
        match self.wait(&sig, 90) {
            Ok(()) => Err(Error(format!("tx {sig} unexpectedly succeeded"))),
            Err(e) => {
                let ms = t0.elapsed().as_millis();
                let (cu, fee, logs) = self.meta(&sig);
                Ok((TxInfo { sig, cu, fee, size, ms, logs }, e))
            }
        }
    }

    /// Simulates (nothing lands). Returns (error JSON if any, logs, units).
    pub fn simulate(&mut self, ixs: &[Instruction], payer: &str, signers: &[&str]) -> Result<Sim> {
        let payer_kp = self.key(payer);
        let sk: Vec<&Keypair> = signers.iter().map(|n| self.key(n)).collect();
        let (tx, _sig, size) = self.build(ixs, payer_kp, &sk)?;
        let r = self
            .rpc
            .call(
                "simulateTransaction",
                json!([Self::b64(&tx), {"encoding": "base64", "sigVerify": true, "commitment": "confirmed"}]),
            )
            .map_err(|e| Error(format!("simulate: {}", e.message)))?;
        let v = &r["value"];
        let logs = v["logs"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let err = if v["err"].is_null() { None } else { Some(v["err"].clone()) };
        Ok((err, logs, v["unitsConsumed"].as_u64(), size))
    }

    /// Records a result row (and a JSON line in the log file).
    pub fn record(&mut self, step: &str, what: &str, expected: &str, actual: &str, ok: bool, t: Option<&TxInfo>) {
        let row = Row {
            step: step.into(),
            what: what.into(),
            expected: expected.into(),
            actual: actual.into(),
            ok,
            sig: t.map(|t| t.sig.clone()).unwrap_or_default(),
            cu: t.and_then(|t| t.cu),
            fee: t.and_then(|t| t.fee),
            size: t.map(|t| t.size).unwrap_or(0),
            ms: t.map(|t| t.ms).unwrap_or(0),
        };
        println!(
            "[{}] {:<5} {}  | expected: {} | actual: {}{}{}",
            if ok { "PASS" } else { "FAIL" },
            step,
            what,
            expected,
            actual,
            if row.sig.is_empty() { String::new() } else { format!(" | sig {}", row.sig) },
            row.cu.map(|c| format!(" | {c} CU")).unwrap_or_default()
        );
        let logs = t.map(|t| t.logs.clone()).unwrap_or_default();
        let line = json!({"logs": logs, "step": row.step, "what": row.what, "expected": row.expected, "actual": row.actual, "ok": row.ok, "sig": row.sig, "cu": row.cu, "fee": row.fee, "size": row.size, "ms": row.ms});
        let _ = writeln!(self.jsonl, "{line}");
        self.rows.push(row);
    }

    /// Assert-and-record helper.
    pub fn check(
        &mut self,
        step: &str,
        what: &str,
        expected: impl ToString,
        actual: impl ToString,
        t: Option<&TxInfo>,
    ) -> bool {
        let (e, a) = (expected.to_string(), actual.to_string());
        let ok = e == a;
        self.record(step, what, &e, &a, ok, t);
        ok
    }

    pub fn check_true(
        &mut self,
        step: &str,
        what: &str,
        expected: &str,
        actual: &str,
        ok: bool,
        t: Option<&TxInfo>,
    ) -> bool {
        self.record(step, what, expected, actual, ok, t);
        ok
    }
}

pub fn summarize_logs(e: &RpcErr) -> String {
    let logs = e.data["logs"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" / "))
        .unwrap_or_default();
    let err = e.data["err"].to_string();
    format!("[err {err}] {}", logs.chars().take(400).collect::<String>())
}

/// `{"InstructionError":[0,{"Custom":6012}]}` to 6012.
pub fn custom_code(err: &Value) -> Option<u64> {
    err["InstructionError"][1]["Custom"].as_u64()
}

/// Sum of the vault program's own `consumed N of M compute units` log lines.
pub fn program_cu(logs: &[String], program: &Pubkey) -> Option<u64> {
    let prefix = format!("Program {program} consumed ");
    let mut total = None;
    for l in logs {
        if let Some(rest) = l.strip_prefix(&prefix) {
            if let Some(n) = rest.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) {
                total = Some(total.unwrap_or(0) + n);
            }
        }
    }
    total
}
