#!/usr/bin/env bash
# Verify the trusted runtime assets as SERVED over HTTP (not on disk).
# Usage: verify-served-assets.sh <base-url>   e.g. http://127.0.0.1:18080
# Requires: curl, sha256sum, python3. Exit 0 only if:
#  - /wasm-host/assets.json is valid cvd-trusted-runtime-assets-v1 listing
#    exactly cvd_browser_host.wasm + cvd_browser_coordinator.wasm (no extras,
#    no duplicates, sha256 = 64 lowercase hex, bytes = positive integer);
#  - each served wasm has wasm magic, the exact byte length and sha256;
#  - host-bridge.js and plugin-worker.js return 200 with a JavaScript MIME.
set -uo pipefail
BASE=${1:?usage: verify-served-assets.sh <base-url>}
BASE=${BASE%/}
T=$(mktemp -d "${TMPDIR:-/tmp}/cvd-served-XXXXXX"); trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL served-assets: $*" >&2; exit 1; }
get() { curl -sS --max-time 20 -o "$2" -w '%{http_code} %{content_type}' "$BASE/wasm-host/$1"; }

r=$(get assets.json "$T/assets.json") || fail "fetch assets.json"
[ "${r%% *}" = 200 ] || fail "assets.json http ${r%% *}"
expected=$(python3 - "$T/assets.json" <<'PY'
import json, re, sys
try:
    m = json.load(open(sys.argv[1]))
except Exception as e:
    sys.exit(f"assets.json invalid JSON: {e}")
want = ["cvd_browser_host.wasm", "cvd_browser_coordinator.wasm"]
if not isinstance(m, dict) or set(m) != {"format", "assets"}: sys.exit("assets.json keys must be exactly format+assets")
if m["format"] != "cvd-trusted-runtime-assets-v1": sys.exit(f"bad format {m['format']!r}")
a = m["assets"]
if not isinstance(a, list) or sorted(x.get("file") for x in a if isinstance(x, dict)) != sorted(want) or len(a) != 2:
    sys.exit(f"assets must be exactly {want}")
for x in a:
    if set(x) != {"file", "sha256", "bytes"}: sys.exit(f"bad entry keys {sorted(x)}")
    if not isinstance(x["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", x["sha256"]): sys.exit(f"bad sha256 for {x['file']}")
    if type(x["bytes"]) is not int or x["bytes"] <= 0: sys.exit(f"bad bytes for {x['file']}")
    print(x["file"], x["sha256"], x["bytes"])
PY
) || fail "manifest rejected"

while read -r f sha bytes; do
  r=$(get "$f" "$T/$f") || fail "fetch $f"
  [ "${r%% *}" = 200 ] || fail "$f http ${r%% *}"
  head -c 4 "$T/$f" | od -An -tx1 | grep -q '00 61 73 6d' || fail "$f not wasm"
  n=$(wc -c < "$T/$f" | tr -d ' ')
  [ "$n" = "$bytes" ] || fail "$f bytes $n != manifest $bytes"
  h=$(sha256sum "$T/$f" | cut -d' ' -f1)
  [ "$h" = "$sha" ] || fail "$f sha256 $h != manifest $sha"
  echo "ok $f $bytes $sha"
done <<< "$expected"

for f in host-bridge.js plugin-worker.js; do
  r=$(get "$f" "$T/$f") || fail "fetch $f"
  code=${r%% *} ct=${r#* }
  [ "$code" = 200 ] || fail "$f http $code"
  [ -s "$T/$f" ] || fail "$f empty"
  mime=$(printf '%s' "${ct%%;*}" | tr 'A-Z' 'a-z' | tr -d ' ')
  case "$mime" in text/javascript|application/javascript) ;; *) fail "$f Content-Type '$ct' is not JavaScript";; esac
  echo "ok $f $ct"
done
echo "PASS served-assets $BASE"
