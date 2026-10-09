//! The JSON-RPC client against a tiny local HTTP server: request shapes, answer parsing and
//! error messages (which must never echo the URL, because URLs carry API keys).
mod mock {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Serves `answers` in order, one per connection; records each request body.
    pub fn serve(answers: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/?api-key=SECRETKEY123", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for ans in answers {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut buf = vec![];
                let mut tmp = [0u8; 4096];
                let body = loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break String::new();
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = s.read(&mut tmp).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break String::from_utf8_lossy(&buf[i + 4..]).to_string();
                    }
                };
                seen2.lock().unwrap().push(body);
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", ans.len(), ans);
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (url, seen)
    }
}

use anchor_lang::prelude::Pubkey;
use base64::Engine;
use setl8_admin::rpc::{HttpRpc, Rpc};

fn result(v: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc":"2.0","id":1,"result":v}).to_string()
}

#[test]
fn each_call_sends_the_right_request_and_parses_the_answer() {
    let acc_key = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let data_b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
    let acct = serde_json::json!({"lamports": 77, "owner": owner.to_string(), "data": [data_b64, "base64"], "executable": false});
    let (url, seen) = mock::serve(vec![
        result(serde_json::json!("GenesisHashXYZ")),
        result(serde_json::json!({"context": {"slot": 1}, "value": acct.clone()})),
        result(serde_json::json!({"context": {"slot": 1}, "value": null})),
        result(serde_json::json!([{"pubkey": acc_key.to_string(), "account": acct}])),
        result(serde_json::json!({"context": {"slot": 1}, "value": {"blockhash": "BH123", "lastValidBlockHeight": 5}})),
        result(serde_json::json!(1_447_680u64)),
        result(
            serde_json::json!({"context": {"slot": 1}, "value": {"err": null, "logs": ["Program log: hi"], "unitsConsumed": 4242}}),
        ),
        result(
            serde_json::json!({"context": {"slot": 1}, "value": {"err": {"InstructionError": [0, {"Custom": 6015}]}, "logs": [], "unitsConsumed": 1}}),
        ),
        result(serde_json::json!("SIGNATURE1")),
        result(
            serde_json::json!({"context": {"slot": 1}, "value": [{"confirmationStatus": "confirmed", "err": null}]}),
        ),
        result(
            serde_json::json!({"context": {"slot": 1}, "value": [{"confirmationStatus": "confirmed", "err": {"InstructionError": [0, "Custom"]}}]}),
        ),
    ]);
    let rpc = HttpRpc::new(&url);
    assert_eq!(rpc.genesis_hash().unwrap(), "GenesisHashXYZ");
    let a = rpc.account(&acc_key).unwrap().unwrap();
    assert_eq!((a.lamports, a.owner, a.data.clone(), a.executable), (77, owner, vec![1, 2, 3], false));
    assert!(rpc.account(&acc_key).unwrap().is_none());
    let list = rpc.program_accounts(&owner, &[9, 8, 7, 6, 5, 4, 3, 2]).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, acc_key);
    assert_eq!(rpc.latest_blockhash().unwrap(), "BH123");
    assert_eq!(rpc.min_balance_for_rent(80).unwrap(), 1_447_680);
    let ok = rpc.simulate(&[1, 2, 3]).unwrap();
    assert_eq!((ok.err, ok.units, ok.logs), (None, Some(4242), vec!["Program log: hi".to_string()]));
    let bad = rpc.simulate(&[1]).unwrap();
    assert!(bad.err.unwrap().contains("6015"));
    assert_eq!(rpc.send(&[7, 7]).unwrap(), "SIGNATURE1");
    rpc.confirm("SIGNATURE1").unwrap();
    let e = rpc.confirm("SIGNATURE1").unwrap_err();
    assert!(e.0.contains("FAILED"), "{e}");

    let reqs = seen.lock().unwrap().clone();
    let j = |i: usize| serde_json::from_str::<serde_json::Value>(&reqs[i]).unwrap();
    assert_eq!(j(0)["method"], "getGenesisHash");
    assert_eq!(j(1)["method"], "getAccountInfo");
    assert_eq!(j(1)["params"][0], acc_key.to_string());
    assert_eq!(j(1)["params"][1]["encoding"], "base64");
    assert_eq!(j(3)["method"], "getProgramAccounts");
    assert_eq!(j(3)["params"][1]["filters"][0]["memcmp"]["offset"], 0);
    assert_eq!(
        j(3)["params"][1]["filters"][0]["memcmp"]["bytes"],
        bs58::encode([9u8, 8, 7, 6, 5, 4, 3, 2]).into_string()
    );
    assert_eq!(j(5)["params"][0], 80);
    assert_eq!(j(6)["method"], "simulateTransaction");
    assert_eq!(j(6)["params"][0], base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]));
    assert_eq!(j(6)["params"][1]["sigVerify"], true);
    assert_eq!(j(8)["method"], "sendTransaction");
    assert_eq!(j(8)["params"][0], base64::engine::general_purpose::STANDARD.encode([7u8, 7]));
    assert_eq!(j(9)["method"], "getSignatureStatuses");
}

#[test]
fn rpc_errors_name_the_host_and_never_the_url_query() {
    let (url, _) = mock::serve(vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"Transaction simulation failed"}})
            .to_string(),
        "this is not json".to_string(),
        result(serde_json::json!(12345)),
    ]);
    let rpc = HttpRpc::new(&url);
    let e = rpc.genesis_hash().unwrap_err();
    assert!(e.0.contains("Transaction simulation failed") && e.0.contains("127.0.0.1"), "{e}");
    assert!(!e.0.contains("SECRETKEY123"), "{e}");
    let e = rpc.genesis_hash().unwrap_err();
    assert!(e.0.contains("not JSON") && !e.0.contains("SECRETKEY123"), "{e}");
    let e = rpc.genesis_hash().unwrap_err();
    assert!(e.0.contains("not a string"), "{e}");
    // nothing listens
    let dead = HttpRpc::new("http://127.0.0.1:1/?api-key=SECRETKEY123");
    let e = dead.genesis_hash().unwrap_err();
    assert!(e.0.contains("cannot reach the node") && !e.0.contains("SECRETKEY123"), "{e}");
}
