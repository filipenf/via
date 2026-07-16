#!/usr/bin/env bash
# Grade the task-board orchestration eval against via task / agent JSON + fixture.
# See RUBRIC.md for criteria. Exit 0 only when every required check passes.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
FIXTURE="$ROOT/fixture"
REVIEW_MD="$FIXTURE/REVIEW.md"
EVAL_BOARD_ID="${EVAL_BOARD_ID:-}"
EVAL_REQUIRE_AGENTS="${EVAL_REQUIRE_AGENTS:-0}"
EVAL_JSON="${EVAL_JSON:-0}"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
declare -a RESULTS=()

log() {
  # Human-facing lines go to stderr so EVAL_JSON=1 can emit clean JSON on stdout.
  printf '==> %s\n' "$*" >&2
}

record() {
  local status="$1"
  local id="$2"
  local detail="$3"
  RESULTS+=("$status|$id|$detail")
  case "$status" in
    PASS) PASS_COUNT=$((PASS_COUNT + 1)) ;;
    FAIL) FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
    SKIP) SKIP_COUNT=$((SKIP_COUNT + 1)) ;;
  esac
  printf '%-4s  %-14s  %s\n' "$status" "$id" "$detail" >&2
}

fail_hard() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

# True when REVIEW.md shows a real loop: ≥2 rounds, or Changes requested then Approved.
review_loop_ok() {
  local file="$1"
  [[ -f "$file" ]] || return 1

  local rounds
  rounds="$(grep -cE '^## Round[[:space:]]+[0-9]+' "$file" || true)"
  if (( rounds >= 2 )); then
    return 0
  fi

  # Ordered evidence: a Changes-requested verdict appears before a later Approved.
  if awk '
    BEGIN { seen_reject = 0 }
    /^###[[:space:]]+Verdict[[:space:]]*$/ { next_verdict = 1; next }
    next_verdict && /Changes requested/ { seen_reject = 1; next_verdict = 0; next }
    next_verdict && /Approved/ {
      if (seen_reject) { found = 1; exit }
      next_verdict = 0
      next
    }
    next_verdict { next_verdict = 0 }
    END { exit found ? 0 : 1 }
  ' "$file"; then
    return 0
  fi

  return 1
}

command -v via >/dev/null 2>&1 || fail_hard "via is not on PATH"
command -v jq >/dev/null 2>&1 || fail_hard "jq is required for grading"
command -v nvim >/dev/null 2>&1 || fail_hard "nvim is required for fixture verify"

# Prefer grading against the workspace that owns the fixture (repo root), so
# portable runs work even when cwd is elsewhere.
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"
cd "$REPO_ROOT"

if [[ -n "$EVAL_BOARD_ID" ]]; then
  log "switching to board $EVAL_BOARD_ID"
  via task board use "$EVAL_BOARD_ID" >/dev/null
fi

TASK_JSON="$(via task list --json)"
BOARD="$(printf '%s' "$TASK_JSON" | jq -r '.board // empty')"
TASK_COUNT="$(printf '%s' "$TASK_JSON" | jq '.tasks | length')"

# --- board ---
if [[ -n "$EVAL_BOARD_ID" ]]; then
  if [[ "$BOARD" == "$EVAL_BOARD_ID" ]]; then
    record PASS board "active board is $BOARD"
  else
    record FAIL board "expected board=$EVAL_BOARD_ID got=${BOARD:-<empty>}"
  fi
elif [[ "$BOARD" == eval-* ]]; then
  record PASS board "active board is $BOARD"
else
  record FAIL board "active board must be eval-* (got=${BOARD:-<empty>}); set EVAL_BOARD_ID"
fi

# --- task_count ---
if (( TASK_COUNT >= 2 )); then
  record PASS task_count "$TASK_COUNT tasks"
else
  record FAIL task_count "need >= 2 tasks, found $TASK_COUNT"
fi

