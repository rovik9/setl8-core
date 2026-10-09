//! The keeper's view of a cluster, behind a trait so the decision code and the tests never need a network.
//! `HttpChain` is the real implementation, over the admin tool's JSON-RPC client.

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AccountDeserialize, Discriminator};
use base64::Engine;
use core_vault::state::{PayoutClaim, ProductRegistry};
use serde_json::{json, Value};
use setl8_admin::rpc::HttpRpc;
use solana_hash::Hash;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawAccount {
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
}

/// A transaction error as far as the keeper cares: the custom program code, and the node's own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxErr {
    pub custom: Option<u32>,
    pub text: String,
}

impl TxErr {
    pub fn from_json(v: &Value) -> TxErr {
        let custom = v["InstructionError"][1]["Custom"].as_u64().map(|c| c as u32);
        TxErr { custom, text: v.to_string() }
    }

    /// Extracts `custom program error: 0x178e` from a node's error message.
    pub fn from_message(msg: &str) -> TxErr {
        let custom = msg
            .split("custom program error: 0x")
            .nth(1)
            .and_then(|r| r.split(|c: char| !c.is_ascii_hexdigit()).next())
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        TxErr { custom, text: msg.chars().take(300).collect() }
    }
}

#[derive(Clone, Debug)]
pub enum ChainError {
    /// HTTP 429 or similar: back off and try again.
    RateLimited(String),
    /// The node could not be reached or answered garbage: try again.
    Network(String),
    /// The node refused the transaction, or it landed and failed.
    Rejected(TxErr),
    /// The provider does not support this call (for example `getProgramAccounts`): configure the fallback.
    Unsupported(String),
    Other(String),
}

impl ChainError {
    pub fn transient(&self) -> bool {
        matches!(self, ChainError::RateLimited(_) | ChainError::Network(_))
    }
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::RateLimited(m) | ChainError::Network(m) | ChainError::Unsupported(m) | ChainError::Other(m) => {
                write!(f, "{m}")
            }
            ChainError::Rejected(e) => write!(f, "rejected: {}", e.text),
        }
    }
}

pub type CResult<T> = Result<T, ChainError>;

#[derive(Clone, Debug, Default)]
pub struct Sim {
    pub err: Option<TxErr>,
    pub logs: Vec<String>,
    pub units: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Confirmed,
    Failed(TxErr),
}

pub trait Chain {
    fn genesis_hash(&self) -> CResult<String>;
    /// The chain's own unix time (the clock the program reads), not the local clock.
    fn now(&self) -> CResult<i64>;
    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>>;
    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>>;
    /// Addresses of every `PayoutClaim` (discriminator + size filter), without their data.
    fn claim_addresses(&self) -> CResult<Vec<Pubkey>>;
    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>>;
    fn balance(&self, key: &Pubkey) -> CResult<u64>;
    /// A recent blockhash and the last block height at which it is valid.
    fn blockhash(&self) -> CResult<(Hash, u64)>;
    fn block_height(&self) -> CResult<u64>;
    fn simulate(&self, tx: &[u8]) -> CResult<Sim>;
    fn send(&self, tx: &[u8]) -> CResult<String>;
    fn status(&self, signature: &str) -> CResult<Option<TxStatus>>;
}

// ---------------------------------------------------------------------------------- the real thing

pub struct HttpChain {
    rpc: HttpRpc,
    program: Pubkey,
}

impl HttpChain {
    pub fn new(url: &str, program: Pubkey) -> HttpChain {
        HttpChain { rpc: HttpRpc::new(url), program }
    }

    fn call(&self, method: &str, params: Value) -> CResult<Value> {
        self.rpc.call(method, params).map_err(|e| {
            let m = e.0;
            if m.contains("429") || m.to_lowercase().contains("rate limit") || m.contains("Too Many Requests") {
                ChainError::RateLimited(m)
            } else if m.contains("cannot reach the node")
                || m.contains("is not JSON")
                || m.contains("HTTP 50")
                || m.contains("HTTP 408")
            {
                ChainError::Network(m)
            } else if m.contains("custom program error")
                || m.contains("Blockhash not found")
                || m.contains("simulation failed")
            {
                ChainError::Rejected(TxErr::from_message(&m))
            } else if m.contains("HTTP 403")
                || m.contains("HTTP 410")
                || m.contains("excluded from account secondary indexes")
                || m.contains("method not found")
                || m.contains("Method not found")
            {
                ChainError::Unsupported(m)
            } else {
                ChainError::Other(m)
            }
        })
    }

