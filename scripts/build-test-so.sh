#!/usr/bin/env bash
# Builds the LOCALNET (public test keys) program into target/test-deploy/,
# never into target/deploy/ (which is reserved for the real-key build), and
# fails if the SBF toolchain reports a stack-frame overflow.
#
# Usage: scripts/build-test-so.sh
set -uo pipefail

cd "$(dirname "$0")/.."

out="target/test-deploy"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

mkdir -p "$out"
(cd programs/core-vault && cargo build-sbf --features localnet --sbf-out-dir "../../$out") 2>&1 | tee "$log"
status="${PIPESTATUS[0]}"
if [ "$status" -ne 0 ]; then
  echo "localnet build failed (exit $status)" >&2
  exit "$status"
fi

if grep -qE 'overflows the maximum allowed frame|Stack offset' "$log"; then
  echo >&2
  echo "ERROR: the localnet build reported a stack-frame overflow (exceeds 4096 bytes):" >&2
  grep -E 'overflows the maximum allowed frame|Stack offset' "$log" >&2
  echo "Box the large accounts (Box<Account<..>>) or shrink the frame." >&2
  exit 1
fi

# build-sbf drops a throwaway program keypair next to the .so; the test build
# neither needs nor should keep one.
rm -f "$out/core_vault-keypair.json"

so="$out/core_vault.so"
if [ ! -f "$so" ]; then
  echo "ERROR: $so was not produced" >&2
  exit 1
fi
echo "OK: no stack-frame overflow (localnet build)"
echo "localnet (TEST keys) $so sha256: $(shasum -a 256 "$so" | cut -d' ' -f1)"
