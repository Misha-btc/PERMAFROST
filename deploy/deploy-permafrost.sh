#!/usr/bin/env bash
# PERMAFROST flagship deploy — target slot 4:365 (verified empty on mainnet 2026-08-14).
#
# Mechanics (from alkanes CREATERESERVED semantics):
#   protostone [3,365, <init args>] stores the attached WASM envelope at [4:365]
#   AND executes the init cellpack atomically. If init REVERTS, the binary
#   storage rolls back too — the deploy silently fails. So we Initialize
#   (opcode 0) in the same protostone, with args that cannot revert.
#
# Init cellpack (opcode 0):
#   [0, underlying_block, underlying_tx, penalty_bps, name, symbol, interval, periods]
#   penalty_bps = 1000  (10% tithe)
#   name  = "PERMAFROST" packed u128 LE = 398215580027291461371216
#   symbol= "FROST"      packed u128 LE = 362858049094
#   interval = 144 (one drip per Bitcoin day), periods = 365 (the horizon)
#
# ── VERIFY BEFORE MAINNET ──────────────────────────────────────────────
# 1. UNDERLYING must be the LP token the BTCUSD pool actually mints to
#    liquidity providers. Confirm from a real add-liquidity trace (which
#    alkane id lands at the provider's pointer output) before trusting
#    the default below. Initialize binds it immutably — wrong id means
#    redeploying at a different slot.
# 2. Re-probe slot 4:365 is still empty at broadcast time (first come):
#    see probe_slot below. If sniped: fallbacks 4:144, 4:273 (both empty
#    as of 2026-08-14).
# 3. Rehearse on regtest first (PROVIDER=subfrost-regtest, any token id
#    as underlying, --mine).
# ──────────────────────────────────────────────────────────────────────

set -euo pipefail

CLI="${CLI:-$HOME/GitHub/alkanes-rs/target/release/alkanes-cli}"
PROVIDER="${PROVIDER:-subfrost-regtest}"        # switch to mainnet deliberately
WALLET="${WALLET:-$HOME/.alkanes/wallet.json}"
WASM="${WASM:-$(dirname "$0")/../target/wasm32-unknown-unknown/release/permafrost.wasm}"
FEE_RATE="${FEE_RATE:-2}"

SLOT=365
PENALTY_BPS=1000
NAME_U128=398215580027291461371216   # "PERMAFROST"
SYMBOL_U128=362858049094               # "FROST"
INTERVAL=144
PERIODS=365

# Default underlying = the BTCUSD pool (4:1778) on mainnet — VERIFY (see
# header) that the pool contract IS its own LP token before broadcasting.
UNDERLYING_BLOCK="${UNDERLYING_BLOCK:-4}"
UNDERLYING_TX="${UNDERLYING_TX:-1778}"

probe_slot() {
  local url="$1"
  local body
  body=$(curl -s -m 20 "$url" -H 'Content-Type: application/json' -d "{\"jsonrpc\":\"2.0\",\"method\":\"alkanes_simulate\",\"params\":[{\"target\":\"4:${SLOT}\",\"inputs\":[\"99\"],\"alkanes\":[],\"transaction\":\"0x\",\"block\":\"0x\",\"height\":\"1000000\",\"txindex\":0,\"vout\":0}],\"id\":1}")
  if echo "$body" | grep -q 'unexpected end of file'; then
    echo "slot 4:${SLOT} is EMPTY on ${url} — clear to deploy"
  else
    echo "!! slot 4:${SLOT} looks OCCUPIED on ${url}:"
    echo "$body" | head -c 300
    echo
    exit 1
  fi
}

case "$PROVIDER" in
  mainnet)  probe_slot "https://mainnet.subfrost.io/v4/subfrost" ;;
  subfrost-regtest) probe_slot "https://regtest.subfrost.io/v4/subfrost" ;;
esac

# ── Anonymous build ────────────────────────────────────────────────────
# Release wasm embeds absolute source paths in panic strings (home dir,
# cargo registry). Build with path remapping so the on-chain binary
# carries no identifying paths, then verify before broadcasting.
build_anon() {
  RUSTFLAGS="--remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$(pwd)=/build --remap-path-prefix=$HOME=/anon" \
    cargo build --release --target wasm32-unknown-unknown
}
[ -f "$WASM" ] || { echo "wasm not found: $WASM — run build_anon (macOS: CC=/opt/homebrew/opt/llvm/bin/clang AR=llvm-ar)"; exit 1; }
if strings "$WASM" | grep -qE "/Users/|/home/"; then
  echo "!! wasm embeds user paths — rebuild with build_anon (path remapping) before deploying"
  exit 1
fi

CELLPACK="[3,${SLOT},0,${UNDERLYING_BLOCK},${UNDERLYING_TX},${PENALTY_BPS},${NAME_U128},${SYMBOL_U128},${INTERVAL},${PERIODS}]:v0:v0"
echo "provider:  $PROVIDER"
echo "envelope:  $WASM ($(wc -c < "$WASM") bytes)"
echo "cellpack:  $CELLPACK"
echo "underlying: ${UNDERLYING_BLOCK}:${UNDERLYING_TX}"
echo

MINE_FLAG=""
[ "$PROVIDER" = "subfrost-regtest" ] && MINE_FLAG="--mine"

"$CLI" -p "$PROVIDER" \
  --wallet-file "$WALLET" \
  alkanes execute \
  --envelope "$WASM" \
  --fee-rate "$FEE_RATE" \
  --trace \
  $MINE_FLAG \
  "$CELLPACK"

echo
echo "post-deploy checks:"
echo "  name:   simulate 4:${SLOT} [99]  -> 'PERMAFROST'"
echo "  symbol: simulate 4:${SLOT} [100] -> 'FROST'"
echo "  params: simulate 4:${SLOT} [103] -> 1000; [110] -> 144; [111] -> 365"
echo "  underlying: simulate 4:${SLOT} [104] -> ${UNDERLYING_BLOCK}:${UNDERLYING_TX}"
