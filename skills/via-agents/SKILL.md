---
name: via-agents
description: >-
  Coordinate with other AI agents and the via task board in a via session:
  discover running agents, spawn role-based helpers (orchestrator, reviewer,
  coder), message them, and manage workspace-scoped tasks (create, claim,
  update, review, done). Use when VIA_SESSION is set and you need another
  agent's help, want to delegate or hand off work, track multi-step work on
  the board, or move a task through claim / review / done.
---

# via agents skill

via can run several agent panes side by side. The **default** is a single
interactive PTY agent pane (`--agent opencode`, etc.) for everyday work. **ACP
orchestration is opt-in:** when you need coordinated multi-agent handoff, spawn
an ACP orchestrator plus helpers (reviewer, coder, …).

This skill covers the bus/CLI used when `VIA_SESSION` is set.

## Default vs orchestration

| Mode              | Primary pane                             | When                              |
| ----------------- | ---------------------------------------- | --------------------------------- |
| **Default**       | PTY `agent` (`opencode`, `claude`, …)    | Simple changes, interactive use   |
| **Orchestration** | PTY `agent` stays; add spawned ACP panes | Multi-agent handoff, auto prompts |

`--agent opencode` does **not** auto-upgrade to `opencode acp`. Spawned helpers
**do** resolve to ACP when the configured agent is in the known table
(`opencode`, `cursor-agent`, `agent`) or when `acp_agent` / `--acp-agent` is set
(e.g. `claude-code-acp`).

## Your identity

Each agent pane gets `VIA_AGENT_ID` and `VIA_AGENT_ROLE` in its environment.

```bash
via agent whoami        # who am I, and which session
```

The default interactive pane is id `agent`. Bus messages with no recipient
default to `orchestrator` once spawned; `via agent send` errors if the recipient
is not registered.

## Discover other agents

```bash
via agent list           # id + role of every agent in this session
via agent list --json    # machine-readable
```

## Start orchestration

Spawn an ACP orchestrator, then helpers. Built-in presets supply default roles
for `orchestrator`, `reviewer`, and `coder` when `--role` is omitted. Commands
without an explicit `--command` resolve to ACP form (e.g. `opencode` →
`opencode acp`).

Override presets in `~/.config/via/via.conf`:

```toml
[agents.reviewer]
role = "reviewer"
# command = "cursor-agent acp"  # optional
```

```bash
via agent spawn --id orchestrator
via agent spawn --id reviewer
via agent spawn --id coder
```

Spawn is unavailable when the session has no ACP mapping for the configured
agent (e.g. `claude` without `--acp-agent`).

When orchestration is done, terminate spawned panes (orchestrator included):

```bash
via agent terminate --id reviewer
via agent terminate --id orchestrator
```

The primary PTY `agent` pane cannot be terminated.

## Send a message

Messages always go to the recipient's **mailbox**. Live delivery:

- **ACP agents**: full body delivered as a prompt (automatic turn).
- **PTY agents**: mailbox only; read with `via agent inbox`.

```bash
via agent send --to orchestrator -m "Plan the refactor."
via agent send --to reviewer -m "Please review the branch."
via agent send --to agent -m "Note for the interactive pane."   # mailbox only
```

Use `--no-notify` for mailbox-only even for ACP recipients.

## Read your messages

```bash
via agent inbox          # read and clear your mailbox
via agent inbox --peek   # read without clearing
via agent inbox --json   # machine-readable
via agent inbox --wait 30  # block up to 30s for a message
```

PTY orchestration loops can use `--wait` instead of `sleep` + immediate `inbox`:
the command returns as soon as a message lands, or after the timeout with an
empty result.

## Tasks vs messages

| Need                                              | Use                                      |
| ------------------------------------------------- | ---------------------------------------- |
| Durable work that survives restart / handoff      | `via task create` / claim / update / done |
| Multi-step plan with dependencies or review gate  | Task board                                |
| Assigning work to a helper or human for sign-off  | Task (`claim` / `assignee` / `review`)    |
| Ephemeral chat, quick question, one-off nudge     | `via agent send`                         |
| Live prompt to an ACP pane without board state    | `via agent send`                         |

Prefer the board for structured workflow; use `via agent send` for ad-hoc messages.

## Task boards (workspace-scoped)

Tasks survive via restarts; they live on a **board** within a **workspace**
(sanitized absolute project `cwd`), not in the ephemeral instance directory.
Restart / `via task list` reuses the existing active board; only create a board
when none exist yet, or explicitly with `via task board new --id …`.

