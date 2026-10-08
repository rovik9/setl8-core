#!/usr/bin/env bash
# Pre-deploy gate: proves the deployable program embeds the REAL admin pubkeys
# and NONE of the public test keys. Run it before any deploy.
#
# With no argument it first runs the default build (stack-frame checked), then
# inspects target/deploy/core_vault.so. With a path argument (or $CORE_VAULT_SO)
# it skips the build and inspects that file instead -- used to show that a
# localnet-built .so is rejected.
#
# Exit 0 only if the .so contains the raw 32 bytes of BOTH real addresses and of
# NEITHER test address. No network access.
#
# Usage: scripts/verify-deploy-build.sh [path/to/core_vault.so]
set -uo pipefail

cd "$(dirname "$0")/.."

REAL_SL8="SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ"
REAL_ROV="RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ"
TEST_SL8="9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP"
TEST_ROV="D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6"

so="${1:-${CORE_VAULT_SO:-}}"
if [ -z "$so" ]; then
  DEFAULT_ONLY=1 scripts/anchor-build-checked.sh || exit 1
  so="target/deploy/core_vault.so"
fi
[ -f "$so" ] || { echo "ERROR: $so not found" >&2; exit 1; }

echo
echo "verifying $so"
echo "sha256: $(shasum -a 256 "$so" | cut -d' ' -f1)"

python3 -I - "$so" "$REAL_SL8" "$REAL_ROV" "$TEST_SL8" "$TEST_ROV" <<'PY'
import sys

ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

def b58_32(s):
    n = 0
    for ch in s:
        n = n * 58 + ALPHABET.index(ch)
    raw = n.to_bytes(32, "big")  # raises OverflowError if it does not fit 32 bytes
    # base58 keeps leading '1's as leading zero bytes; to_bytes already pads.
    return raw

so, real_sl8, real_rov, test_sl8, test_rov = sys.argv[1:]
data = open(so, "rb").read()
checks = [
    ("REAL SL8 admin", real_sl8, True),
    ("REAL ROV admin", real_rov, True),
    ("TEST SL8 admin", test_sl8, False),
    ("TEST ROV admin", test_rov, False),
]
ok = True
for label, addr, must_have in checks:
    present = b58_32(addr) in data
    good = present == must_have
    ok &= good
    print(f"  {'PASS' if good else 'FAIL'}  {label} {addr}: "
          f"{'present' if present else 'absent'} (expected {'present' if must_have else 'absent'})")
if not ok:
    print("\nFAIL: this .so is NOT a deployable real-key build.", file=sys.stderr)
    sys.exit(1)
print("\nOK: contains both real admin keys and no test keys.")
PY
