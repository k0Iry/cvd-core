#!/usr/bin/env bash
# cvd-wasm v1 foundation gate (lane A). One deterministic command:
#   scripts/wasm-foundation-gate.sh [OUT_DIR]
# builds bounded wasm32 modules, generates owned fixtures + native oracles,
# runs the full native workspace tests and the real-Chromium Worker smoke.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/out-guard.sh"
# Validate canonical OUT against the source tree BEFORE the rm -rf of OUT/fixtures.
OUT=$(out_guard "$ROOT" "${1:-${CVD_GATE_OUT:-$ROOT/target/wasm-gate}}")
T="${CVD_TARGET_DIR:-$OUT/target}"
W="$T/wasm32-unknown-unknown/wasm-release"
F="$OUT/fixtures"
cd "$ROOT"
rm -rf "$F" && mkdir -p "$F"
echo "== build bounded wasm32 modules (.cargo/config.toml: --max-memory=512MiB)"
cargo build --locked --target-dir "$T" --profile wasm-release --target wasm32-unknown-unknown -p sample-indicator -p stateful-indicator -p cvd-browser-host
echo "== hostile fixture: low 4 MiB declared max for the memory-grow control"
# RUSTFLAGS replaces (not appends to) the config rustflags for this one package build.
RUSTFLAGS="-C link-arg=--max-memory=4194304" cargo build --locked --target-dir "$T" --profile wasm-release --target wasm32-unknown-unknown -p hostile-wasm-plugin
echo "== owned fixtures + native C-ABI oracles"
cargo run --locked -q --target-dir "$T" -p cvd-browser-host --example native_oracle -- --fixtures "$F"
echo "== native workspace tests (incl. original native compatibility tests)"
CVD_WASM_DIR="$W" cargo test --locked --target-dir "$T" --workspace
echo "== real Chromium Worker smoke"
CVD_WASM_DIR="$W" CVD_FIXTURE_DIR="$F" node tests/wasm-host-browser.mjs
( cd "$W" && sha256sum sample_indicator.wasm stateful_indicator.wasm hostile_wasm_plugin.wasm cvd_browser_host.wasm ) | tee "$OUT/artifact-hashes.txt"
( cd "$F" && find . -type f | sort | xargs sha256sum ) > "$OUT/fixture-hashes.txt"
echo "GATE PASS"