# --- bodies ---
BODY_OK="$(printf '%s' "$TASK_JSON" | jq '
  [.tasks[]
    | select(
        (.body // "") != ""
        and ((.body | length) >= 40)
        and ((.body | test("goal|acceptance"; "i")))
      )
  ] | length
')"
if (( BODY_OK >= 2 )); then
  record PASS bodies "$BODY_OK tasks with substantial Goal/Acceptance bodies"
else
  record FAIL bodies "need >= 2 substantial bodies mentioning Goal or Acceptance (found $BODY_OK)"
fi

# --- dependency ---
DEP_OK="$(printf '%s' "$TASK_JSON" | jq '
  (
    [.tasks[] | select((.blocked_by // []) | length > 0)] | length
  ) as $edges
  | (
    [.tasks[] | select((.body // "") | test("via:[A-Za-z0-9._-]+"))] | length
  ) as $refs
  | {edges: $edges, refs: $refs, ok: (($edges + $refs) > 0)}
')"
if [[ "$(printf '%s' "$DEP_OK" | jq -r '.ok')" == "true" ]]; then
  record PASS dependency "$(printf '%s' "$DEP_OK" | jq -r '"blocked_by edges=\(.edges) via: refs=\(.refs)"')"
else
  record FAIL dependency "need blocked_by edge or via:<id> in a body"
fi

# --- assignee ---
ASSIGNEE_OK="$(printf '%s' "$TASK_JSON" | jq '
  [.tasks[]
    | select(
        (.assignee // "") != ""
        and (.assignee // "") != "human"
      )
  ] | length
')"
if (( ASSIGNEE_OK >= 1 )); then
  record PASS assignee "$ASSIGNEE_OK task(s) assigned to a non-human agent"
else
  record FAIL assignee "need >= 1 task assigned to a spawned helper (not human)"
fi

# --- lifecycle ---
LIFE_OK="$(printf '%s' "$TASK_JSON" | jq '
  [.tasks[] | select(.status == "done" or .status == "review")] | length
')"
if (( LIFE_OK >= 1 )); then
  record PASS lifecycle "$LIFE_OK task(s) in review or done"
else
  record FAIL lifecycle "need >= 1 task in review or done"
fi

# --- agents: both coder and reviewer when session / EVAL_REQUIRE_AGENTS ---
HAVE_SESSION=0
if [[ -n "${VIA_SESSION:-}" ]] && via session get >/dev/null 2>&1; then
  HAVE_SESSION=1
fi

if (( HAVE_SESSION == 1 )) || [[ "$EVAL_REQUIRE_AGENTS" == "1" ]]; then
  if AGENT_JSON="$(via agent list --json 2>/dev/null)"; then
    HAS_CODER="$(printf '%s' "$AGENT_JSON" | jq '[.[] | select(.id == "coder")] | length')"
    HAS_REVIEWER="$(printf '%s' "$AGENT_JSON" | jq '[.[] | select(.id == "reviewer")] | length')"
    if (( HAS_CODER >= 1 && HAS_REVIEWER >= 1 )); then
      record PASS agents "coder and reviewer both registered"
    else
      record FAIL agents "need both coder and reviewer (coder=$HAS_CODER reviewer=$HAS_REVIEWER)"
    fi
  else
    record FAIL agents "via agent list failed (session required for this check)"
  fi
else
  record SKIP agents "VIA_SESSION unset; spawn check skipped (set EVAL_REQUIRE_AGENTS=1 to require)"
fi

# --- review_artifact ---
if [[ ! -f "$REVIEW_MD" ]]; then
  record FAIL review_artifact "missing $REVIEW_MD"
elif ! grep -qE '^## Round[[:space:]]+[0-9]+' "$REVIEW_MD"; then
  record FAIL review_artifact "REVIEW.md needs at least one '## Round N' section"
elif review_loop_ok "$REVIEW_MD"; then
  rounds="$(grep -cE '^## Round[[:space:]]+[0-9]+' "$REVIEW_MD" || true)"
  record PASS review_artifact "REVIEW.md loop ok (rounds=$rounds)"
else
  record FAIL review_artifact "need ≥2 rounds or Changes requested then Approved"
fi

# --- fixture ---
if "$FIXTURE/verify.sh" >/tmp/via-eval-verify.out 2>&1; then
  record PASS fixture "verify.sh exited 0"
else
  record FAIL fixture "verify.sh failed (see /tmp/via-eval-verify.out)"
fi

REQUIRED_FAIL=0
for row in "${RESULTS[@]}"; do
  status="${row%%|*}"
  if [[ "$status" == "FAIL" ]]; then
    REQUIRED_FAIL=1
  fi
done

TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
log "score: pass=$PASS_COUNT fail=$FAIL_COUNT skip=$SKIP_COUNT total=$TOTAL"

if [[ "$EVAL_JSON" == "1" ]]; then
  jq -n \
    --arg board "$BOARD" \
    --argjson pass "$PASS_COUNT" \
    --argjson fail "$FAIL_COUNT" \
    --argjson skip "$SKIP_COUNT" \
    --argjson ok "$([[ $REQUIRED_FAIL -eq 0 ]] && echo true || echo false)" \
    --argjson results "$(printf '%s\n' "${RESULTS[@]}" | jq -R 'split("|") | {status: .[0], id: .[1], detail: .[2]}' | jq -s .)" \
    '{ok: $ok, board: $board, pass: $pass, fail: $fail, skip: $skip, results: $results}'
fi

if (( REQUIRED_FAIL != 0 )); then
  log "OVERALL FAIL"
  exit 1
fi

log "OVERALL PASS"
exit 0
