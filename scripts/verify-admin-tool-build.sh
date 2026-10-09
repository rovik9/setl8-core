#!/usr/bin/env bash
# Builds the setl8-admin signing tool in release mode with DEFAULT features and proves the
# binary embeds the REAL admin pubkeys and none of the public test keys (the same check
# verify-deploy-build.sh makes on the program). Run it before using the tool for real.
# No network access beyond what cargo needs to fetch crates on a first build.
#
# Usage: scripts/verify-admin-tool-build.sh
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release --manifest-path tools/admin/Cargo.toml
bin="tools/admin/target/release/setl8-admin"
echo
echo "built $bin"
exec scripts/verify-deploy-build.sh "$bin"
