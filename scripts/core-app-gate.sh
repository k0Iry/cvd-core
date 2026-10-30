#!/usr/bin/env bash
# Self-contained CLEAN-CORE application/interaction gate.
#   scripts/core-app-gate.sh <OUT_DIR> [reps]
# Builds, from THIS checkout only (no sibling repos, audit paths, Release
# assets or generated static wasm): the trusted host + coordinator wasm, the
# TEST-ONLY `cvd`/`flow` alias fixtures (tests/fixtures/wasm/interaction-plugin),
# assembles the shipped static tree (whitelist + assets.json) and runs the real
# app/Worker/LWC interaction suite (tests/browser-interaction.mjs) against it.
# The fixtures are imported through the app's file input only; they never enter
# the assembled static tree, Docker context or any Release.
# Exit 0 only when every build step ran, the assembled tree carries exactly the
# two trusted wasm, and every scenario in the inventory completed.
# CORE_APP_GATE_INJECT=<build|assemble|interaction|inventory|coverage|lwcrender> forces that step
# to fail (control: omitted required work can never return zero).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); REPS=${2:-1}
. "$ROOT/scripts/out-guard.sh"
# Validate canonical OUT against the source tree BEFORE any mkdir/rm below.
OUT=$(out_guard "$ROOT" "${1:?OUT_DIR}"); mkdir -p "$OUT"; cd "$ROOT"
# Refuse ambient artifact overrides: every input identity is explicit below.
for v in $(env | sed -n 's/^\(CVD_[A-Z_]*\)=.*/\1/p'); do
  case $v in CVD_CHROMIUM|CVD_HARD_TIMEOUT_MIN) ;; *) unset "$v";; esac
done
unset CARGO_TARGET_DIR
inject() { if [ "${CORE_APP_GATE_INJECT:-}" = "$1" ]; then echo "injected failure: $1" >&2; exit 1; fi; }
T=$OUT/target; FX=$OUT/fixtures; ST=$OUT/static; RES=$OUT/interaction; COV=$OUT/coverage
rm -rf "$FX" "$ST" "$RES" "$COV"; mkdir -p "$FX"
echo "== build trusted host + coordinator wasm from source"
inject build
echo "== acquire locked deps of root + standalone workspaces (network; honours CARGO_NET_OFFLINE)"
for d in . crates/browser-coordinator tests/fixtures/wasm/interaction-plugin; do ( cd "$d" && cargo fetch --locked ); done
cargo build --offline --locked --target-dir "$T/host" --profile wasm-release --target wasm32-unknown-unknown -p cvd-browser-host
( cd crates/browser-coordinator && cargo build --offline --locked --release --target wasm32-unknown-unknown --target-dir "$T/coordinator" )
HOST=$T/host/wasm32-unknown-unknown/wasm-release/cvd_browser_host.wasm
COORD=$T/coordinator/wasm32-unknown-unknown/release/cvd_browser_coordinator.wasm
echo "== build TEST-ONLY cvd/flow alias fixtures + ctxdemo warm-up coverage fixture"
for f in cvd flow ctxdemo; do
  ( cd tests/fixtures/wasm/interaction-plugin && cargo build --offline --locked --release --target wasm32-unknown-unknown --features "$f" --target-dir "$T/fixture-$f" )
  cp "$T/fixture-$f/wasm32-unknown-unknown/release/interaction_fixture.wasm" "$FX/cvd_indicator_$f.wasm"
done
echo "== assemble shipped static tree"
inject assemble
sh scripts/assemble-static.sh static "$HOST" "$COORD" "$ST"
got=$(cd "$ST" && find . -name '*.wasm' | sort | tr '\n' ' ')
[ "$got" = "./wasm-host/cvd_browser_coordinator.wasm ./wasm-host/cvd_browser_host.wasm " ] || { echo "FAIL assembled wasm set: $got"; exit 1; }
test -s "$ST/wasm-host/assets.json"
sha256sum "$HOST" "$COORD" "$FX"/*.wasm "$ST/js/app.js" | tee "$OUT/artifact-hashes.txt"
echo "== real Chromium app interaction (clean-core fixtures)"
inject interaction
CVD_STATIC_ROOT=$ST CVD_LOCAL_WASM=$FX/cvd_indicator_cvd.wasm,$FX/cvd_indicator_flow.wasm \
  CVD_INTERACTION_OUT=$RES CVD_REPS=$REPS timeout 3000 node tests/browser-interaction.mjs
echo "== scenario inventory"
inject inventory
# Pinned scenario inventory (12); a skipped or renamed scenario fails.
want=cached_drag,cached_zoom,crossing_buffer,crossing_newer,hidden_resize_resume,history_prepend_delayed,indicator_menu,long_drag,pane_contract,realtime_while_history,source_reset_new_grid,wide_zoom_return
node -e "
const r=JSON.parse(require('fs').readFileSync('$RES/interaction-results.json','utf8'));
const want='$want'.split(',');const got=Object.keys(r.scenarios).sort();
if(!want.length||want.join()!==got.join()){console.error('FAIL inventory want',want,'got',got);process.exit(1)}
const bad=got.filter(k=>{const s=r.scenarios[k];return s.all_ok!==true});
if(bad.length||(r.errors&&r.errors.length)){console.error('FAIL scenarios',bad,r.errors);process.exit(1)}
console.log('INVENTORY',got.length,'scenarios:',got.join(','))"
echo "== warm-up coverage (cvd+flow+ctxdemo; 30m drag + same-fix TF roundtrip)"
inject coverage
# Separate run so the 12-scenario inventory keeps its 2-plugin expectations.
# Pinned coverage inventory (2); omission/rename/failure is nonzero.
covwant=tf_roundtrip_pending,warmup_exposed_drag
CVD_STATIC_ROOT=$ST CVD_LOCAL_WASM=$FX/cvd_indicator_cvd.wasm,$FX/cvd_indicator_flow.wasm,$FX/cvd_indicator_ctxdemo.wasm \
  CVD_SCENARIOS=$covwant CVD_INTERACTION_OUT=$COV CVD_REPS=$REPS timeout 1200 node tests/browser-interaction.mjs
node -e "
const r=JSON.parse(require('fs').readFileSync('$COV/interaction-results.json','utf8'));
const want='$covwant'.split(',');const got=Object.keys(r.scenarios).sort();
if(want.join()!==got.join()){console.error('FAIL coverage inventory want',want,'got',got);process.exit(1)}
const bad=got.filter(k=>r.scenarios[k].all_ok!==true);
if(bad.length||(r.errors&&r.errors.length)||!r.cleanup||r.cleanup.ok!==true){console.error('FAIL coverage',bad,r.errors,r.cleanup);process.exit(1)}
console.log('COVERAGE INVENTORY',got.length,'scenarios:',got.join(','))"
echo "== real-LWC plugin renderer regression (assembled static tree)"
inject lwcrender
# Exactly 4 renderer kinds must PASS; nonzero exit propagates via set -e.
CVD_STATIC_ROOT=$ST timeout 120 node tests/lwc-tail-update-browser.mjs | tee "$OUT/lwc-render-browser.log"
[ "$(grep -c '^PASS ' "$OUT/lwc-render-browser.log")" = 4 ] && grep -qx '4/4 renderer kinds GREEN' "$OUT/lwc-render-browser.log" || { echo "FAIL lwcrender inventory"; exit 1; }
echo "CORE APP GATE PASS"
