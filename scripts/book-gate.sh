#!/usr/bin/env bash
# Manual/local served order-book gate (NOT run by push/PR CI).
#   scripts/book-gate.sh <OUT_DIR> <replay|real>
# Required (explicit, never discovered):
#   CVD_BOOK_WASM        order-book plugin .wasm to import (caller-supplied)
#   CVD_PG_BIN           directory with initdb, postgres, pg_ctl
# Optional:
#   CVD_LEGACY_WASM_DIR  dir with released cvd_indicator_cvd.wasm + cvd_indicator_flow.wasm
#   CVD_CHROMIUM         browser binary (default /usr/bin/chromium)
# Builds cvd_browser_host.wasm + cvd_browser_coordinator.wasm from THIS checkout
# into OUT/target, assembles the static tree, starts a disposable unix-socket
# PostgreSQL under a fresh mktemp dir, runs the production app_router
# (`postgres_served_book_chain`) and drives real Chromium (tests/book-browser.mjs).
# `replay` uses a SYNTHETIC scripted exchange; `real` opts in to public Binance.
# Every stage fails fast; cleanup is verified and reported.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/out-guard.sh"
OUT=$(out_guard "$ROOT" "${1:?usage: book-gate.sh <OUT_DIR> <replay|real>}") || exit 1
MODE=${2:?usage: book-gate.sh <OUT_DIR> <replay|real>}
case "$MODE" in replay|real) ;; *) echo "FAIL mode must be replay|real" >&2; exit 1;; esac
BOOK=${CVD_BOOK_WASM:?CVD_BOOK_WASM required}
PB=${CVD_PG_BIN:?CVD_PG_BIN required}
LEGACY=${CVD_LEGACY_WASM_DIR:-}
CHROME=${CVD_CHROMIUM:-/usr/bin/chromium}
fail() { echo "FAIL $*" >&2; exit 1; }
magic() { head -c 4 "$1" | od -An -tx1 | grep -q '00 61 73 6d'; }
# Stage 0: explicit inputs exist and are what they claim (identity recorded).
[ -s "$BOOK" ] && magic "$BOOK" || fail "CVD_BOOK_WASM is not a wasm file: $BOOK"
for b in initdb postgres pg_ctl; do [ -x "$PB/$b" ] || fail "missing $PB/$b"; done
if [ -n "$LEGACY" ]; then for f in cvd_indicator_cvd.wasm cvd_indicator_flow.wasm; do
  [ -s "$LEGACY/$f" ] && magic "$LEGACY/$f" || fail "legacy artifact missing/invalid: $LEGACY/$f"; done; fi
