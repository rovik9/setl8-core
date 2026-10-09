#!/usr/bin/env bash
# Fails if the STAGED diff adds anything that looks like a private key: a JSON array of 64 numbers
# (a solana-keygen keypair file), or a base58 string that decodes to exactly 64 bytes (a keypair
# in base58, which is also what a transaction signature looks like: signatures in docs are cut to
# 20 characters on purpose). Run before every commit: scripts/secret-scan-staged.sh
set -euo pipefail
cd "$(dirname "$0")/.."
git diff --cached -U0 --no-color | python3 -c '
import re, sys
ALPHA = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58len(s):
    n = 0
    for c in s:
        n = n * 58 + ALPHA.index(c)
    raw = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return len(raw) + (len(s) - len(s.lstrip("1")))
bad = []
file = ""
for line in sys.stdin:
    if line.startswith("+++ "):
        file = line[6:].strip(); continue
    if not line.startswith("+") or line.startswith("+++"):
        continue
    for m in re.finditer(r"\[\s*(?:\d{1,3}\s*,\s*){63}\d{1,3}\s*\]", line):
        nums = [int(x) for x in re.findall(r"\d+", m.group(0))]
        if len(nums) == 64 and all(n < 256 for n in nums):
            bad.append((file, "JSON array of 64 bytes"))
    for m in re.finditer(r"[1-9A-HJ-NP-Za-km-z]{80,90}", line):
        if b58len(m.group(0)) == 64:
            bad.append((file, "base58 string decoding to 64 bytes: " + m.group(0)[:6] + "..."))
if bad:
    for f, why in bad:
        print(f"SECRET-SCAN FAIL: {f}: {why}", file=sys.stderr)
    sys.exit(1)
print("secret scan: staged diff is clean")
'
