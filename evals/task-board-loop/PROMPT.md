# Task-board orchestration eval (program repair + review loop)

You are driving a **via** multi-agent session (`VIA_SESSION` is set). Use the
bundled **via-agents** skill and the `via` CLI. Do not ask the human for
confirmation — plan, delegate, review in a loop, and finish.

## Run id

`{{RUN_ID}}`

Use this exact board id: `eval-{{RUN_ID}}`.

## Problem

A small Lua library lives under `evals/task-board-loop/fixture/`, split across:

- `bisect.lua` — `find_first_in_sorted`
- `flatten.lua` — `flatten`
- `gcd.lua` — `gcd`

The helpers are **almost working**: many smoke cases already pass, but
`./verify.sh` is red. Read `fixture/README.md` and run the suite. Tests are
labeled `PASS_TO_PASS` (must stay green) and `FAIL_TO_PASS` (must become green).

**Scope:** only edit files inside `evals/task-board-loop/fixture/` (the three
modules above, plus the review artifact below). Do not change `test_algo.lua` or
`verify.sh`. Do not change the parent via codebase. Do not delete the eval board
when finished.

## Required workflow (hard review loop)

1. **Board** — create and activate:
   `via task board new --id eval-{{RUN_ID}} --title "Task-board eval {{RUN_ID}}"`
2. **Spawn both helpers** — you must spawn **`coder` and `reviewer`**:
   `via agent spawn --id coder` and `via agent spawn --id reviewer`.
3. **Plan** — create **at least two** tasks with short titles and rich `-m`
   bodies (Goal / Scope / Acceptance). Wire dependencies with `--blocked-by`
   **and** `Depends on: via:<id>` in the body. Suggested split: triage failing
   cases → fix defects (possibly one task per module) → review gate.
4. **Coder** — assign/claim implement work to `coder`. Coder repairs the
   fixture modules, runs `./verify.sh`, then moves the task to **`review`**.
   Coder must **not** mark tasks `done`.
5. **Reviewer** — assign review to `reviewer`. Reviewer re-runs `./verify.sh`,
   checks that PASS_TO_PASS cases still pass, and writes durable findings to:

   `evals/task-board-loop/fixture/REVIEW.md`

   Use this template (append new rounds; do not delete prior rounds):

   ```markdown
   # Review

   ## Round 1
   ### Findings
   - …
   ### Verdict
   Changes requested
   ```

   Verdict must be exactly `Changes requested` or `Approved` on its own line
   under `### Verdict`.
6. **Loop** — if the verdict is `Changes requested`, send findings to the coder
   (`via agent send` and/or task status back to `in_progress`), fix, and open a
   new review round (`## Round 2`, …). **A single one-shot `Approved` without a
   prior reject is not enough for this eval** — you need either ≥2 rounds, or a
   `Changes requested` round followed later by an `Approved` round.
7. **Done** — only after a final **Approved** round and green `./verify.sh` may
   tasks move to `done`. Leave the board scorable; do not terminate the eval
   board.

## Done looks like

- Active board id is `eval-{{RUN_ID}}`
- ≥ 2 tasks with substantial bodies and at least one dependency edge
- ≥ 1 task assigned to a spawned helper (not only `human`)
- ≥ 1 task in `review` or `done`
- Both `coder` and `reviewer` appear in `via agent list`
- `fixture/REVIEW.md` shows a real review loop (reject then approve, or ≥2 rounds)
- `evals/task-board-loop/fixture/verify.sh` exits 0
