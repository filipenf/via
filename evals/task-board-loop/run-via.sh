#!/usr/bin/env bash
# Live via-session runner for the task-board orchestration eval.
# Requires VIA_SESSION and an ACP-capable spawn mapping (same as e2e-agents).
#
# Default delivery target is a freshly spawned ACP `orchestrator` so the prompt
# auto-runs. Set EVAL_SEND_TO=agent to mailbox the primary PTY pane instead.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
TIMEOUT_SECONDS="${VIA_E2E_TIMEOUT_SECONDS:-900}"
POLL_SECONDS="${VIA_E2E_POLL_SECONDS:-15}"
RUN_ID="${VIA_E2E_RUN_ID:-$(date +%Y%m%d%H%M%S)-$$}"
BOARD_ID="eval-${RUN_ID}"
EVAL_SEND_TO="${EVAL_SEND_TO:-orchestrator}"
PROMPT_FILE="$ROOT/PROMPT.md"
GRADE="$ROOT/grade.sh"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '==> %s\n' "$*"
}

cleanup() {
  via agent terminate --id coder >/dev/null 2>&1 || true
  via agent terminate --id reviewer >/dev/null 2>&1 || true
  if [[ "$EVAL_SEND_TO" == "orchestrator" ]]; then
    via agent terminate --id orchestrator >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

command -v via >/dev/null 2>&1 || fail "via is not on PATH"
via session get >/dev/null || fail "VIA_SESSION is not set or the via session is not live"
[[ -f "$PROMPT_FILE" ]] || fail "missing $PROMPT_FILE"
[[ -x "$GRADE" ]] || chmod +x "$GRADE"

log "run id: $RUN_ID"
log "expected board: $BOARD_ID"
log "send target: $EVAL_SEND_TO"

# Ensure fixture starts red so a prior local fix cannot falsely pass.
if "$ROOT/fixture/verify.sh" >/dev/null 2>&1; then
  log "warning: fixture already green before the run; reset bisect/flatten/gcd.lua if this is unintentional"
fi
rm -f "$ROOT/fixture/REVIEW.md"
PROMPT="$(sed "s/{{RUN_ID}}/${RUN_ID}/g" "$PROMPT_FILE")"

if [[ "$EVAL_SEND_TO" == "orchestrator" ]]; then
  via agent terminate --id orchestrator >/dev/null 2>&1 || true
  sleep 1
  log "spawning orchestrator (ACP delivery target)"
  via agent spawn --id orchestrator
  registry="$(via agent list --json)"
  printf '%s\n' "$registry" | grep -Fq '"id": "orchestrator"' || fail "orchestrator was not registered"
  printf '%s\n' "$registry" | grep -Fq '"mode": "acp"' || fail "orchestrator did not register as ACP"
fi

log "delivering eval prompt to $EVAL_SEND_TO"
via agent send --to "$EVAL_SEND_TO" -m "$PROMPT"

export EVAL_BOARD_ID="$BOARD_ID"
export EVAL_REQUIRE_AGENTS=1

deadline=$((SECONDS + TIMEOUT_SECONDS))
while (( SECONDS < deadline )); do
  if EVAL_BOARD_ID="$BOARD_ID" EVAL_REQUIRE_AGENTS=1 "$GRADE"; then
    log "eval passed for board $BOARD_ID"
    # Leave the board for inspection; still terminate helpers on exit.
    exit 0
  fi
  remaining=$((deadline - SECONDS))
  log "not green yet; retrying in ${POLL_SECONDS}s (${remaining}s left)"
  sleep "$POLL_SECONDS"
done

fail "timed out after ${TIMEOUT_SECONDS}s waiting for grade.sh to pass (board=$BOARD_ID)"
