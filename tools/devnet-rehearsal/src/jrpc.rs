//! A tiny JSON-RPC client that, unlike the admin tool's, keeps the node's error payload
//! (a failed preflight simulation carries the program error and logs).

use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct RpcErr {
    pub message: String,
    pub data: Value,
}

impl std::fmt::Display for RpcErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct Jrpc {
    url: String,
    agent: ureq::Agent,
}

impl Jrpc {
    pub fn new(url: &str) -> Jrpc {
        Jrpc { url: url.to_string(), agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(40)).build() }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcErr> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        // Public endpoints rate limit; retry a few times with backoff on HTTP 429 / transport errors.
        let mut last = RpcErr { message: "no attempt".into(), data: Value::Null };
        for attempt in 0..6u64 {
            match self.agent.post(&self.url).send_json(body.clone()) {
                Ok(resp) => {
                    let v: Value = resp
                        .into_json()
                        .map_err(|_| RpcErr { message: format!("{method}: answer is not JSON"), data: Value::Null })?;
                    if let Some(e) = v.get("error") {
                        return Err(RpcErr {
                            message: e.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string(),
                            data: e.get("data").cloned().unwrap_or(Value::Null),
                        });
                    }
                    return v
                        .get("result")
                        .cloned()
                        .ok_or(RpcErr { message: format!("{method}: no result"), data: Value::Null });
                }
                Err(ureq::Error::Status(429, _)) => {
                    last = RpcErr { message: format!("{method}: HTTP 429 (rate limited)"), data: Value::Null }
                }
                Err(ureq::Error::Status(c, _)) => {
                    return Err(RpcErr { message: format!("{method}: HTTP {c}"), data: Value::Null })
                }
                Err(ureq::Error::Transport(t)) => {
                    last = RpcErr { message: format!("{method}: transport error {:?}", t.kind()), data: Value::Null }
                }
            }
            std::thread::sleep(Duration::from_millis(800 * (attempt + 1)));
        }
        Err(last)
    }
}