    fn decode_account(v: &Value) -> Option<RawAccount> {
        let data = base64::engine::general_purpose::STANDARD.decode(v["data"][0].as_str()?).ok()?;
        Some(RawAccount { lamports: v["lamports"].as_u64()?, owner: v["owner"].as_str()?.parse().ok()?, data })
    }

    fn gpa(&self, disc: [u8; 8], size: Option<usize>, slice_none: bool) -> CResult<Vec<(Pubkey, RawAccount)>> {
        let mut filters = vec![json!({"memcmp": {"offset": 0, "bytes": bs58::encode(disc).into_string()}})];
        if let Some(s) = size {
            filters.push(json!({"dataSize": s}));
        }
        let mut cfg = json!({"encoding": "base64", "commitment": "confirmed", "filters": filters});
        if slice_none {
            cfg["dataSlice"] = json!({"offset": 0, "length": 0});
        }
        let r = self.call("getProgramAccounts", json!([self.program.to_string(), cfg]))?;
        let arr = r.as_array().ok_or_else(|| ChainError::Other("getProgramAccounts: answer is not a list".into()))?;
        let mut out = vec![];
        for item in arr {
            let key: Pubkey = item["pubkey"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| ChainError::Other("getProgramAccounts: bad pubkey".into()))?;
            let acc = Self::decode_account(&item["account"]).unwrap_or(RawAccount {
                lamports: 0,
                owner: self.program,
                data: vec![],
            });
            out.push((key, acc));
        }
        Ok(out)
    }
}

impl Chain for HttpChain {
    fn genesis_hash(&self) -> CResult<String> {
        self.call("getGenesisHash", json!([]))?
            .as_str()
            .map(String::from)
            .ok_or_else(|| ChainError::Other("genesis hash is not a string".into()))
    }

    fn now(&self) -> CResult<i64> {
        let clock = anchor_lang::prelude::pubkey!("SysvarC1ock11111111111111111111111111111111");
        let acc = self.account(&clock)?.ok_or_else(|| ChainError::Other("no clock sysvar".into()))?;
        // slot u64, epoch_start_timestamp i64, epoch u64, leader_schedule_epoch u64, unix_timestamp i64
        acc.data
            .get(32..40)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| ChainError::Other("clock sysvar too short".into()))
    }

    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>> {
        let r =
            self.call("getAccountInfo", json!([key.to_string(), {"encoding": "base64", "commitment": "confirmed"}]))?;
        let v = &r["value"];
        if v.is_null() {
            return Ok(None);
        }
        Self::decode_account(v).map(Some).ok_or_else(|| ChainError::Other("malformed account in answer".into()))
    }

    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>> {
        let mut out = vec![];
        for chunk in keys.chunks(100) {
            let ks: Vec<String> = chunk.iter().map(|k| k.to_string()).collect();
            let r = self.call("getMultipleAccounts", json!([ks, {"encoding": "base64", "commitment": "confirmed"}]))?;
            let arr = r["value"].as_array().ok_or_else(|| ChainError::Other("getMultipleAccounts: no list".into()))?;
            for v in arr {
                out.push(if v.is_null() { None } else { Self::decode_account(v) });
            }
        }
        Ok(out)
    }

    fn claim_addresses(&self) -> CResult<Vec<Pubkey>> {
        let disc: [u8; 8] = PayoutClaim::DISCRIMINATOR.try_into().expect("8-byte discriminator");
        Ok(self.gpa(disc, Some(PayoutClaim::SPACE), true)?.into_iter().map(|(k, _)| k).collect())
    }

    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>> {
        let disc: [u8; 8] = ProductRegistry::DISCRIMINATOR.try_into().expect("8-byte discriminator");
        self.gpa(disc, None, false)
    }

    fn balance(&self, key: &Pubkey) -> CResult<u64> {
        let r = self.call("getBalance", json!([key.to_string(), {"commitment": "confirmed"}]))?;
        r["value"].as_u64().ok_or_else(|| ChainError::Other("getBalance: no value".into()))
    }

    fn blockhash(&self) -> CResult<(Hash, u64)> {
        let r = self.call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))?;
        let h: Hash = r["value"]["blockhash"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ChainError::Other("no blockhash".into()))?;
        Ok((h, r["value"]["lastValidBlockHeight"].as_u64().unwrap_or(u64::MAX)))
    }

    fn block_height(&self) -> CResult<u64> {
        self.call("getBlockHeight", json!([{"commitment": "confirmed"}]))?
            .as_u64()
            .ok_or_else(|| ChainError::Other("no block height".into()))
    }

    fn simulate(&self, tx: &[u8]) -> CResult<Sim> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(tx);
        let r = self.call(
            "simulateTransaction",
            json!([b64, {"encoding": "base64", "sigVerify": true, "commitment": "confirmed"}]),
        )?;
        let v = &r["value"];
        Ok(Sim {
            err: if v["err"].is_null() { None } else { Some(TxErr::from_json(&v["err"])) },
            logs: v["logs"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            units: v["unitsConsumed"].as_u64(),
        })
    }

    fn send(&self, tx: &[u8]) -> CResult<String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(tx);
        self.call(
            "sendTransaction",
            json!([b64, {"encoding": "base64", "preflightCommitment": "confirmed", "maxRetries": 3}]),
        )?
        .as_str()
        .map(String::from)
        .ok_or_else(|| ChainError::Other("send: answer is not a signature".into()))
    }

    fn status(&self, signature: &str) -> CResult<Option<TxStatus>> {
        let r = self.call("getSignatureStatuses", json!([[signature], {"searchTransactionHistory": false}]))?;
        let st = &r["value"][0];
        if st.is_null() {
            return Ok(None);
        }
        if !st["err"].is_null() {
            return Ok(Some(TxStatus::Failed(TxErr::from_json(&st["err"]))));
        }
        Ok(match st["confirmationStatus"].as_str() {
            Some("confirmed") | Some("finalized") => Some(TxStatus::Confirmed),
            _ => None,
        })
    }
}

