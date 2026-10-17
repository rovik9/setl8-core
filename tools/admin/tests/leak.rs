//! A custom RPC URL carries an API key (in the path, the query or the userinfo). It must never reach
//! the transaction file, stdout, stderr, a prompt or an error message, and a transaction file written
//! by an older version (which has the full URL in its informational `cluster` field) must still be
//! inspected, signed and sent, with the key scrubbed from every output and from the rewritten file.
#![cfg(feature = "localnet")]
mod common;

use common::*;
use setl8_admin::cluster::Cluster;

const KEY: &str = "SECRETKEY123";

/// (url, the label tx.json must carry)
fn urls() -> Vec<(String, &'static str)> {
    vec![
        (format!("https://rpc.example.com/v2/{KEY}"), "custom (rpc.example.com)"),
        (format!("https://rpc.example.com/?api-key={KEY}"), "custom (rpc.example.com)"),
        (format!("https://user:{KEY}@rpc.example.com/"), "custom (rpc.example.com)"),
        (format!("https://{KEY}@rpc.example.com:8899/x?k={KEY}#{KEY}"), "custom (rpc.example.com:8899)"),
        // a password with a raw '/' and a key after '@' in the path: the parser cannot tell, so it hides the host
        (format!("https://user:{KEY}/x@rpc.example.com/"), "custom (<host hidden>)"),
    ]
}

fn no_key(what: &str, text: &str) {
    assert!(!text.contains(KEY), "{what} leaks the key:\n{text}");
}

fn assert_clean(h: &TestHost) {
    no_key("stdout", &h.out);
    no_key("stderr", &h.err);
    no_key("prompts", &h.prompts.join("\n"));
}

fn plan_args<'a>(w: &'a World, out: &'a str, url: &'a str, usdc: &'a str, usdt: &'a str) -> Vec<&'a str> {
    let _ = w;
    vec![
        "plan",
        "init-vault",
        "--cluster",
        url,
        "--out",
        out,
        "--usdc-mint",
        usdc,
        "--usdt-mint",
        usdt,
        "--recent-blockhash",
    ]
}

#[test]
fn cluster_name_shows_the_host_only() {
    for (url, label) in urls() {
        let c = Cluster::parse(&url).unwrap();
        assert_eq!(c.name(), label, "{url}");
        no_key("name()", &c.name());
        no_key("Debug", &format!("{c:?}"));
    }
    assert_eq!(Cluster::parse("http://127.0.0.1:8899").unwrap().name(), "custom (127.0.0.1:8899)");
}

