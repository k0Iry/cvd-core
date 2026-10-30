# Sourced by gate scripts. out_guard ROOT OUT -> prints canonical OUT or exits 2.
# A gate output dir is wiped piecemeal (OUT/fixtures, OUT/static, ...), so it
# must never be, contain, or alias the source checkout. Rules (on canonical
# paths, symlinks resolved, BEFORE any mkdir/rm):
#   - reject "/" and any ancestor of (or equal to) ROOT;
#   - inside ROOT, allow only below ROOT/target (gitignored build output);
#   - outside ROOT anything else is allowed (isolated scratch dirs).
out_guard() {
  local root out
  root=$(realpath -e -- "$1") || { echo "out-guard: bad root $1" >&2; exit 2; }
  [ -n "${2:-}" ] || { echo "out-guard: OUT_DIR required" >&2; exit 2; }
  out=$(realpath -m -- "$2") || { echo "out-guard: cannot resolve $2" >&2; exit 2; }
  case "$out" in
    /) echo "out-guard: refusing OUT=/ " >&2; exit 2;;
  esac
  case "$root/" in
    "$out"/*) echo "out-guard: refusing OUT=$out (equal to or ancestor of source $root)" >&2; exit 2;;
  esac
  case "$out/" in
    "$root/target/"?*) ;;
    "$root"/*) echo "out-guard: refusing OUT=$out (inside source tree; use $root/target/... or an external dir)" >&2; exit 2;;
  esac
  printf '%s\n' "$out"
}
