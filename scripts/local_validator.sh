#!/usr/bin/env bash
# Run the whole demo against a local solana-test-validator, the same binary the
# devnet run uses. Needs: agave CLI on PATH, the program built
# (cd programs/vineland-pool && cargo-build-sbf), and ~/.config/solana/id.json.
# The ledger lives in a temp dir and is deleted on exit: a test validator's
# ledger grows without bound while it runs.
set -euo pipefail
cd "$(dirname "$0")/.."
LEDGER="$(mktemp -d)"
solana-test-validator --reset --quiet --ledger "$LEDGER" --rpc-port 8899 \
  --bpf-program AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx \
  programs/vineland-pool/target/deploy/vineland_pool.so &
VALIDATOR=$!
trap 'kill $VALIDATOR; wait $VALIDATOR 2>/dev/null; rm -rf "$LEDGER"' EXIT
curl -s --retry 60 --retry-connrefused --retry-delay 1 http://127.0.0.1:8899 \
  -X POST -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' >/dev/null
solana airdrop 10 "$(solana address)" --url http://127.0.0.1:8899 >/dev/null
(cd client && VINELAND_RPC=http://127.0.0.1:8899 cargo run --release --bin devnet_demo)
