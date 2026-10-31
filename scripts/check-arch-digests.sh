#!/usr/bin/env bash
# Usage: check-arch-digests.sh DIR SHA IMAGE
# Prints IMAGE@digest for amd64 then arm64. Fails unless DIR holds exactly one
# well-formed record per required arch for SHA, with distinct digests.
set -euo pipefail
dir=$1 sha=$2 image=$3
[ "$(find "$dir" -maxdepth 1 -type f | wc -l)" -eq 2 ] || { echo "expected exactly 2 digest records" >&2; exit 1; }
seen=""
for arch in amd64 arm64; do
  f="$dir/$arch"
  { [ -f "$f" ] && [ "$(wc -l < "$f")" -eq 1 ]; } || { echo "missing/invalid $arch record" >&2; exit 1; }
  extra=""
  read -r a s d extra < "$f"
  { [ "$a" = "$arch" ] && [ "$s" = "$sha" ] && [ -z "$extra" ] && [[ "$d" =~ ^sha256:[0-9a-f]{64}$ ]]; } \
    || { echo "bad $arch record" >&2; exit 1; }
  case " $seen " in *" $d "*) echo "duplicate digest" >&2; exit 1 ;; esac
  seen="$seen $d"
  printf '%s@%s\n' "$image" "$d"
done
