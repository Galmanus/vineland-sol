#!/usr/bin/env bash
# Committee-free Vineland, end to end, on a throwaway local validator: both
# programs loaded at their real ids, the relayer over HTTP, and the vineland
# CLI for every role (operator, three members, a clean wallet). Nothing here
# touches devnet or mainnet; every key is fresh and the ledger lives in a temp
# dir that is removed on exit.
#
#   client/scripts/local_e2e.sh
#
# Needs both SBF artifacts (or their paths in VINELAND_POOL_V2_SO /
# RIVERRUN_VERIFIER_SO) and agave's solana-test-validator, solana,
# solana-keygen and spl-token (SOLANA_BIN). Builds the two binaries first.
# Exits non-zero if any step fails; the validator and the relayer are killed
# on exit, Ctrl-C included.
set -euo pipefail

SOLANA_BIN=${SOLANA_BIN:-$HOME/.local/share/solana/install/active_release/bin}
CLIENT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
POOL_SO=${VINELAND_POOL_V2_SO:-$CLIENT/../programs/vineland-pool/target/deploy/vineland_pool.so}
VERIFIER_SO=${RIVERRUN_VERIFIER_SO:-$CLIENT/../../mirror-pool/.claude/worktrees/agent-ae0080ba263a865c1/programs/riverrun-m31-verifier/target/deploy/riverrun_m31_verifier.so}
POOL_ID=3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi
VERIFIER_ID=8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo
RPC_PORT=${RPC_PORT:-18899}
RELAYER_PORT=${RELAYER_PORT:-18787}
URL=http://127.0.0.1:$RPC_PORT
RELAYER=http://127.0.0.1:$RELAYER_PORT
NOTE=1000000 # 1.00 of a 6-decimal test dollar
FEE_BPS=197
K_MIN=3

WORK=$(mktemp -d "${TMPDIR:-/tmp}/vineland-e2e.XXXXXX")
VALIDATOR_PID=
RELAYER_PID=

