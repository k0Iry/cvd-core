#!/usr/bin/env bash
# Output-path safety control for core-app-gate.sh / wasm-foundation-gate.sh.
# Runs ONLY against an owned dummy checkout with sentinel files (never the
# real repo): copies the two gate scripts + out-guard.sh into SANDBOX/src,
# plants sentinels where a wrong OUT would rm -rf, and asserts every unsafe
# OUT is refused with exit 2 and all sentinels survive; a safe OUT passes the
# guard (the dummy then fails later at cargo, which is expected).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
SB=$(mktemp -d "${TMPDIR:-/tmp}/cvd-outguard-XXXXXX"); trap 'rm -rf "$SB"' EXIT
S=$SB/src; mkdir -p "$S/scripts" "$S/tests/fixtures" "$S/static" "$S/target" "$SB/ext"
cp "$REPO/scripts/core-app-gate.sh" "$REPO/scripts/wasm-foundation-gate.sh" "$REPO/scripts/out-guard.sh" "$S/scripts/"
plant() { for d in "$S/fixtures" "$S/static" "$S/interaction" "$S/tests/fixtures" "$SB/fixtures" "$SB/static"; do mkdir -p "$d"; echo s > "$d/SENTINEL"; done; }
check() { for d in "$S/fixtures" "$S/static" "$S/interaction" "$S/tests/fixtures" "$SB/fixtures" "$SB/static"; do [ -f "$d/SENTINEL" ] || { echo "SENTINEL LOST: $d"; return 1; }; done; }
ln -s "$S" "$SB/alias-root"; ln -s "$S/tests" "$SB/alias-tests"
fail=0; n=0
# CARGO=false makes any post-guard progress harmless and visible.
run() { ( cd "$SB" && PATH="$SB/nobin:$PATH" CVD_GATE_OUT= bash "$S/scripts/$1" "$2" ) >"$SB/log" 2>&1; echo $?; }
mkdir -p "$SB/nobin"; printf '#!/bin/sh\nexit 97\n' > "$SB/nobin/cargo"; chmod +x "$SB/nobin/cargo"
for gate in core-app-gate.sh wasm-foundation-gate.sh; do
  for out in "$S" "$S/" "$S/tests" "$S/static" "$SB" / "$SB/alias-root" "$SB/alias-tests" "$S/tests/../" "$S/target"; do
    plant; rc=$(run "$gate" "$out"); n=$((n+1))
    if [ "$rc" = 2 ] && check && grep -q out-guard "$SB/log"; then echo "PASS refuse $gate OUT=$out"
    else echo "FAIL refuse $gate OUT=$out rc=$rc"; sed 's/^/  | /' "$SB/log"; fail=$((fail+1)); fi
  done
  for out in "$SB/ext/out" "$S/target/gate-out"; do
    plant; rc=$(run "$gate" "$out"); n=$((n+1))
    if [ "$rc" = 97 ] && check && ! grep -q out-guard "$SB/log"; then echo "PASS allow  $gate OUT=$out (stopped at stub cargo rc=97)"
    else echo "FAIL allow  $gate OUT=$out rc=$rc"; sed 's/^/  | /' "$SB/log"; fail=$((fail+1)); fi
  done
done
echo "out-guard control: $n cases, fail=$fail"; [ $fail = 0 ]
