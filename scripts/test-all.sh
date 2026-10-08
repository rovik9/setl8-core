#!/usr/bin/env bash
# Full local test run; stops at the first failure.
#
#   1. builds the LOCALNET (public test keys) .so into target/test-deploy/
#      (stack-frame checked); target/deploy/ is never touched
#   2. cargo test -p core-vault, default features (real-key constants pinned) and
#      with --features localnet (test-key constants pinned)
#   3. tests-rs LiteSVM suite (loads target/test-deploy/core_vault.so)
#   4. TypeScript suite on a validator that loads the same .so
#
# Usage: scripts/test-all.sh
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n==== %s ====\n' "$1"; }

step "1/4 build localnet test .so"
scripts/build-test-so.sh

step "2/4 cargo test -p core-vault (default = real keys)"
cargo test -p core-vault

step "2/4 cargo test -p core-vault --features localnet (test keys)"
cargo test -p core-vault --features localnet

step "3/4 tests-rs (LiteSVM)"
cargo test --manifest-path tests-rs/Cargo.toml

step "4/4 TypeScript suite"
scripts/test-ts.sh

printf '\nALL GREEN\n'