stop() {
    local pid=$1
    [ -n "$pid" ] || return 0
    kill "$pid" 2>/dev/null || return 0
    for _ in $(seq 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

cleanup() {
    local code=$?
    set +e
    trap - EXIT INT TERM
    if [ "$code" -ne 0 ]; then
        echo "== FAILED (exit $code); last relayer and validator log lines:"
        tail -n 20 "$WORK/relayer.log" 2>/dev/null
        tail -n 20 "$WORK/validator.log" 2>/dev/null
    fi
    stop "$RELAYER_PID"
    stop "$VALIDATOR_PID"
    rm -rf "$WORK"
    exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

step() { echo; echo "== $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }
pubkey() { "$SOLANA_BIN/solana-keygen" pubkey "$WORK/$1.json"; }
# Field "name: value" from a CLI run's saved output.
field() { sed -n "s/^$1: *\([^ ]*\).*/\1/p" "$2" | head -n 1; }
# Raw token amount of `owner`'s associated account (0 if it has none).
balance_of() {
    "$SOLANA_BIN/spl-token" --config "$WORK/cli.yml" balance "$MINT" --owner "$1" --output json-compact 2>/dev/null \
        | sed -n 's/.*"amount":"\([0-9]*\)".*/\1/p' || true
}
expect_refusal() {
    local what=$1
    shift
    if "$@" >"$WORK/refusal.out" 2>&1; then
        cat "$WORK/refusal.out"
        die "$what: expected a refusal, it succeeded"
    fi
    echo "  refused as expected: $what: $(grep -m 1 -i -E 'error|refus' "$WORK/refusal.out" | cut -c 1-160)"
}

for f in "$POOL_SO" "$VERIFIER_SO"; do
    [ -f "$f" ] || die "missing program artifact $f"
done

step "build"
(cd "$CLIENT" && cargo build --release --bin vineland --bin relayer 2>&1 | tail -n 1)
VINELAND=$CLIENT/target/release/vineland
RELAYER_BIN=$CLIENT/target/release/relayer
export VINELAND_RPC=$URL

step "guards: no binary here talks to mainnet"
expect_refusal "relayer on mainnet" env VINELAND_RPC=https://api.mainnet-beta.solana.com RELAYER_KEYPAIR=/dev/null "$RELAYER_BIN"
expect_refusal "cli on mainnet" "$VINELAND" status --pool "$POOL_ID" --url https://api.mainnet-beta.solana.com

step "local validator (ledger in $WORK)"
"$SOLANA_BIN/solana-test-validator" \
    --ledger "$WORK/ledger" --reset --quiet \
    --bind-address 127.0.0.1 --rpc-port "$RPC_PORT" --faucet-port $((RPC_PORT + 1001)) \
    --gossip-port 18090 --dynamic-port-range 18100-18200 \
    --bpf-program "$POOL_ID" "$POOL_SO" \
    --bpf-program "$VERIFIER_ID" "$VERIFIER_SO" \
    >"$WORK/validator.log" 2>&1 &
VALIDATOR_PID=$!
for i in $(seq 120); do
    kill -0 "$VALIDATOR_PID" 2>/dev/null || die "validator exited"
    if curl -sf -X POST -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$URL" | grep -q '"ok"'; then
        break
    fi
    [ "$i" -lt 120 ] || die "validator not healthy after 60 s"
    sleep 0.5
done
"$SOLANA_BIN/solana" config set --config "$WORK/cli.yml" --url "$URL" --keypair "$WORK/operator.json" --commitment confirmed >/dev/null
S=("$SOLANA_BIN/solana" --config "$WORK/cli.yml")
SPL=("$SOLANA_BIN/spl-token" --config "$WORK/cli.yml")
echo "  $("${S[@]}" cluster-version) at $URL"
for id in "$POOL_ID" "$VERIFIER_ID"; do
    "${S[@]}" account "$id" >/dev/null || die "program $id not loaded"
done

step "fresh keys, funded by the local faucet (the clean wallet gets nothing)"
for k in operator relayer member0 member1 member2 clean mint; do
    "$SOLANA_BIN/solana-keygen" new --no-bip39-passphrase --silent --outfile "$WORK/$k.json" >/dev/null
done
OPERATOR=$(pubkey operator)
RELAYER_KEY=$(pubkey relayer)
CLEAN=$(pubkey clean)
"${S[@]}" airdrop 10 "$OPERATOR" >/dev/null
"${S[@]}" airdrop 5 "$RELAYER_KEY" >/dev/null
for i in 0 1 2; do
    "${S[@]}" airdrop 1 "$(pubkey member$i)" >/dev/null
done
echo "  operator $OPERATOR, relayer $RELAYER_KEY, clean wallet $CLEAN"

step "a 6-decimal test dollar; one note's worth per member"
"${SPL[@]}" create-token --decimals 6 "$WORK/mint.json" >/dev/null
MINT=$(pubkey mint)
for i in 0 1 2; do
    m=$(pubkey member$i)
    "${SPL[@]}" create-account "$MINT" --owner "$m" --fee-payer "$WORK/operator.json" >/dev/null
    "${SPL[@]}" mint "$MINT" 1 --recipient-owner "$m" >/dev/null
done
echo "  mint $MINT"

step "relayer"
RELAYER_KEYPAIR="$WORK/relayer.json" BIND="127.0.0.1:$RELAYER_PORT" "$RELAYER_BIN" >"$WORK/relayer.log" 2>&1 &
RELAYER_PID=$!
for i in $(seq 60); do
    kill -0 "$RELAYER_PID" 2>/dev/null || die "relayer exited"
    curl -sf "$RELAYER/health" >"$WORK/health.json" 2>/dev/null && break
    [ "$i" -lt 60 ] || die "relayer not up after 30 s"
    sleep 0.5
done
echo "  GET /health: $(cat "$WORK/health.json")"
code=$(curl -s -o "$WORK/bad.json" -w '%{http_code}' -X POST --data 'not a bundle' "$RELAYER/withdraw")
[ "$code" = 400 ] || die "garbage bundle: expected 400, got $code"
echo "  POST /withdraw with garbage: $code $(cat "$WORK/bad.json")"
RELAYER_SOL_BEFORE=$("${S[@]}" balance "$RELAYER_KEY" --lamports | cut -d' ' -f1)

step "operator init"
"$VINELAND" operator init --mint "$MINT" --note "$NOTE" --fee-bps "$FEE_BPS" --k-min "$K_MIN" --keypair "$WORK/operator.json" | tee "$WORK/init.out"
POOL=$(field pool "$WORK/init.out")
TREASURY=$(field treasury "$WORK/init.out")
[ -n "$POOL" ] || die "no pool address printed"

step "three members: allow, note new, deposit (the relayer folds each leaf)"
for i in 0 1 2; do
    m=$(pubkey member$i)
    "$VINELAND" operator allow --pool "$POOL" --member "$m" --keypair "$WORK/operator.json" | sed "s/^/  [member $i] /"
    "$VINELAND" note new --pool "$POOL" --out "$WORK/note$i.json" 2>"$WORK/note$i.warn" | sed "s/^/  [member $i] /"
    perms=$(stat -c %a "$WORK/note$i.json")
    [ "$perms" = 600 ] || die "note file mode $perms, expected 600"
    grep -q "IS THE MONEY" "$WORK/note$i.warn" || die "no warning printed for the note file"
    "$VINELAND" deposit --note "$WORK/note$i.json" --keypair "$WORK/member$i.json" --relayer "$RELAYER" | sed "s/^/  [member $i] /"
    if [ "$i" = 1 ]; then
        expect_refusal "withdraw with 2 notes under a k_min of 3" \
            "$VINELAND" withdraw --note "$WORK/note0.json" --to "$CLEAN" --relayer "$RELAYER"
    fi
done
expect_refusal "a note file is never overwritten" "$VINELAND" note new --pool "$POOL" --out "$WORK/note0.json"
"$VINELAND" status --pool "$POOL" | sed 's/^/  /'

step "member 0 withdraws through the relayer to a fresh wallet (member 0's key is not used)"
"$VINELAND" withdraw --note "$WORK/note0.json" --to "$CLEAN" --relayer "$RELAYER" | tee "$WORK/withdraw.json"
grep -q '"withdraw"' "$WORK/withdraw.json" || die "no withdraw signature returned"
expect_refusal "withdraw the same note again" "$VINELAND" withdraw --note "$WORK/note0.json" --to "$CLEAN" --relayer "$RELAYER"

step "member 2 exits in public (reveals its secret, refund to its own account)"
"$VINELAND" exit --note "$WORK/note2.json" --payer "$WORK/member2.json" | sed 's/^/  /'
expect_refusal "exit the same note again" "$VINELAND" exit --note "$WORK/note2.json" --payer "$WORK/member2.json"
expect_refusal "exit a note that was withdrawn" "$VINELAND" exit --note "$WORK/note0.json" --payer "$WORK/member0.json"

step "status"
"$VINELAND" status --pool "$POOL" | tee "$WORK/status.out" | sed 's/^/  /'

step "relayer log"
sed 's/^/  /' "$WORK/relayer.log"

step "balances (raw units of the 6-decimal test dollar)"
CLEAN_BAL=$(balance_of "$CLEAN")
TREASURY_BAL=$(balance_of "$OPERATOR")
VAULT_BAL=$(field vault "$WORK/status.out")
M0=$(balance_of "$(pubkey member0)")
M1=$(balance_of "$(pubkey member1)")
M2=$(balance_of "$(pubkey member2)")
CLEAN_SOL=$("${S[@]}" balance "$CLEAN" --lamports | cut -d' ' -f1)
RELAYER_SOL_AFTER=$("${S[@]}" balance "$RELAYER_KEY" --lamports | cut -d' ' -f1)
FEE=$((NOTE * FEE_BPS / 10000))
printf '  %-34s %10s  (expected %s)\n' \
    "clean wallet $CLEAN" "$CLEAN_BAL" "$((NOTE - FEE))" \
    "treasury $TREASURY" "$TREASURY_BAL" "$FEE" \
    "vault (member 1's note)" "$VAULT_BAL" "$NOTE" \
    "member 0 (withdrew privately)" "$M0" 0 \
    "member 1 (still in the pool)" "$M1" 0 \
    "member 2 (exited, refunded)" "$M2" "$NOTE" \
    "clean wallet SOL (lamports)" "$CLEAN_SOL" 0
echo "  relayer SOL: $RELAYER_SOL_BEFORE -> $RELAYER_SOL_AFTER lamports, spent $((RELAYER_SOL_BEFORE - RELAYER_SOL_AFTER)): fees for 9 folds and one withdrawal, the clean wallet's token account and the nullifier record (the buffer rent came back)"
[ "$CLEAN_BAL" = $((NOTE - FEE)) ] || die "clean wallet balance"
[ "$TREASURY_BAL" = "$FEE" ] || die "treasury balance"
[ "$VAULT_BAL" = "$NOTE" ] || die "vault balance"
[ "$M0" = 0 ] && [ "$M1" = 0 ] || die "member 0/1 balances"
[ "$M2" = "$NOTE" ] || die "member 2 refund"
[ "$CLEAN_SOL" = 0 ] || die "the clean wallet received SOL from someone"
echo
echo "local e2e: OK"
