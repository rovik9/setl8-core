#!/usr/bin/env bash
# Runs the whole rehearsal against a LOCAL solana-test-validator, to validate the driver and the
# deploy commands without needing devnet SOL. This is NOT the devnet rehearsal: it is a real
# validator (real fees, real compute metering, real token / ATA / system programs, real durable
# nonces) but a private one. The devnet script is scripts/devnet-rehearsal.sh.
#
# Keys: the throwaway keys in ~/.setl8-devnet (generated for the rehearsal; no real key is ever used).
# Usage: tools/devnet-rehearsal/selftest-local.sh [path-to-vault-so] [path-to-sector-so]
set -euo pipefail

cd "$(dirname "$0")/../.."
D="${SETL8_DEVNET_DIR:-$HOME/.setl8-devnet}"
URL="http://127.0.0.1:8899"
VAULT_SO="${1:-$D/build-tree/out/core_vault.so}"
SECTOR_SO="${2:-tools/devnet-sector/target/deploy/mock_sector.so}"
LEDGER="$(mktemp -d)"
trap 'kill "$VAL" 2>/dev/null || true; rm -rf "$LEDGER"' EXIT

[ -f "$VAULT_SO" ] && [ -f "$SECTOR_SO" ] || { echo "missing .so files" >&2; exit 1; }
solana-test-validator --reset --quiet --ledger "$LEDGER" >"$LEDGER/validator.log" 2>&1 &
VAL=$!
for _ in $(seq 1 60); do
  curl -s -m 2 -X POST "$URL" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q ok && break
  sleep 1
done

sol() { solana "$@" --url "$URL"; }
for k in upgrade-authority sl8-test rov-test keeper freeze-authority trader1 trader2 trader3 bonder1 bonder2; do
  sol airdrop 20 "$(solana-keygen pubkey "$D/$k.json")" --keypair "$D/keeper.json" >/dev/null
done
VP=$(solana-keygen pubkey "$D/program-vault.json"); SP=$(solana-keygen pubkey "$D/program-sector.json")
for pair in "program-vault $VAULT_SO" "program-sector $SECTOR_SO"; do
  set -- $pair
  sol program deploy "$2" --program-id "$D/$1.json" --keypair "$D/upgrade-authority.json" \
    --upgrade-authority "$D/upgrade-authority.json" --max-len "$(wc -c <"$2" | tr -d ' ')"
done
GENESIS=$(curl -s -X POST "$URL" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getGenesisHash"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"])')
cargo run --release --manifest-path tools/devnet-rehearsal/Cargo.toml -- \
  --rpc-url "$URL" --cluster localnet --expect-genesis "$GENESIS" --keys-dir "$D" \
  --work-dir "$D/rehearsal-work-localnet" --vault-program "$VP" --sector-program "$SP"
