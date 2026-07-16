# Rubric — task-board orchestration eval

Machine-checkable criteria. `grade.sh` implements these checks; this file is the
human-readable mirror.

Required unless marked **optional**.

| Id | Check | Pass when |
| --- | --- | --- |
| `board` | Board isolation | Active board id equals `EVAL_BOARD_ID`, or matches `eval-*` when unset |
| `task_count` | Task count | `via task list --json` has ≥ 2 tasks |
| `bodies` | Task bodies | ≥ 2 tasks have a non-empty `body` of length ≥ 40 that mentions Goal **or** Acceptance (case-insensitive) |
| `dependency` | Dependency wiring | ≥ 1 task has a non-empty `blocked_by`, **or** ≥ 1 body contains `via:<id>` |
| `assignee` | Delegation | ≥ 1 task has `assignee` set to something other than `human` / empty |
| `lifecycle` | Progress | ≥ 1 task has status `done` or `review` |
| `agents` | Spawned helpers | **Optional** when `VIA_SESSION` is unset and `EVAL_REQUIRE_AGENTS` is not `1`. When required: `via agent list --json` includes **both** `coder` **and** `reviewer` |
| `review_artifact` | Review loop evidence | `fixture/REVIEW.md` exists; has ≥1 `## Round N` section; shows either ≥2 rounds **or** a `Changes requested` verdict followed later by an `Approved` verdict |
| `fixture` | Code green | `fixture/verify.sh` exits 0 (PASS_TO_PASS + FAIL_TO_PASS) |

## Scoring

- Each required check is PASS or FAIL.
- Overall PASS only if every required check passes.
- Optional checks that are skipped are reported as `SKIP`, not FAIL.

## Non-goals

- No LLM-as-judge of review prose quality beyond the REVIEW.md template shape.
- Does not require terminating spawned agents.
- Does not require the Neovim `:ViaTasks` UI.
