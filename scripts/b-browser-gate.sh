#!/usr/bin/env bash
# Lane B gate (durable). Usage: scripts/b-browser-gate.sh <audit-dir> [reps]
# audit-dir holds build/public and A-parent-rev3/fixtures/hostile (no native runner).
# Exits nonzero if ANY stage fails; per-stage codes in $OUT/exit-codes.txt.
# B_GATE_STAGES (space list) restricts stages; B_GATE_INJECT_FAIL=<stage>
# forces that stage to fail (regression check for status aggregation).
set -u
AUD=${1:?audit dir}; REPS=${2:-1}; OUT=${B_GATE_OUT:-$AUD/B-rev2-evidence}
ROOT=$(cd "$(dirname "$0")/.." && pwd); cd "$ROOT"; mkdir -p "$OUT"; : > "$OUT/exit-codes.txt"
W=$AUD/build/public/wasm32-unknown-unknown/wasm-release
STAGES=${B_GATE_STAGES:-coordinator node lwcrender local data p2 toggle interaction}
# One explicit coordinator target dir for test, build AND copy: an inherited
# CARGO_TARGET_DIR must never select a stale artifact.
CT=${B_COORD_TARGET:-$AUD/build/b-coordinator}
FAILED=0
. "$ROOT/scripts/owned-group.sh"
record() { echo "$1=$2" >> "$OUT/exit-codes.txt"; [ "$2" -eq 0 ] || FAILED=1; }
want() { case " $STAGES " in *" $1 "*) return 0;; esac; return 1; }
inject() { if [ "${B_GATE_INJECT_FAIL:-}" = "$1" ]; then echo "injected failure for $1"; return 1; fi; return 0; }

if want coordinator; then
  ( set -e; inject coordinator; cd crates/browser-coordinator
    export CARGO_TARGET_DIR="$CT"
    cargo test --offline --locked
    cargo build --offline --locked --release --target wasm32-unknown-unknown
    A="$CT/wasm32-unknown-unknown/release/cvd_browser_coordinator.wasm"
    cp "$A" "$ROOT/static/wasm-host/cvd_browser_coordinator.wasm"
    cmp "$A" "$ROOT/static/wasm-host/cvd_browser_coordinator.wasm"
    sha256sum "$A" ) > "$OUT/coordinator.log" 2>&1; record coordinator $?
