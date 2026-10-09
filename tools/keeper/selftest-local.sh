#!/usr/bin/env bash
# End-to-end test of the keeper against a LOCAL solana-test-validator (a real runtime, a private cluster).
#
#   1. starts a validator, deploys the vault (a throwaway program id; PUBLIC test admin keys) and the mock sector
#   2. the rehearsal driver (tools/devnet-rehearsal) creates the vault, a product and a queue of claims
#      (3 traders + N extra traders, USDC and USDT), then stops
#   3. the keeper binary (built with --features localnet) runs: status, dry-run, then THREE `once` processes
#      started at the same moment (three independent keepers on one cycle), then status again
#   4. checks the cycle is closed and every claim paid, and prints transactions, compute units and timing
#
# Keys: throwaway keys only (~/.setl8-devnet, generated for the rehearsal) and a fresh keeper key in a temp
# dir. No real key is ever used. Usage: tools/keeper/selftest-local.sh [extra-claims (default 9)]
set -euo pipefail

cd "$(dirname "$0")/../.."
D="${SETL8_DEVNET_DIR:-$HOME/.setl8-devnet}"
EXTRA="${1:-9}"
URL="http://127.0.0.1:8899"
VAULT_SO="$D/build-tree/out/core_vault.so"
SECTOR_SO="tools/devnet-sector/target/deploy/mock_sector.so"
T="$(mktemp -d)"
trap 'kill "$VAL" 2>/dev/null || true; rm -rf "$T"' EXIT
[ -f "$VAULT_SO" ] && [ -f "$SECTOR_SO" ] || { echo "run: scripts/devnet-rehearsal.sh --cluster devnet build" >&2; exit 1; }

cargo build --release --manifest-path tools/devnet-rehearsal/Cargo.toml 2>&1 | tail -1
cargo build --release --features localnet --manifest-path tools/keeper/Cargo.toml 2>&1 | tail -1
KEEPER=tools/keeper/target/release/setl8-keeper

solana-test-validator --reset --quiet --ledger "$T/ledger" >"$T/validator.log" 2>&1 &
VAL=$!
for _ in $(seq 1 60); do
  curl -s -m 2 -X POST "$URL" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q ok && break
  sleep 1
done
rpc() { curl -s -X POST "$URL" -H 'content-type: application/json' -d "$1"; }
sol() { solana "$@" --url "$URL"; }

for k in upgrade-authority sl8-test rov-test keeper freeze-authority trader1 trader2 trader3 bonder1 bonder2; do
  sol airdrop 50 "$(solana-keygen pubkey "$D/$k.json")" --keypair "$D/keeper.json" >/dev/null
done
VP=$(solana-keygen pubkey "$D/program-vault.json"); SP=$(solana-keygen pubkey "$D/program-sector.json")
for pair in "program-vault $VAULT_SO" "program-sector $SECTOR_SO"; do
  set -- $pair
  sol program deploy "$2" --program-id "$D/$1.json" --keypair "$D/upgrade-authority.json" \
    --upgrade-authority "$D/upgrade-authority.json" --max-len "$(wc -c <"$2" | tr -d ' ')" >/dev/null
done
GENESIS=$(rpc '{"jsonrpc":"2.0","id":1,"method":"getGenesisHash"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"])')

echo "== the rehearsal driver builds the world and a queue of claims"
tools/devnet-rehearsal/target/release/devnet-rehearsal --rpc-url "$URL" --cluster localnet --expect-genesis "$GENESIS" \
  --keys-dir "$D" --work-dir "$T/work" --vault-program "$VP" --sector-program "$SP" \
  --until extra-claims --extra-claims "$EXTRA" | tail -3

# fresh, funded, hot keeper keys (mode 600): one per keeper, as independent operators would have
for k in a b c d; do
  solana-keygen new --no-bip39-passphrase --silent -o "$T/keeper-$k.json" >/dev/null
  chmod 600 "$T/keeper-$k.json"
  sol airdrop 5 "$(solana-keygen pubkey "$T/keeper-$k.json")" --keypair "$D/keeper.json" >/dev/null
done
COMMON=(--cluster localnet --rpc "$URL" --program-id "$VP")

echo "== status (before)"
$KEEPER status "${COMMON[@]}" --keypair "$T/keeper-d.json" | tee "$T/status-before.json" | python3 -c '
import sys, json
s = json.load(sys.stdin)
print("cycle", s["cycle"], "| open claims", s["open_claims"], "| next", s["next"], "| alerts", [a["kind"] for a in s["alerts"]])'