# Drop ambient overrides; only the values validated above are passed on.
for v in $(env | sed -n 's/^\(CVD_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
unset DATABASE_URL CARGO_TARGET_DIR
export CVD_REPS=1
mkdir -p "$OUT"; cd "$ROOT"
T=$OUT/target
echo "== stage build: trusted host + coordinator from this checkout -> $T"
cargo build --locked --target-dir "$T/host" --profile wasm-release --target wasm32-unknown-unknown -p cvd-browser-host > "$OUT/build-host.log" 2>&1 || fail "host build (see build-host.log)"
( cd crates/browser-coordinator && cargo build --locked --release --target wasm32-unknown-unknown --target-dir "$T/coordinator" ) > "$OUT/build-coordinator.log" 2>&1 || fail "coordinator build"
H=$T/host/wasm32-unknown-unknown/wasm-release/cvd_browser_host.wasm
C=$T/coordinator/wasm32-unknown-unknown/release/cvd_browser_coordinator.wasm
magic "$H" && magic "$C" || fail "built trusted modules missing"
{ sha256sum "$H" "$C" "$BOOK"; [ -z "$LEGACY" ] || sha256sum "$LEGACY"/cvd_indicator_cvd.wasm "$LEGACY"/cvd_indicator_flow.wasm; } > "$OUT/identity.sha256"
cat "$OUT/identity.sha256"

S=$(mktemp -d "${TMPDIR:-/tmp}/book-gate.XXXXXX"); M=$S/mail; mkdir -p "$S/sock" "$M"; SP=
. scripts/owned-group.sh
cleanup() { rc=$?; set +e
  if [ -n "$SP" ]; then for _ in $(seq 1 100); do owned_alive "$SP" || break; sleep 0.2; done
    owned_reap "$SP" 5; r=$?; [ "$r" -eq 0 ] || { echo "FAIL owned group $SP needed force (rc $r)"; rc=1; }
    s=${OWNED_STATUS:-1}; [ "$s" -eq 0 ] || { echo "FAIL server test exit $s"; [ $rc -ne 0 ] || rc=1; }; fi
  if [ -f "$S/data/postmaster.pid" ]; then "$PB"/pg_ctl -D "$S/data" stop -m fast -w >/dev/null 2>&1 || { echo "FAIL pg_ctl stop"; rc=1; }; fi
  [ ! -f "$S/data/postmaster.pid" ] || { echo "FAIL postmaster still present; keeping $S"; exit 1; }
  rm -rf "$S"; [ ! -e "$S" ] || { echo "FAIL $S not removed"; rc=1; }
  echo "BOOK_GATE_CLEANUP rc=$rc"; exit $rc; }
trap cleanup EXIT
echo "== stage pg: disposable unix-socket cluster"
"$PB"/initdb -D "$S/data" -A trust -U cvdtest >/dev/null || fail initdb
echo "CREATE DATABASE cvd_retirement_test;" | "$PB"/postgres --single -D "$S/data" postgres >/dev/null || fail createdb
"$PB"/pg_ctl -D "$S/data" -o "-k $S/sock -c listen_addresses=''" -l "$S/log" -w start >/dev/null || fail "pg start"
echo "== stage assemble static"
sh scripts/assemble-static.sh static "$H" "$C" "$S/static" > "$OUT/assets.json" || fail assemble
echo "== stage server ($MODE)"
FEED=replay; [ "$MODE" = real ] && FEED=real
CVD_BOOK_FEED=$FEED CVD_RETIREMENT_TEST_URL="postgres:///cvd_retirement_test?host=$S/sock&user=cvdtest" CVD_SERVED_CHAIN_DIR=$M CVD_SERVED_STATIC_DIR=$S/static \
  owned_spawn timeout 900 cargo test --locked -p cvd-server postgres_served_book_chain -- --ignored --nocapture > "$OUT/server-$MODE.log" 2>&1
SP=$OWNED_PID
for _ in $(seq 1 3000); do [ -s "$M/port" ] && break; owned_alive "$SP" || fail "server exited before listening"; sleep 0.2; done
[ -s "$M/port" ] || fail "server did not listen"
PORT=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["port"])' "$M/port")
echo "== stage served bytes (port $PORT)"
bash scripts/verify-served-assets.sh "http://127.0.0.1:$PORT" > "$OUT/served-assets.log" 2>&1 || fail "served trusted assets"
V=$(sed -n 's/.*app\.js?v=\([a-z0-9-]*\).*/\1/p' "$S/static/index.html")
for f in index.html "css/app.css?v=$V" "js/app.js?v=$V" "js/book-panel.js?v=$V" "wasm-host/host-bridge.js?v=$V"; do
  curl -sS --max-time 20 -o "$S/served.tmp" "http://127.0.0.1:$PORT/${f#index.html}" || fail "fetch $f"
  cmp -s "$S/served.tmp" "$S/static/${f%%\?*}" || fail "served bytes differ: $f"
done
echo "served bytes OK (cache token $V)" | tee -a "$OUT/served-assets.log"
echo "== stage browser"
CVD_SERVED_CHAIN_DIR=$M CVD_SERVED_STATIC_DIR=$S/static CVD_BOOK_WASM=$BOOK CVD_LEGACY_WASM_DIR=$LEGACY CVD_CHROMIUM=$CHROME \
  CVD_BOOK_SHOTS=$OUT/shots timeout 600 node tests/book-browser.mjs > "$OUT/browser-$MODE.log" 2>&1
BRC=$?; echo "browser rc=$BRC" | tee -a "$OUT/browser-$MODE.log"
grep -E '^(PASS|FAIL)' "$OUT/browser-$MODE.log" | sort | uniq -c | awk '{print $2}' | sort | uniq -c
[ "$BRC" -eq 0 ] || exit 1
! grep -q '^FAIL' "$OUT/browser-$MODE.log" || exit 1
echo "BOOK GATE PASS ($MODE)"
exit 0
