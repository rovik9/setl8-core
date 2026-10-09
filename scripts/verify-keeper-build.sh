#!/usr/bin/env bash
# Builds the setl8-keeper in release mode with DEFAULT features and proves it was built for the REAL deployment:
# the real admin pubkeys are inside (it derives the vault address from them) and `setl8-keeper version` reports
# the REAL keys as compiled in. Unlike the program, the keeper binary ALSO contains the two public test admin
# pubkeys on purpose: they are in its list of keys it refuses to use as a fee payer.
# The keeper holds no admin key and refuses to run with one.
#
# Usage: scripts/verify-keeper-build.sh
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release --manifest-path tools/keeper/Cargo.toml
bin="tools/keeper/target/release/setl8-keeper"
echo
echo "built $bin"
echo "sha256: $(shasum -a 256 "$bin" | cut -d' ' -f1)"
ver="$("$bin" version)"
echo "$ver"
case "$ver" in *"REAL admin keys"*) ;; *) echo "FAIL: this binary was not built with the real admin keys" >&2; exit 1 ;; esac

python3 -I - "$bin" <<'PY'
import sys
ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58_32(s):
    n = 0
    for ch in s:
        n = n * 58 + ALPHABET.index(ch)
    return n.to_bytes(32, "big")
data = open(sys.argv[1], "rb").read()
ok = True
for label, addr in (("REAL SL8 admin", "SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ"), ("REAL ROV admin", "RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ")):
    present = b58_32(addr) in data
    ok &= present
    print(f"  {'PASS' if present else 'FAIL'}  {label} {addr}: {'present' if present else 'absent'} (expected present)")
if not ok:
    print("\nFAIL: this keeper does not embed the real admin keys.", file=sys.stderr)
    sys.exit(1)
print("\nOK: the keeper embeds the real admin keys and was built for them.")
PY
