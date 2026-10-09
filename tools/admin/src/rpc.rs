//! The only network code. A tiny JSON-RPC client over `ureq`, behind a trait so the rest of
//! the tool (and its tests) never need a network. RPC URLs can carry API keys, so error
//! messages name the host only.

use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use base64::Engine;
use serde_json::{json, Value};

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcAccount {
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
    pub executable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimResult {
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub units: Option<u64>,
}

pub trait Rpc {
    fn genesis_hash(&self) -> Result<String>;
    fn account(&self, key: &Pubkey) -> Result<Option<RpcAccount>>;
    /// Accounts owned by `program` whose first 8 bytes equal `discriminator`.
    fn program_accounts(&self, program: &Pubkey, discriminator: &[u8; 8]) -> Result<Vec<(Pubkey, RpcAccount)>>;
    fn latest_blockhash(&self) -> Result<String>;
    fn min_balance_for_rent(&self, data_len: usize) -> Result<u64>;
    fn simulate(&self, tx_bytes: &[u8]) -> Result<SimResult>;
    /// Returns the first signature (base58).
    fn send(&self, tx_bytes: &[u8]) -> Result<String>;
    /// Blocks until the signature is confirmed, or fails (including when the transaction failed).
    fn confirm(&self, signature: &str) -> Result<()>;
}

pub fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest).to_string()
}

pub struct HttpRpc {
    url: String,
    agent: ureq::Agent,
}

impl HttpRpc {
    pub fn new(url: &str) -> HttpRpc {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build();
        HttpRpc { url: url.to_string(), agent }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let host = host_of(&self.url);
        let resp = self.agent.post(&self.url).send_json(body).map_err(|e| match e {
            ureq::Error::Status(code, _) => Error(format!("rpc {host}: HTTP {code} for {method}")),
            ureq::Error::Transport(t) => Error(format!("rpc {host}: cannot reach the node ({:?})", t.kind())),
        })?;
        let v: Value = resp.into_json().map_err(|_| Error(format!("rpc {host}: answer for {method} is not JSON")))?;
        if let Some(e) = v.get("error") {
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("error");
            return Err(Error(format!("rpc {host}: {method} failed: {msg}")));
        }
        v.get("result").cloned().ok_or_else(|| Error(format!("rpc {host}: {method} answer has no result")))
    }
}

fn parse_account(v: &Value) -> Result<RpcAccount> {
    let bad = || Error("rpc: malformed account in answer".to_string());
    let data_b64 = v.get("data").and_then(|d| d.get(0)).and_then(|s| s.as_str()).ok_or_else(bad)?;
    Ok(RpcAccount {
        lamports: v.get("lamports").and_then(|x| x.as_u64()).ok_or_else(bad)?,
        owner: v.get("owner").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).ok_or_else(bad)?,
        data: base64::engine::general_purpose::STANDARD.decode(data_b64).map_err(|_| bad())?,
        executable: v.get("executable").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

impl Rpc for HttpRpc {
    fn genesis_hash(&self) -> Result<String> {
        self.call("getGenesisHash", json!([]))?
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error("rpc: genesis hash is not a string".into()))
    }

    fn account(&self, key: &Pubkey) -> Result<Option<RpcAccount>> {
        let r =
            self.call("getAccountInfo", json!([key.to_string(), {"encoding": "base64", "commitment": "confirmed"}]))?;
        match r.get("value") {
            Some(Value::Null) | None => Ok(None),
            Some(v) => Ok(Some(parse_account(v)?)),
        }
    }

    fn program_accounts(&self, program: &Pubkey, disc: &[u8; 8]) -> Result<Vec<(Pubkey, RpcAccount)>> {
        let r = self.call(
            "getProgramAccounts",
            json!([program.to_string(), {
                "encoding": "base64",
                "commitment": "confirmed",
                "filters": [{"memcmp": {"offset": 0, "bytes": bs58::encode(disc).into_string()}}]
            }]),
        )?;
        let arr = r.as_array().ok_or_else(|| Error("rpc: getProgramAccounts answer is not a list".into()))?;
        let mut out = vec![];
        for item in arr {
            let key: Pubkey = item
                .get("pubkey")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error("rpc: malformed program account".into()))?;
            out.push((
                key,
                parse_account(item.get("account").ok_or_else(|| Error("rpc: malformed program account".into()))?)?,
            ));
        }
        Ok(out)
    }

    fn latest_blockhash(&self) -> Result<String> {
        let r = self.call("getLatestBlockhash", json!([{"commitment": "finalized"}]))?;
        r.get("value")
            .and_then(|v| v.get("blockhash"))
            .and_then(|b| b.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| Error("rpc: no blockhash in answer".into()))
    }

    fn min_balance_for_rent(&self, data_len: usize) -> Result<u64> {
        self.call("getMinimumBalanceForRentExemption", json!([data_len]))?
            .as_u64()
            .ok_or_else(|| Error("rpc: rent answer is not a number".into()))
    }

    fn simulate(&self, tx: &[u8]) -> Result<SimResult> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(tx);
        let r = self.call(
            "simulateTransaction",
            json!([b64, {"encoding": "base64", "sigVerify": true, "commitment": "confirmed"}]),
        )?;
        let v = r.get("value").ok_or_else(|| Error("rpc: simulation has no value".into()))?;
        Ok(SimResult {
            err: v.get("err").filter(|e| !e.is_null()).map(|e| e.to_string()),
            logs: v
                .get("logs")
                .and_then(|l| l.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
            units: v.get("unitsConsumed").and_then(|u| u.as_u64()),
        })
    }

    fn send(&self, tx: &[u8]) -> Result<String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(tx);
        self.call("sendTransaction", json!([b64, {"encoding": "base64", "preflightCommitment": "confirmed"}]))?
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error("rpc: send answer is not a signature".into()))
    }

    fn confirm(&self, signature: &str) -> Result<()> {
        for _ in 0..60 {
            let r = self.call("getSignatureStatuses", json!([[signature], {"searchTransactionHistory": false}]))?;
            if let Some(st) = r.get("value").and_then(|v| v.get(0)).filter(|s| !s.is_null()) {
                if let Some(e) = st.get("err").filter(|e| !e.is_null()) {
                    return Err(Error(format!("the transaction was included but FAILED: {e}")));
                }
                let level = st.get("confirmationStatus").and_then(|s| s.as_str()).unwrap_or("");
                if level == "confirmed" || level == "finalized" {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        Err(Error("not confirmed after 120 s; check the signature on an explorer before retrying".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_never_leak_their_query_in_errors() {
        assert_eq!(host_of("https://mainnet.helius-rpc.com/?api-key=SECRET"), "mainnet.helius-rpc.com");
        assert_eq!(host_of("http://127.0.0.1:8899"), "127.0.0.1:8899");
    }
}
