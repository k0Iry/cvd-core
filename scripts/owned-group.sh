# Test-infrastructure helper (sourced). Positively owned process groups.
#   owned_spawn <cmd...>     run cmd in the background as the leader of a NEW
#                            process group (setsid); sets OWNED_PID. Fails if
#                            the child is not its own group leader, so cleanup
#                            can never target the caller's group.
#   owned_reap <pid> [grace_s]  TERM the whole group, bounded wait, KILL the
#                            group, reap the leader and verify no member of the
#                            group survives. Returns nonzero if the group had to
#                            be force-killed or anything survives; never global.
owned_spawn() {
  setsid "$@" &
  OWNED_PID=$!
  local pg
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pg=$(ps -o pgid= -p "$OWNED_PID" 2>/dev/null | tr -d ' ')
    [ "$pg" = "$OWNED_PID" ] && break; sleep 0.05
  done
  if [ "$pg" != "$OWNED_PID" ] || [ "$pg" = "$(ps -o pgid= -p $$ | tr -d ' ')" ]; then
    echo "FAIL owned_spawn: pid $OWNED_PID not its own group leader (pgid=$pg)"; kill -KILL "$OWNED_PID" 2>/dev/null; return 1
  fi
}
owned_alive() { pgrep -g "$1" >/dev/null 2>&1; }
owned_reap() {
  local g=$1 grace=${2:-5} forced=0 st=0
  [ -n "$g" ] && [ "$g" -gt 1 ] && [ "$g" != "$(ps -o pgid= -p $$ | tr -d ' ')" ] || { echo "FAIL owned_reap: refusing group '$g'"; return 1; }
  if owned_alive "$g"; then
    kill -TERM -- "-$g" 2>/dev/null
    for _ in $(seq 1 $((grace * 10))); do owned_alive "$g" || break; sleep 0.1; done
    if owned_alive "$g"; then forced=1; echo "owned_reap: group $g ignored TERM; KILL"; kill -KILL -- "-$g" 2>/dev/null
      for _ in $(seq 1 50); do owned_alive "$g" || break; sleep 0.1; done
    fi
  fi
  wait "$g" 2>/dev/null; st=$?
  if owned_alive "$g" || [ -n "${OWNED_GROUP_INJECT_SURVIVOR:-}" ]; then echo "FAIL owned_reap: group $g survivors: $(pgrep -g "$g" | tr '\n' ' ')"; return 1; fi
  OWNED_STATUS=$st
  [ "$forced" -eq 0 ] || return 2
  return 0
}
