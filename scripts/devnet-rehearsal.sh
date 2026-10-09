#!/usr/bin/env bash
# DEVNET REHEARSAL of the whole core-vault protocol (module 6). Devnet ONLY.
#
#   scripts/devnet-rehearsal.sh --cluster devnet <command>
#
# commands:
#   build   build the devnet copy of the vault (scratch tree, throwaway program id, localnet = PUBLIC test
#           admin keys, stack-frame checked) and the mock sector program
#   fund    airdrop devnet SOL to the throwaway keys (rate limited: retries with backoff)
#   needs   print which keys still need how much SOL, and exit 3 if any
#   deploy  deploy both programs with `solana program deploy` (skips a program that is already deployed)
#   run     run the rehearsal driver (tools/devnet-rehearsal) against devnet
#   all     build, fund, needs, deploy, run
#
# Safety: refuses any cluster but `devnet`, any RPC URL that does not contain "devnet" or does contain
# "mainnet", and any node whose genesis hash is not devnet's. Every solana command passes an explicit --url
# and --keypair; your own solana config and wallet are never used. All keys are THROWAWAY keys generated
# into ~/.setl8-devnet (chmod 700, never in this repo) plus the PUBLIC localnet test admin keys. Anyone can
# sign admin actions on this deployment (public keys): acceptable ONLY on devnet.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$(pwd)"

[ "${1:-}" = "--cluster" ] && [ "${2:-}" = "devnet" ] || { echo "usage: $0 --cluster devnet <build|fund|needs|deploy|run|all>   (devnet is the only cluster this script accepts)" >&2; exit 2; }
CMD="${3:-}"
URL="${DEVNET_RPC_URL:-https://api.devnet.solana.com}"
case "$URL" in
  *mainnet*) echo "refusing: the RPC URL contains 'mainnet'" >&2; exit 2 ;;
  *devnet*) ;;
  *) echo "refusing: the RPC URL must contain 'devnet'" >&2; exit 2 ;;
esac
DEVNET_GENESIS="EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
D="${SETL8_DEVNET_DIR:-$HOME/.setl8-devnet}"
B="$D/build-tree"
VAULT_SO="$B/out/core_vault.so"
SECTOR_SO="$REPO/tools/devnet-sector/target/deploy/mock_sector.so"

genesis_ok() {
  local g
  g=$(curl -s -m 30 -X POST "$URL" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getGenesisHash"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"])')
  [ "$g" = "$DEVNET_GENESIS" ] || { echo "refusing: the node's genesis hash is $g, not devnet's" >&2; exit 2; }
}

need_keys() {
  [ -d "$D" ] || { echo "no $D: generate the throwaway keys first (see docs/DEVNET-REHEARSAL.md, section 2)" >&2; exit 1; }
  [ "$(stat -f '%Lp' "$D" 2>/dev/null || stat -c '%a' "$D")" = "700" ] || { echo "$D must be mode 700" >&2; exit 1; }
}

pub() { solana-keygen pubkey "$D/$1.json"; }

cmd_build() {
  need_keys
  local pid; pid=$(pub program-vault)
  rm -rf "$B"; mkdir -p "$B"
  rsync -a --exclude target programs Cargo.toml Cargo.lock Anchor.toml "$B/"
  # the program id is baked into the .so (declare_id!): swap it in the SCRATCH copy only
  local old; old=$(grep -o 'declare_id!("[A-Za-z0-9]*")' programs/core-vault/src/lib.rs | sed 's/declare_id!("\(.*\)")/\1/')
  sed -i.bak "s/$old/$pid/" "$B/programs/core-vault/src/lib.rs" "$B/Anchor.toml"; rm -f "$B/programs/core-vault/src/lib.rs.bak" "$B/Anchor.toml.bak"
  (cd "$B/programs/core-vault" && cargo build-sbf --features localnet --sbf-out-dir "$B/out") 2>&1 | tee "$B/build.log"
  if grep -qE 'overflows the maximum allowed frame|Stack offset' "$B/build.log"; then echo "ERROR: stack-frame overflow in the devnet build" >&2; exit 1; fi
  rm -f "$B/out/core_vault-keypair.json"
  echo "devnet vault .so ($pid, PUBLIC test admin keys) sha256: $(shasum -a 256 "$VAULT_SO" | cut -d' ' -f1)"
  cargo build-sbf --manifest-path tools/devnet-sector/program/Cargo.toml --sbf-out-dir tools/devnet-sector/target/deploy
  rm -f tools/devnet-sector/target/deploy/mock_sector-keypair.json
  echo "mock sector .so sha256: $(shasum -a 256 "$SECTOR_SO" | cut -d' ' -f1)"
  cargo build --release --manifest-path tools/devnet-rehearsal/Cargo.toml
}

