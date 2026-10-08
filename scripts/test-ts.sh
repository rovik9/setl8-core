#!/usr/bin/env bash
# Runs the TypeScript suite against the LOCALNET (public test keys) build.
#
# Why not plain `anchor test`? It builds the default (real-key) program and loads
# target/deploy/core_vault.so, whose admin keys the suite cannot sign for; and
# `anchor test --skip-deploy` drops the validator's genesis programs. So this
# script starts its own validator with target/test-deploy/core_vault.so and runs
# `anchor test --skip-local-validator --skip-build --skip-deploy` against it.
# target/deploy/ is never touched.
#
# Usage: scripts/test-ts.sh        (run scripts/build-test-so.sh first, or use test-all.sh)
set -uo pipefail

cd "$(dirname "$0")/.."

program_id="$(sed -n 's/^core_vault = "\(.*\)"/\1/p' Anchor.toml | head -1)"
so="target/test-deploy/core_vault.so"
[ -f "$so" ] || { echo "ERROR: $so missing -- run scripts/build-test-so.sh first" >&2; exit 1; }
command -v solana-test-validator >/dev/null || { echo "ERROR: solana-test-validator not found" >&2; exit 1; }

if lsof -nP -iTCP:8899 -sTCP:LISTEN >/dev/null 2>&1; then
  echo "ERROR: something is already listening on :8899 (a stale validator?). Stop it first." >&2
  exit 1
fi

ledger="$(mktemp -d)"
vlog="$ledger/validator.log"
validator_pid=""
cleanup() {
  [ -n "$validator_pid" ] && kill "$validator_pid" 2>/dev/null && wait "$validator_pid" 2>/dev/null
  rm -rf "$ledger"
}
trap cleanup EXIT

solana-test-validator --reset --quiet --ledger "$ledger/ledger" \
  --bpf-program "$program_id" "$so" >"$vlog" 2>&1 &
validator_pid=$!

for _ in $(seq 1 60); do
  if curl -s -m 1 -X POST -H 'Content-Type: application/json' \
       -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' http://127.0.0.1:8899 | grep -q '"ok"'; then
    ready=1; break
  fi
  if ! kill -0 "$validator_pid" 2>/dev/null; then
    echo "ERROR: validator exited early:" >&2; tail -20 "$vlog" >&2; exit 1
  fi
  sleep 1
done
[ "${ready:-0}" = 1 ] || { echo "ERROR: validator did not become healthy" >&2; tail -20 "$vlog" >&2; exit 1; }

anchor test --skip-local-validator --skip-build --skip-deploy
