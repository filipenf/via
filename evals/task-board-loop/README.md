# Task-board orchestration eval

Repeatable eval of via's **task board + multi-agent review loop** (the bundled
`via-agents` skill). An agent gets one fixed prompt, must create tasks on a
dedicated board, spawn **coder and reviewer**, repair a small multi-file Lua
fixture (QuixBugs-class defects) through a reject→fix→approve loop, and leave
`./verify.sh` green. Scoring is deterministic (`grade.sh`) — no LLM judge.

| Path | Role |
| --- | --- |
| [`PROMPT.md`](PROMPT.md) | Fixed agent instructions (`{{RUN_ID}}` placeholder) |
| [`RUBRIC.md`](RUBRIC.md) | Human-readable criteria |
| [`grade.sh`](grade.sh) | Automated grader |
| [`run-via.sh`](run-via.sh) | Live `VIA_SESSION` runner |
| [`fixture/`](fixture/) | Almost-working Lua modules (`bisect` / `flatten` / `gcd`) |
| [`reference/SOLUTION.md`](reference/SOLUTION.md) | Human-only reference (keep out of agent context) |

Not CI-gated (same policy as `mise run e2e-agents`).

## Prerequisites

- `via` on `PATH` (built/installed)
- `jq`, `nvim` (≥ 0.9, for `nvim -l` fixture tests — same as `mise run test-nvim`)
- For the full loop: a live via session with ACP spawn enabled (e.g.
  `via --agent opencode`), and skills installed (`via plugin install`)

## Portable pack (any harness / model)

1. Start via with cwd at the **repository root** (so `via task` resolves the
   same workspace the grader uses), ACP-capable agent, skills installed.
2. Substitute a run id into the prompt:

   ```sh
   RUN_ID=$(date +%Y%m%d%H%M%S)
   sed "s/{{RUN_ID}}/${RUN_ID}/g" evals/task-board-loop/PROMPT.md
   ```

3. Paste / feed that text into Claude, Cursor, OpenCode, etc. (primary pane or
   your harness's user message).
4. When the agent stops, grade:

   ```sh
   EVAL_BOARD_ID=eval-$RUN_ID mise run eval-task-board-grade
   # or: EVAL_BOARD_ID=eval-$RUN_ID evals/task-board-loop/grade.sh
   ```

Only the prompt injection differs across harnesses; the board, fixture, and
grader stay the same.

## Live via runner

From a via-launched pane (so `VIA_SESSION` is set):

```sh
mise run eval-task-board-via
# or: evals/task-board-loop/run-via.sh
```

By default the runner spawns an ACP `orchestrator`, delivers `PROMPT.md` there
(auto turn), polls `grade.sh` for up to `VIA_E2E_TIMEOUT_SECONDS` (default 900),
then terminates spawned helpers. Override delivery with `EVAL_SEND_TO=agent`
(mailbox-only for the primary PTY pane).

Useful env vars:

| Variable | Default | Meaning |
| --- | --- | --- |
| `VIA_E2E_RUN_ID` | timestamp-pid | Becomes board id `eval-$RUN_ID` |
| `VIA_E2E_TIMEOUT_SECONDS` | `900` | Poll budget |
| `VIA_E2E_POLL_SECONDS` | `15` | Sleep between grade attempts |
| `EVAL_SEND_TO` | `orchestrator` | Prompt recipient |
| `EVAL_BOARD_ID` | (set by runner) | Board the grader must see |
| `EVAL_REQUIRE_AGENTS` | `1` in runner | Require both coder and reviewer |
| `EVAL_JSON` | `0` | Print JSON summary from `grade.sh` |

## What is scored

See [`RUBRIC.md`](RUBRIC.md). Required: eval board, ≥2 tasks with rich bodies,
dependency wiring, non-human assignee, review/done lifecycle, green
`fixture/verify.sh`, durable `fixture/REVIEW.md` showing a real review loop
(≥2 rounds **or** Changes requested then Approved). When a live session is
present (or `EVAL_REQUIRE_AGENTS=1`), both `coder` and `reviewer` must be
registered.

### REVIEW.md template

```markdown
# Review

## Round 1
### Findings
- …
### Verdict
Changes requested

## Round 2
### Findings
- …
### Verdict
Approved
```

## Resetting the fixture

If a previous run left the modules green or left a `REVIEW.md` behind:

```sh
git checkout -- \
  evals/task-board-loop/fixture/bisect.lua \
  evals/task-board-loop/fixture/flatten.lua \
  evals/task-board-loop/fixture/gcd.lua
rm -f evals/task-board-loop/fixture/REVIEW.md
```