balance_sol() { solana balance "$(pub "$1")" --url "$URL" --keypair "$D/$1.json" 2>/dev/null | awk '{print $1}'; }

# key, SOL it needs (generous), why
NEEDS=(
  "upgrade-authority 8   program rent for core-vault (532,600 bytes = 3.7 SOL, doubled while the deploy buffer exists) and the mock sector"
  "keeper 0.5            payer of mints, token accounts, claims, trader states, fees"
  "sl8-test 0.5          fee payer and rent payer of the admin transactions (init_vault, register_product, nonce)"
  "bonder1 0.05          pays the rent of its bond position"
  "bonder2 0.05          pays the rent of its bond position"
)

cmd_needs() {
  need_keys; genesis_ok
  local short=0
  for row in "${NEEDS[@]}"; do
    set -- $row; local key=$1 want=$2; local have; have=$(balance_sol "$key"); have=${have:-0}
    if awk "BEGIN{exit !($have < $want)}"; then
      echo "NEEDS FUNDING: $key $(pub "$key") has $have SOL, needs $want SOL"; short=1
    else
      echo "ok: $key $(pub "$key") has $have SOL (needs $want)"
    fi
  done
  [ "$short" = 0 ] || exit 3
}

cmd_fund() {
  need_keys; genesis_ok
  for row in "${NEEDS[@]}"; do
    set -- $row; local key=$1 want=$2; local ok=0
    for attempt in $(seq 1 30); do
      local have; have=$(balance_sol "$key"); have=${have:-0}
      if awk "BEGIN{exit !($have >= $want)}"; then ok=1; break; fi
      for amt in 5 2 1; do
        if solana airdrop "$amt" "$(pub "$key")" --url "$URL" --keypair "$D/$key.json" 2>&1 | grep -q 'Signature:'; then echo "$key +$amt SOL"; break; fi
      done
      sleep $(( attempt * 15 < 120 ? attempt * 15 : 120 ))
    done
    [ "$ok" = 1 ] || echo "$key: not funded (faucet rate limited); see 'needs'" >&2
  done
  cmd_needs
}

deploy_one() { # name so
  local name=$1 so=$2 pid; pid=$(pub "$name")
  if solana program show "$pid" --url "$URL" --keypair "$D/upgrade-authority.json" >/dev/null 2>&1; then echo "$name $pid already deployed"; return; fi
  solana program deploy "$so" --program-id "$D/$name.json" --keypair "$D/upgrade-authority.json" \
    --upgrade-authority "$D/upgrade-authority.json" --max-len "$(wc -c <"$so" | tr -d ' ')" --url "$URL"
  echo "$name $pid deployed; .so sha256 $(shasum -a 256 "$so" | cut -d' ' -f1)"
}

cmd_deploy() {
  need_keys; genesis_ok
  [ -f "$VAULT_SO" ] && [ -f "$SECTOR_SO" ] || { echo "run 'build' first" >&2; exit 1; }
  deploy_one program-vault "$VAULT_SO"
  deploy_one program-sector "$SECTOR_SO"
}

cmd_run() {
  need_keys; genesis_ok
  tools/devnet-rehearsal/target/release/devnet-rehearsal --rpc-url "$URL" --cluster devnet --expect-genesis "$DEVNET_GENESIS" \
    --keys-dir "$D" --work-dir "$D/rehearsal-work-devnet" --vault-program "$(pub program-vault)" --sector-program "$(pub program-sector)"
}

case "$CMD" in
  build) cmd_build ;;
  fund) cmd_fund ;;
  needs) cmd_needs ;;
  deploy) cmd_deploy ;;
  run) cmd_run ;;
  all) cmd_build; cmd_fund; cmd_deploy; cmd_run ;;
  *) echo "unknown command '$CMD'" >&2; exit 2 ;;
esac
