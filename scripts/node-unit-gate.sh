#!/usr/bin/env bash
# Self-contained frontend unit-contract gate (CI "Frontend unit contracts" and
# the documented local command take this same path).
#   scripts/node-unit-gate.sh <OUT_DIR>
# Builds the trusted Rust coordinator wasm from THIS checkout into the owned
# target OUT/target/coordinator, then runs the full Node suite with
# CVD_COORDINATOR_WASM pointing at exactly that artifact. Never reads a
# pre-generated static/wasm-host/*.wasm. Exit 0 only when the build ran, the
# artifact exists, and the suite reported zero failures with the expected
# inventory (NODE_UNIT_EXPECT="<tests> <pass> <skipped>", default below).
# NODE_UNIT_GATE_INJECT=<build|inventory> forces that step to fail (control).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/out-guard.sh"
# Validate canonical OUT against the source tree BEFORE any mkdir/rm below.
OUT=$(out_guard "$ROOT" "${1:?OUT_DIR}"); mkdir -p "$OUT"; cd "$ROOT"
# Refuse ambient artifact overrides: the only coordinator is the one built here.
for v in $(env | sed -n 's/^\(CVD_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
unset CARGO_TARGET_DIR
inject() { if [ "${NODE_UNIT_GATE_INJECT:-}" = "$1" ]; then echo "injected failure: $1" >&2; exit 1; fi; }
T=$OUT/target/coordinator
COORD=$T/wasm32-unknown-unknown/release/cvd_browser_coordinator.wasm
rm -f "$COORD" "$OUT/coordinator.sha256" "$OUT/node-tests.log"
echo "== build trusted coordinator wasm from source -> $T"
inject build
echo "== acquire locked deps of standalone coordinator workspace (network; honours CARGO_NET_OFFLINE)"
( cd crates/browser-coordinator && cargo fetch --locked )
( cd crates/browser-coordinator && cargo build --offline --locked --release --target wasm32-unknown-unknown --target-dir "$T" )
test -s "$COORD" || { echo "FAIL coordinator artifact missing: $COORD"; exit 1; }
sha256sum "$COORD" > "$OUT/coordinator.sha256"; cat "$OUT/coordinator.sha256"
echo "== node $(node --version): full unit suite"
set +e
CVD_COORDINATOR_WASM=$COORD node --test --test-reporter=tap tests/*.test.cjs tests/*.test.mjs static/js/*.test.cjs > "$OUT/node-tests.log" 2>&1
rc=$?; set -e
tail -n 9 "$OUT/node-tests.log"
[ "$rc" -eq 0 ] || { grep -n '^not ok' "$OUT/node-tests.log" || true; echo "FAIL node suite rc=$rc"; exit "$rc"; }
inject inventory
read -r et ep es <<<"${NODE_UNIT_EXPECT:-168 152 16}"
n() { sed -n "s/^# $1 \([0-9]*\)$/\1/p" "$OUT/node-tests.log"; }
got="$(n tests) $(n pass) $(n fail) $(n skipped)"
[ "$got" = "$et $ep 0 $es" ] || { echo "FAIL node inventory tests/pass/fail/skipped=$got expected $et $ep 0 $es"; exit 1; }
echo "node-unit-gate OK: tests/pass/fail/skipped=$got"
