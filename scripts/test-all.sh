#!/usr/bin/env bash
# Full local test run; stops at the first failure.
#
#   1. builds the LOCALNET (public test keys) .so into target/test-deploy/
#      (stack-frame checked); target/deploy/ is never touched
#   2. cargo test -p core-vault, default features (real-key constants pinned) and
#      with --features localnet (test-key constants pinned)
#   3. tests-rs LiteSVM suite (loads target/test-deploy/core_vault.so)
#   4. TypeScript suite on a validator that loads the same .so
#   5. the setl8-admin signing tool (own workspace, own lockfile): unit tests with the
#      default (REAL-key) constants, then the full LiteSVM ceremony suite with the
#      `localnet` test keys
#   6. the mock sector program used by the devnet rehearsal (own workspace): build-sbf, then its
#      LiteSVM tests; and a compile check of the rehearsal driver
#   7. the setl8-keeper payout-cycle runner (own workspace, own lockfile): unit tests with the default
#      (REAL-key) constants, then the full LiteSVM suite (needs the localnet vault .so from step 1 and the
#      mock sector .so from step 6) with the `localnet` test keys
#
# Usage: scripts/test-all.sh
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n==== %s ====\n' "$1"; }

step "1/7 build localnet test .so"
scripts/build-test-so.sh

step "2/7 cargo test -p core-vault (default = real keys)"
cargo test -p core-vault

step "2/7 cargo test -p core-vault --features localnet (test keys)"
cargo test -p core-vault --features localnet

step "3/7 tests-rs (LiteSVM)"
cargo test --manifest-path tests-rs/Cargo.toml

step "4/7 TypeScript suite"
scripts/test-ts.sh

step "5/7 setl8-admin tool (default = real keys)"
cargo test --manifest-path tools/admin/Cargo.toml

step "5/7 setl8-admin tool (--features localnet: full ceremony suite)"
cargo test --manifest-path tools/admin/Cargo.toml --features localnet

step "6/7 mock sector program (build-sbf + LiteSVM tests) and the rehearsal driver (build)"
rm -f tools/devnet-sector/target/deploy/mock_sector-keypair.json
cargo build-sbf --manifest-path tools/devnet-sector/program/Cargo.toml --sbf-out-dir tools/devnet-sector/target/deploy
rm -f tools/devnet-sector/target/deploy/mock_sector-keypair.json
cargo test --manifest-path tools/devnet-sector/Cargo.toml --workspace
cargo build --manifest-path tools/devnet-rehearsal/Cargo.toml

step "7/7 setl8-keeper (default = real keys)"
cargo test --manifest-path tools/keeper/Cargo.toml

step "7/7 setl8-keeper (--features localnet: full LiteSVM suite)"
cargo test --manifest-path tools/keeper/Cargo.toml --features localnet

printf '\nALL GREEN\n'