#[test]
fn a_url_with_a_key_never_reaches_tx_json_or_any_output_through_the_whole_ceremony() {
    for (url, label) in urls() {
        let w = World::bare();
        let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
        let out = w.s(&w.path("tx.json"));
        let mut all = String::new();

        let (c, h) = w.run(&[], &plan_args(&w, &out, &url, &usdc, &usdt));
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        assert_eq!(w.tx_file("tx.json").cluster, label);
        assert!(h.out.contains(&format!("Cluster ........ {label}")), "{}", h.out);
        let raw = std::fs::read_to_string(w.path("tx.json")).unwrap();
        no_key("tx.json", &raw);
        all.push_str(&h.all_output());

        // inspect online (--rpc URL), then both signatures, then send with the URL as the cluster
        let p = w.s(&w.path("tx.json"));
        let (c, h) = w.run(&[], &["inspect", &p, "--rpc", &url]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        let (c, h) = w.sign("tx.json", &w.rov_key, &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        let (c, h) = w.run(&[], &["send", &p, "--cluster", &url]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        no_key("tx.json after signing", &std::fs::read_to_string(w.path("tx.json")).unwrap());
        assert!(w.exists(&w.keys.vault()));
        let _ = all;
    }
}

#[test]
fn status_and_nonce_with_a_keyed_url_print_no_key() {
    for (url, _) in urls() {
        let w = World::with_vault();
        let (c, h) = w.run(&[], &["status", "--cluster", &url]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        let k = w.s(&w.sl8_key);
        let (c, h) = w.run_auto(&[], &["nonce-create", "--cluster", &url, "--keypair", &k]);
        assert_clean(&h);
        let _ = c;
    }
}

#[test]
fn errors_about_a_keyed_url_do_not_repeat_it() {
    for (url, _) in urls() {
        let w = World::bare();
        let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
        let out = w.s(&w.path("e.json"));
        // offline with no genesis hash for a custom cluster: the error names the cluster
        let mut a = plan_args(&w, &out, &url, &usdc, &usdt);
        a.push("--offline");
        let (c, h) = w.run(&[], &a);
        assert_eq!(c, 1);
        assert!(h.err.contains("no well-known genesis hash") && h.err.contains("custom ("), "{}", h.err);
        assert_clean(&h);
        // the node is on a different cluster than the one the user named
        w.set_genesis("SomeOtherGenesis111111111111111111111111111");
        let mut a = plan_args(&w, &out, &url, &usdc, &usdt);
        a.extend(["--genesis-hash", LOCAL_GENESIS]);
        let (c, h) = w.run(&[], &a);
        assert_eq!(c, 2, "a refusal");
        assert!(h.err.contains("refused") && h.err.contains("custom ("), "{}", h.err);
        assert_clean(&h);
        // a machine with no network
        w.set_genesis(LOCAL_GENESIS);
        let mut h = w.host();
        h.no_network = true;
        let argv: Vec<String> = plan_args(&w, &out, &url, &usdc, &usdt).iter().map(|s| s.to_string()).collect();
        let c = setl8_admin::cli::run_guarded(&mut h, &argv);
        assert_eq!(c, 1);
        assert_clean(&h);
        // a URL typed where a cluster name, a key or a command is expected, or as a stray argument
        for bad in [
            vec!["status", "--cluster", url.trim_start_matches("https://")],
            vec!["status", "--cluster", &url, "--program-id", &url],
            vec!["status", "--cluster", "devnet", &url],
            vec!["inspect", &url],
            vec![&url],
            vec!["plan", &url, "--cluster", "localnet", "--out", "x.json"],
            vec!["plan", "init-vault", &url, "--cluster", "localnet"],
            vec!["nonce-create", &url, "--cluster", "localnet"],
        ] {
            let (c, h) = w.run(&[], &bad);
            assert_ne!(c, 0, "{bad:?}");
            assert_clean(&h);
        }
    }
}

#[test]
fn a_url_typed_where_a_file_goes_is_not_echoed() {
    for (url, _) in urls() {
        let w = World::bare();
        let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
        // --out
        let mut a = plan_args(&w, &url, "localnet", &usdc, &usdt);
        a[5] = &url;
        let (c, h) = w.run(&[], &a);
        assert_ne!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        // --keypair
        let out = w.s(&w.path("tx.json"));
        let (c, _) = w.run(&[], &plan_args(&w, &out, "localnet", &usdc, &usdt));
        assert_eq!(c, 0);
        let p = w.s(&w.path("tx.json"));
        for cmd in ["sign", "nonce-create"] {
            let mut v = vec![cmd, p.as_str(), "--keypair", url.as_str(), "--cluster", "localnet"];
            if cmd == "nonce-create" {
                v.remove(1);
            }
            let (c, h) = w.run(&[], &v);
            assert_ne!(c, 0, "{}{}", h.out, h.err);
            assert_clean(&h);
        }
    }
}

#[test]
fn a_malformed_keyed_url_is_refused_without_echo() {
    let w = World::bare();
    let (c, h) = w.run(&[], &["status", "--cluster", &format!("rpc.example.com/{KEY}")]);
    assert_eq!(c, 1);
    assert!(h.err.contains("unknown cluster"), "{}", h.err);
    assert_clean(&h);
}

/// A file written before this fix: `cluster` is `custom (<full URL>)`.
fn make_old_format(w: &World, url: &str) {
    let path = w.path("tx.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["cluster"] = serde_json::Value::String(format!("custom ({url})"));
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap() + "\n").unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains(KEY), "the fixture must carry the old format");
}

#[test]
fn an_old_format_tx_json_still_inspects_signs_and_sends_and_loses_the_key_on_the_way() {
    for (url, label) in urls() {
        let w = World::bare();
        let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
        let out = w.s(&w.path("tx.json"));
        let (c, h) = w.run(&[], &plan_args(&w, &out, "localnet", &usdc, &usdt));
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        let hash_before = w.hash_of("tx.json");
        let msg_before = w.tx_file("tx.json").message_b64;
        make_old_format(&w, &url);

        let p = w.s(&w.path("tx.json"));
        let (c, h) = w.run(&[], &["inspect", &p, "--rpc", "localnet"]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        assert!(h.out.contains("Message SHA-256") || h.out.contains(&hash_before[..8]), "{}", h.out);

        let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        // the rewritten file is clean, and the signed bytes are the ones planned
        let raw = std::fs::read_to_string(w.path("tx.json")).unwrap();
        no_key("tx.json after sign", &raw);
        let f = w.tx_file("tx.json");
        assert_eq!(f.cluster, label);
        assert_eq!(f.message_sha256, hash_before);
        assert_eq!(f.message_b64, msg_before);
        assert_eq!(f.signatures.len(), 1);
        let (c, h) = w.sign("tx.json", &w.rov_key, &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        let (c, h) = w.send("tx.json", &[]);
        assert_eq!(c, 0, "{}{}", h.out, h.err);
        assert_clean(&h);
        assert!(w.exists(&w.keys.vault()));
    }
}

#[test]
fn a_garbage_cluster_label_in_the_file_is_inspectable_and_never_echoed() {
    let w = World::bare();
    let (usdc, usdt) = (w.usdc.to_string(), w.usdt.to_string());
    let out = w.s(&w.path("tx.json"));
    let (c, _) = w.run(&[], &plan_args(&w, &out, "localnet", &usdc, &usdt));
    assert_eq!(c, 0);
    let before = w.hash_of("tx.json");
    let path = w.path("tx.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["cluster"] = serde_json::Value::String(format!("https://rpc.example.com/{KEY}?api-key={KEY}"));
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    // the label is informational: the message is still rebuilt from the allowlist and hashed the same
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert_clean(&h);
    assert_eq!(w.tx_file("tx.json").cluster, "<unrecognised cluster label>");
    assert_eq!(w.hash_of("tx.json"), before);
}
