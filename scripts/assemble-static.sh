#!/bin/sh
# Assemble the shipped static tree: source static/ minus any pre-existing
# .wasm, plus exactly the two trusted runtime modules built from source.
# Usage: assemble-static.sh <src-static> <host.wasm> <coordinator.wasm> <out>
# Fails if any other .wasm/.so/native artifact would ship, and writes
# wasm-host/assets.json (sha256 + byte size) as the asset/version manifest.
set -eu
SRC=$1 HOST=$2 COORD=$3 OUT=$4
test -s "$HOST" && test -s "$COORD"
# Refuse destructive output paths: OUT must be a fresh, non-root path that
# neither contains nor lies inside the source tree or the input modules.
case "$OUT" in ''|/|.|..|*/..|*/../*) echo "unsafe output path: $OUT" >&2; exit 1;; esac
canon() { d=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P) || return 1; printf '%s/%s' "$d" "$(basename "$1")"; }
mkdir -p "$(dirname "$OUT")"
o=$(canon "$OUT") s=$(canon "$SRC") h=$(canon "$HOST") c=$(canon "$COORD")
for x in "$s" "$h" "$c" "$(pwd -P)"; do
  case "$x/" in "$o"/*) echo "output $o contains input/cwd $x" >&2; exit 1;; esac
done
case "$o/" in "$s"/*) echo "output $o lies inside source $s" >&2; exit 1;; esac
[ "$o" = / ] && { echo "unsafe output /" >&2; exit 1; }
rm -rf "$OUT" && mkdir -p "$(dirname "$OUT")" && cp -R "$SRC" "$OUT"
find "$OUT" -name '*.wasm' -delete
cp "$HOST" "$OUT/wasm-host/cvd_browser_host.wasm"
cp "$COORD" "$OUT/wasm-host/cvd_browser_coordinator.wasm"
# Whitelist: only these two .wasm files and no native libraries/executables.
extra=$(find "$OUT" \( -name '*.wasm' -o -name '*.so' -o -name '*.dylib' -o -name '*.dll' -o -name 'cvd-indicator-runner' \) \
  ! -path "$OUT/wasm-host/cvd_browser_host.wasm" ! -path "$OUT/wasm-host/cvd_browser_coordinator.wasm")
if [ -n "$extra" ]; then echo "forbidden runtime asset(s): $extra" >&2; exit 1; fi
for f in cvd_browser_host.wasm cvd_browser_coordinator.wasm; do
  head -c 4 "$OUT/wasm-host/$f" | od -An -tx1 | grep -q '00 61 73 6d' || { echo "$f is not wasm" >&2; exit 1; }
done
( cd "$OUT/wasm-host"
  printf '{"format":"cvd-trusted-runtime-assets-v1","assets":['
  sep=''
  for f in cvd_browser_host.wasm cvd_browser_coordinator.wasm; do
    printf '%s{"file":"%s","sha256":"%s","bytes":%s}' "$sep" "$f" "$(sha256sum "$f" | cut -d' ' -f1)" "$(wc -c < "$f" | tr -d ' ')"
    sep=','
  done
  printf ']}\n' ) > "$OUT/wasm-host/assets.json"
cat "$OUT/wasm-host/assets.json"