fi
if want node; then
  ( inject node && node --test tests/*.test.cjs tests/*.test.mjs static/js/*.test.cjs ) > "$OUT/node-tests.log" 2>&1; record node $?
fi
# Real-LWC plugin renderer regression (penultimate/historical update,
# null<->value, both band edges, empty->update->clear occupancy).
if want lwcrender; then
  ( inject lwcrender && timeout 120 node tests/lwc-tail-update-browser.mjs ) > "$OUT/lwc-render-browser.log" 2>&1; record lwcrender $?
fi
if want local; then
  ( inject local && CVD_WASM_DIR=$W CVD_HOSTILE_DIR=$AUD/A-parent-rev3/fixtures/hostile CVD_LOCAL_OUT=$OUT/local-wasm timeout 600 node tests/local-wasm-browser.mjs ) > "$OUT/local-wasm-browser.log" 2>&1; record local $?
fi
# DATA slice: Rust-producer-serialized PUBLIC fixture + actual-app data cases.
if want data; then
  ( set -e; inject data
    CVD_PRODUCER_FIXTURE_OUT=$OUT/producer-public-fixture.json cargo test --offline --locked -p cvd-server producer_fixture_export
    CVD_LOCAL_SUITE=data CVD_PRODUCER_FIXTURE=$OUT/producer-public-fixture.json CVD_WASM_DIR=$W CVD_LOCAL_OUT=$OUT/local-data timeout 600 node tests/local-wasm-browser.mjs ) > "$OUT/local-data-browser.log" 2>&1; record data $?
fi
if want p2; then
  # Remaining registry/UI/paging acceptance (PARENT-B-REV6-P2) with the PUBLIC
  # generic prefix fixture built here (own target dir, never a stale copy).
  ( set -e; inject p2; PT=$AUD/build/b-prefix
    cd tests/fixtures/wasm/prefix-plugin
    CARGO_TARGET_DIR=$PT cargo build --offline --locked --release --target wasm32-unknown-unknown
    cp $PT/wasm32-unknown-unknown/release/prefix_indicator.wasm $PT/prefix_v1.wasm
    CARGO_TARGET_DIR=$PT cargo build --offline --locked --release --target wasm32-unknown-unknown --features v2
    cp $PT/wasm32-unknown-unknown/release/prefix_indicator.wasm $PT/prefix_v2.wasm
    sha256sum $PT/prefix_v1.wasm $PT/prefix_v2.wasm; cd "$ROOT"
    CVD_LOCAL_SUITE=p2 CVD_PREFIX_DIR=$PT CVD_WASM_DIR=$W CVD_LOCAL_OUT=$OUT/local-p2 timeout 600 node tests/local-wasm-browser.mjs ) > "$OUT/local-p2-browser.log" 2>&1; record p2 $?
fi
if want toggle; then
  # Toggle durable-receipt / latest-intent / replacement-overlap (PARENT-B-REV7-TOGGLE).
  ( set -e; inject toggle; PT=$AUD/build/b-prefix; test -f $PT/prefix_v2.wasm
    CVD_LOCAL_SUITE=toggle CVD_PREFIX_DIR=$PT CVD_WASM_DIR=$W CVD_LOCAL_OUT=$OUT/local-toggle timeout 300 node tests/local-wasm-browser.mjs ) > "$OUT/local-toggle-browser.log" 2>&1; record toggle $?
fi
if want interaction; then
  ( inject interaction && CVD_REPS=$REPS \
    CVD_LOCAL_WASM=$W/cvd_indicator_cvd.wasm,$W/cvd_indicator_flow.wasm CVD_INTERACTION_OUT=$OUT/interaction timeout 3000 node tests/browser-interaction.mjs ) > "$OUT/browser-interaction.log" 2>&1; record interaction $?
fi
# Owned process-group cleanup control (served forced branch helper).
if want cleanup; then
  ( inject cleanup && bash tests/owned-group-control.sh ) > "$OUT/owned-group-control.log" 2>&1; record cleanup $?
fi
# Self-contained clean-core app/interaction (own builds, TEST-ONLY fixtures).
if want coreapp; then
  ( inject coreapp && bash scripts/core-app-gate.sh "$OUT/core-app" "$REPS" ) > "$OUT/core-app-gate.log" 2>&1; record coreapp $?
fi
# Served chain: REAL app_router (HTTP+WS+static) on a disposable unix-socket PG
# (owned initdb; CVD_PG_BIN = dir with initdb/pg_ctl/postgres) -> Chromium.
if want served; then
  # Owned resources: $S (PG cluster, mailbox, assembled static) and the cargo
  # test pid $SP. Cleanup failures make the stage nonzero; nothing global.
  ( set -e; inject served; PB=${CVD_PG_BIN:?CVD_PG_BIN}; S=$(mktemp -d "${TMPDIR:-/tmp}/served-pg.XXXXXX"); M=$S/mail; mkdir -p $S/sock $M; SP=
    cleanup() { rc=$?; set +e
      if [ -n "$SP" ]; then
        # SP leads its OWN process group (timeout->cargo->Rust test and every
        # descendant): bounded natural exit, then group TERM/KILL + reap.
        for _ in $(seq 1 100); do owned_alive "$SP" || break; sleep 0.2; done
        owned_reap "$SP" 5; r=$?
        [ "$r" -eq 0 ] || { echo "FAIL served owned group $SP needed force/survived (rc $r)"; rc=1; }
        s=${OWNED_STATUS:-1}; [ "$s" -eq 0 ] || { echo "FAIL served server test exit $s"; [ $rc -ne 0 ] || rc=1; }
      fi
      if [ -f "$S/data/postmaster.pid" ]; then "$PB"/pg_ctl -D "$S/data" stop -m fast -w >/dev/null 2>&1 || { echo "FAIL pg_ctl stop"; rc=1; }; fi
      [ ! -f "$S/data/postmaster.pid" ] || { echo "FAIL postmaster still present; keeping $S"; exit 1; }
      rm -rf "$S" || rc=1; [ ! -e "$S" ] || { echo "FAIL $S not removed"; rc=1; }
      echo "SERVED_CLEANUP rc=$rc"; exit $rc; }
    trap cleanup EXIT
    "$PB"/initdb -D $S/data -A trust -U cvdtest >/dev/null
    echo "CREATE DATABASE cvd_retirement_test;" | "$PB"/postgres --single -D $S/data postgres >/dev/null
    "$PB"/pg_ctl -D $S/data -o "-k $S/sock -c listen_addresses=''" -l $S/log -w start >/dev/null
    # Serve the ASSEMBLED tree (whitelist + assets.json), never source static/.
    sh scripts/assemble-static.sh static static/wasm-host/cvd_browser_host.wasm static/wasm-host/cvd_browser_coordinator.wasm $S/static
    CVD_RETIREMENT_TEST_URL="postgres:///cvd_retirement_test?host=$S/sock&user=cvdtest" CVD_SERVED_CHAIN_DIR=$M CVD_SERVED_STATIC_DIR=$S/static \
      owned_spawn timeout 900 cargo test --offline --locked -p cvd-server postgres_served_app_chain -- --ignored --nocapture > "$OUT/served-server.log" 2>&1
    SP=$OWNED_PID
    CVD_SERVED_CHAIN_DIR=$M CVD_SERVED_STATIC_DIR=$S/static CVD_PUBLIC_WASM_DIR=$W timeout 600 node tests/served-chain-browser.mjs ) > "$OUT/served-chain.log" 2>&1; record served $?
fi
echo "overall=$FAILED" >> "$OUT/exit-codes.txt"
cat "$OUT/exit-codes.txt"
exit $FAILED