```bash
via task board list
via task board new --id phase2 --title "Phase 2 work"   # explicit; activates
via task board use default

via task list
via task create "<short title>" -m "<context/summary>"   # auto id like "a3kx"
via task create "Named milestone" --id task-1
via task claim task-1
via task update task-1 --status review
via task done task-1
```

### Lifecycle checklist

1. **Create** — short title + rich `-m` body (required for durable handoff).
2. **Claim** — `via task claim <id>` (sets assignee to you, status `in_progress`).
3. **Work** — keep status `in_progress`; append notes to the body when useful.
4. **Review** — `via task review <id>` or `via task update <id> --status review`
   (notifies the primary `agent` pane and opens the review surface).
5. **Done** — only after human/reviewer sign-off: `via task done <id>`.

Do not mark work `done` yourself when a review gate is expected.

### Add context when you create a task

A task on the board may be picked up by another agent or a human later - write
it so it can be worked without you being present. Pass a `-m/--body` summary on
**every** `via task create`. Keep the **title** short (one line); put the detail
in the **body**. A good body contains:

- **Goal**: what done looks like, in one or two sentences.
- **Scope**: files/modules/areas to touch (and what to leave alone).
- **Repro / acceptance**: how to verify (commands, expected output, test names
  that must pass).
- **Dependencies**: link upstream tasks with `--blocked-by <ids>` (if any)
- **Pointers**: `file_path:line` references for the code that motivates the
  task, and any prior-session notes (`@skills/...`, Obsidian notes, etc.).

Linking in the body: use the `via:<id>` form (the same prefix the board rows
use) so dependencies are greppable in `via task list` / `via task show`.

```bash
# Minimal example: short title, rich body, dependency wiring.
via task create "Surface review state in board header" \
  -m "Add a review-count line to the board header in nvim/tasks.lua when any " \
     "task has status=review.\n\nGoal: a glance at :ViaTasks tells the human " \
     "there are items awaiting review.\nScope: nvim/tasks.lua board_header() " \
     "only; no Rust changes.\nAcceptance: scripts/test-nvim.sh green; new test "\
     "for header with review items.\nDepends on: via:90ib" \
  --blocked-by 90ib
```

`via task` works from a via agent pane (uses instance `cwd`) or from the project
directory without a live instance.

When a live via instance is running, task transitions also deliver to agents:

- **Unassigned create**: notifies the primary `agent` pane (and `orchestrator`
  if spawned) that work is available on the board
- **Assignee**: notified on create, assignee change, or status change (ACP
  agents get a live prompt; PTY agents read `via agent inbox`)
- **Review gate**: moving a task to `review` notifies the primary `agent` pane
  for human sign-off **and** opens the configured review surface
  (`review_backend = "nvim"` opens a Neovim diff of working-tree changes;
  `review_backend = "hunk"` opens the inline hunk pane). This is the task-level
  human gate - distinct from ACP `session/request_permission` tool modals.

Spawned helpers also receive a compact board snapshot (active board, their
assigned tasks, top queued items) in their first prompt / mailbox note.

Tasks can optionally have a status update/report section for capturing the
decisions, risks, caveats and trade-offs found during implementation

## Command reference

| Command                                                       | Purpose                                           |
| ------------------------------------------------------------- | ------------------------------------------------- |
| `via agent whoami`                                            | Show this agent's id, role, and session           |
| `via agent list [--json]`                                     | List agents in this session                       |
| `via agent spawn --id ID [--role R] [--command CMD]`          | Open pane; preset fills missing role/command      |
| `via agent terminate --id ID`                                 | Close a sub-agent (not the primary `agent` pane)  |
| `via agent send [--to ID] -m TEXT [--no-focus] [--no-notify]` | Deliver to a registered agent (errors if missing) |
| `via agent inbox [--json] [--peek] [--wait SECONDS]`          | Read your mailbox (optionally block for new mail) |
| `via task board list\|new\|use`                               | Manage task boards in the workspace               |
| `via task list\|create\|show\|path\|claim\|update\|done`     | Tasks on the active board                         |

## Sandbox notes

These commands use local Unix sockets and files under the **instance** runtime
directory (`instances/<pid>/`) for the agent bus; tasks use `workspaces/` under
the via data directory.

If you see "VIA_SESSION is not set", you are not inside a via-launched pane
(agent bus commands only). `via task` can still run from the project directory.
