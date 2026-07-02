#!/usr/bin/env bash
set -u -o pipefail

TIMEOUT_SECONDS="${VIA_E2E_TIMEOUT_SECONDS:-90}"
POLL_SECONDS="${VIA_E2E_POLL_SECONDS:-2}"
RUN_ID="${VIA_E2E_RUN_ID:-$(date +%Y%m%d%H%M%S)-$$}"
MISSING_RECIPIENT_OUTPUT="$(mktemp -t via-e2e-missing-recipient.XXXXXX)"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '==> %s\n' "$*"
}

run() {
  log "$*"
  "$@"
}

cleanup() {
  via agent terminate --id reviewer >/dev/null 2>&1 || true
  via agent terminate --id orchestrator >/dev/null 2>&1 || true
  rm -f "$MISSING_RECIPIENT_OUTPUT"
}
trap cleanup EXIT

require_marker() {
  local marker="$1"
  local deadline=$((SECONDS + TIMEOUT_SECONDS))
  while (( SECONDS < deadline )); do
    if VIA_AGENT_ID=agent via agent inbox --json --peek 2>/dev/null | grep -Fq "$marker"; then
      log "observed marker: $marker"
      return 0
    fi
    sleep "$POLL_SECONDS"
  done
  fail "timed out waiting for primary agent inbox marker: $marker"
}

require_reviewer_mailbox_marker() {
  local marker="$1"
  if ! VIA_AGENT_ID=reviewer via agent inbox --json --peek 2>/dev/null | grep -Fq "$marker"; then
    fail "expected reviewer mailbox marker not found: $marker"
  fi
  log "observed reviewer mailbox marker: $marker"
}

drain_reviewer_mailbox() {
  VIA_AGENT_ID=reviewer via agent inbox --json >/dev/null 2>&1 || true
}

require_agent_absent() {
  local id="$1"
  local deadline=$((SECONDS + TIMEOUT_SECONDS))
  while (( SECONDS < deadline )); do
    if ! via agent list --json | grep -Fq "\"id\": \"$id\""; then
      log "agent absent: $id"
      return 0
    fi
    sleep "$POLL_SECONDS"
  done
  fail "$id still registered after terminate"
}

require_absent_primary_marker() {
  local marker="$1"
  if VIA_AGENT_ID=agent via agent inbox --json --peek 2>/dev/null | grep -Fq "$marker"; then
    fail "unexpected primary agent inbox marker: $marker"
  fi
  log "confirmed primary inbox does not contain marker: $marker"
}

command -v via >/dev/null 2>&1 || fail "via is not on PATH"
via session get >/dev/null || fail "VIA_SESSION is not set or the via session is not live"

log "run id: $RUN_ID"
cleanup
sleep 1

run via agent spawn --id reviewer
registry="$(via agent list --json)"
printf '%s\n' "$registry"
printf '%s\n' "$registry" | grep -Fq '"id": "reviewer"' || fail "reviewer was not registered"
printf '%s\n' "$registry" | grep -Fq '"mode": "acp"' || fail "reviewer did not register as ACP"
printf '%s\n' "$registry" | grep -Fq '"command": "opencode acp"' || log "warning: reviewer command is not opencode acp"

single_marker="reviewer ACP delivery OK $RUN_ID"
run via agent send --to reviewer -m "E2E single-agent marker $RUN_ID. Run shell commands only; do not edit files. Run this exact command: via agent send --to agent -m '$single_marker'"
require_marker "$single_marker"

pty_marker="PTY mailbox-only $RUN_ID"
run via agent send --to agent -m "$pty_marker"
require_marker "$pty_marker"

no_notify_marker="ACP no-notify mailbox-only $RUN_ID"
unexpected_no_notify_reply="unexpected ACP no-notify reply $RUN_ID"
run via agent send --to reviewer --no-notify -m "Mailbox-only marker $no_notify_marker. If this auto-runs, send: via agent send --to agent -m '$unexpected_no_notify_reply'"
sleep 5
require_reviewer_mailbox_marker "$no_notify_marker"
require_absent_primary_marker "$unexpected_no_notify_reply"
drain_reviewer_mailbox

if via agent send --to missing-e2e-agent -m "should fail $RUN_ID" >"$MISSING_RECIPIENT_OUTPUT" 2>&1; then
  fail "send to missing recipient unexpectedly succeeded"
fi
log "missing recipient failed as expected"

run via agent spawn --id orchestrator
registry="$(via agent list --json)"
printf '%s\n' "$registry"
printf '%s\n' "$registry" | grep -Fq '"id": "orchestrator"' || fail "orchestrator was not registered"
printf '%s\n' "$registry" | grep -Fq '"mode": "acp"' || fail "orchestrator did not register as ACP"

loop_marker="reviewer loop OK $RUN_ID"
run via agent send --to orchestrator -m "E2E multi-agent loop marker $RUN_ID. Run shell commands only; do not edit files. Send this exact command to reviewer now: via agent send --to reviewer -m \"Run this exact shell command and nothing else: via agent send --to agent -m '$loop_marker'\""
require_marker "$loop_marker"

run via agent terminate --id reviewer
run via agent terminate --id orchestrator
require_agent_absent reviewer
require_agent_absent orchestrator
trap - EXIT
rm -f "$MISSING_RECIPIENT_OUTPUT"


registry="$(via agent list --json)"
printf '%s\n' "$registry"

log "E2E agent orchestration smoke passed"
