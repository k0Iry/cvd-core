#!/usr/bin/env bash
# Control for scripts/owned-group.sh (served-stage forced cleanup):
#  1. a TERM-ignoring GRANDCHILD under timeout->bash->bash (the
#     timeout->cargo->Rust-test shape) cannot survive owned_reap;
#  2. an unrelated process (own group) and the caller survive;
#  3. a forced reap and an (injected) surviving member return nonzero.
set -u
. "$(dirname "$0")/../scripts/owned-group.sh"
fail=0; ok() { echo "PASS $*"; }; bad() { echo "FAIL $*"; fail=1; }
D=$(mktemp -d "${TMPDIR:-/tmp}/owned-ctl.XXXXXX")
setsid sleep 300 & CTRL=$!
# RED witness of the retired shape: pkill -P (direct children) + kill leader.
timeout 900 bash -c "bash -c 'trap \"\" TERM; echo \$\$ > $D/old; while :; do sleep 1; done' & trap '' TERM; wait" & OLD=$!
for _ in $(seq 1 50); do [ -s "$D/old" ] && break; sleep 0.1; done; OGC=$(cat "$D/old")
sleep 0.2; pkill -KILL -P "$OLD"; kill -KILL "$OLD"; wait "$OLD" 2>/dev/null; sleep 0.3
kill -0 "$OGC" 2>/dev/null && ok "RED: old direct-child kill leaves grandchild $OGC alive" || bad "old shape did not reproduce"
kill -KILL "$OGC" 2>/dev/null
owned_spawn timeout 900 bash -c "bash -c 'trap \"\" TERM; echo \$\$ > $D/gc; while :; do sleep 1; done' & trap '' TERM; wait" || bad spawn
for _ in $(seq 1 50); do [ -s "$D/gc" ] && break; sleep 0.1; done
GC=$(cat "$D/gc" 2>/dev/null); [ -n "$GC" ] && kill -0 "$GC" && ok "grandchild $GC running in group $OWNED_PID" || bad "grandchild not started"
[ "$(ps -o pgid= -p "$GC" | tr -d ' ')" = "$OWNED_PID" ] && ok "grandchild in owned group" || bad "grandchild group"
owned_reap "$OWNED_PID" 1; r=$?
[ "$r" -eq 2 ] && ok "forced reap reported nonzero ($r)" || bad "forced reap rc=$r"
kill -0 "$GC" 2>/dev/null && bad "grandchild $GC survived" || ok "TERM-ignoring grandchild gone"
pgrep -g "$OWNED_PID" >/dev/null && bad "owned group members remain" || ok "owned group empty"
kill -0 "$CTRL" && ok "unrelated control $CTRL survives" || bad "control killed"
kill -0 $$ && ok "caller survives"
# Cooperative exit: rc 0 and real exit status captured.
owned_spawn bash -c 'sleep 0.5; exit 7'; sleep 1.2; owned_reap "$OWNED_PID" 1; r=$?
[ "$r" -eq 0 ] && [ "${OWNED_STATUS:-}" = 7 ] && ok "clean reap rc=0 status=7" || bad "clean reap rc=$r status=${OWNED_STATUS:-}"
# Injected survivor -> nonzero.
owned_spawn sleep 0.5; sleep 1.0; OWNED_GROUP_INJECT_SURVIVOR=1 owned_reap "$OWNED_PID" 1; r=$?
[ "$r" -ne 0 ] && ok "injected survivor rc=$r" || bad "injected survivor returned 0"
# Refuses the caller's own group / nonsense ids.
owned_reap "$(ps -o pgid= -p $$ | tr -d ' ')" 1 && bad "reaped caller group" || ok "refused caller group"
owned_reap 1 1 && bad "reaped pgid 1" || ok "refused pgid 1"
kill "$CTRL"; wait "$CTRL" 2>/dev/null; rm -rf "$D"
echo "OWNED_GROUP_CONTROL fail=$fail"; exit $fail