echo "== dry-run (sends nothing)"
$KEEPER dry-run "${COMMON[@]}" --fee-payer-pubkey "$(solana-keygen pubkey "$T/keeper-d.json")" >"$T/dry.log"
python3 - "$T/dry.log" <<'PY'
import sys, json
ev = [json.loads(l) for l in open(sys.argv[1]) if l.startswith("{")]
w = [e for e in ev if e["event"] == "would_send"]
print("would send:", [e["label"] for e in w])
assert w and w[0]["label"].startswith("reconcile_product") and any(e["label"] == "begin_heartbeat" for e in w)
PY

echo "== three keepers at the same moment (separate processes, one cycle)"
T0=$(date +%s)
$KEEPER once "${COMMON[@]}" --keypair "$T/keeper-a.json" --log-file "$T/keeper-a.log" >/dev/null & PA=$!
$KEEPER once "${COMMON[@]}" --keypair "$T/keeper-b.json" --log-file "$T/keeper-b.log" >/dev/null & PB=$!
$KEEPER once "${COMMON[@]}" --keypair "$T/keeper-c.json" --log-file "$T/keeper-c.log" >/dev/null & PC=$!
set +e; wait $PA; CA=$?; wait $PB; CB=$?; wait $PC; CC=$?; set -e
T1=$(date +%s)
echo "exit codes: keeper A $CA, B $CB, C $CC (0 = progress or nothing to do, 10 = an alert is present); wall time $((T1 - T0)) s"
[ "$CA" -lt 20 ] && [ "$CB" -lt 20 ] && [ "$CC" -lt 20 ] || { echo "FAIL: a keeper hit a hard failure" >&2; exit 1; }

echo "== status (after) and a third keeper that finds nothing to do"
$KEEPER status "${COMMON[@]}" --keypair "$T/keeper-d.json" >"$T/status-after.json"
$KEEPER once "${COMMON[@]}" --keypair "$T/keeper-d.json" --log-file "$T/keeper-d.log" >/dev/null || true

python3 - "$T" "$URL" <<'PY'
import sys, json, glob, urllib.request
t, url = sys.argv[1], sys.argv[2]
before = json.load(open(f"{t}/status-before.json")); after = json.load(open(f"{t}/status-after.json"))
def rpc(method, params):
    req = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(), {"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req))["result"]
n = before["open_claims"]["count"]
assert before["cycle"]["active"] is False and n >= 7, before
assert after["cycle"]["active"] is False and after["cycle"]["id"] == 1, after
assert after["cycle"]["processed"] == after["cycle"]["eligible"] == n, after
assert after["open_claims"]["count"] == 0 and after["open_claims"]["total"] == 0, after["open_claims"]
print(f"OK: the cycle closed, {n} claims paid in full, nothing owed")
sent = {}
for name in ("a", "b", "c", "d"):
    for line in open(f"{t}/keeper-{name}.log"):
        if not line.startswith("{"): continue
        e = json.loads(line)
        if e["event"] == "pass_done": print(f"keeper {name}: sent {len(e['sent'])} transactions, idle={e['idle']!r}, hard_failure={e['hard_failure']!r}")
        if e["event"] in ("lost_race", "begin_not_needed", "batch_already_done"): print(f"keeper {name}: {e['event']} {e.get('label', '')} {e.get('error', '')} {e.get('now_state', '')}")
        if e["event"] == "sent": sent[e["signature"]] = (name, e["label"], e.get("units"))
        if e["event"] == "lost_race": print(f"keeper {name} lost a race benignly: {e['label']} -> {e['error']}")
        if e["level"] == "error": print(f"ERROR EVENT from keeper {name}: {e}"); raise SystemExit(1)
print(f"{len(sent)} transactions landed from the keepers:")
rows = []
for sig, (who, label, units) in sorted(sent.items(), key=lambda x: x[1][1]):
    tx = rpc("getTransaction", [sig, {"encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}])
    cu = tx["meta"]["computeUnitsConsumed"] if tx else None
    rows.append((label, who, cu, units))
    print(f"  {label:<46} by keeper {who}: {cu} CU on chain (simulated {units}), fee {tx['meta']['fee'] if tx else '?'} lamports")
json.dump([{"label": r[0], "keeper": r[1], "cu": r[2], "simulated_cu": r[3]} for r in rows], open(f"{t}/e2e.json", "w"))
print("e2e result: PASS")
PY