/// Decodes a claim account (any problem is `None`).
pub fn decode_claim(address: Pubkey, acc: &RawAccount, program: &Pubkey) -> Option<crate::model::ClaimView> {
    if acc.owner != *program {
        return None;
    }
    let c = PayoutClaim::try_deserialize(&mut acc.data.as_slice()).ok()?;
    Some(crate::model::ClaimView {
        address,
        trader_wallet: c.trader_wallet,
        owed: c.owed,
        created_in_cycle: c.created_in_cycle,
        last_settled_cycle: c.last_settled_cycle,
        kind: c.kind,
    })
}

impl<T: Chain + ?Sized> Chain for Box<T> {
    fn genesis_hash(&self) -> CResult<String> {
        (**self).genesis_hash()
    }
    fn now(&self) -> CResult<i64> {
        (**self).now()
    }
    fn account(&self, key: &Pubkey) -> CResult<Option<RawAccount>> {
        (**self).account(key)
    }
    fn accounts(&self, keys: &[Pubkey]) -> CResult<Vec<Option<RawAccount>>> {
        (**self).accounts(keys)
    }
    fn claim_addresses(&self) -> CResult<Vec<Pubkey>> {
        (**self).claim_addresses()
    }
    fn registries(&self) -> CResult<Vec<(Pubkey, RawAccount)>> {
        (**self).registries()
    }
    fn balance(&self, key: &Pubkey) -> CResult<u64> {
        (**self).balance(key)
    }
    fn blockhash(&self) -> CResult<(Hash, u64)> {
        (**self).blockhash()
    }
    fn block_height(&self) -> CResult<u64> {
        (**self).block_height()
    }
    fn simulate(&self, tx: &[u8]) -> CResult<Sim> {
        (**self).simulate(tx)
    }
    fn send(&self, tx: &[u8]) -> CResult<String> {
        (**self).send(tx)
    }
    fn status(&self, signature: &str) -> CResult<Option<TxStatus>> {
        (**self).status(signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_codes_are_read_from_json_and_from_messages() {
        let v = json!({"InstructionError": [0, {"Custom": 6030}]});
        assert_eq!(TxErr::from_json(&v).custom, Some(6030));
        assert_eq!(TxErr::from_json(&json!("BlockhashNotFound")).custom, None);
        let m = "rpc x: sendTransaction failed: Transaction simulation failed: Error processing Instruction 0: custom program error: 0x178e";
        assert_eq!(TxErr::from_message(m).custom, Some(0x178e));
        assert_eq!(TxErr::from_message("something else").custom, None);
    }

    #[test]
    fn transient_errors() {
        assert!(ChainError::RateLimited("x".into()).transient() && ChainError::Network("x".into()).transient());
        assert!(!ChainError::Other("x".into()).transient() && !ChainError::Unsupported("x".into()).transient());
    }
}
