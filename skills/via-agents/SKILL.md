---
name: via-agents
description: >-
  Coordinate with other AI agents in a via session: discover running agents,
  spawn new role-based agents (orchestrator, reviewer, coder), and message them.
  Use when VIA_SESSION is set and you need another agent's help, want to
  delegate a sub-task, or were asked to review/hand off work to another agent.
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

- **ACP agents** — full body delivered as a prompt (automatic turn).
- **PTY agents** — mailbox only; read with `via agent inbox`.

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

## Task boards (workspace-scoped)

Tasks survive via restarts; they live on a **board** within a **workspace** (hash
of project `cwd`), not in the ephemeral instance directory.

```bash
via task board list
via task board new --id phase2 --title "Phase 2 work"
via task board use default

via task list
via task create "Implement delivery hook" --id task-1
via task claim task-1
via task update task-1 --status review
via task done task-1
```

`via task` works from a via agent pane (uses instance `cwd`) or from the project
directory without a live instance.

When a live via instance is running, task transitions also deliver to agents:

- **Assignee** — notified on create, assignee change, or status change (ACP
  agents get a live prompt; PTY agents read `via agent inbox`)
- **Review gate** — moving a task to `review` also notifies the primary `agent`
  pane for human sign-off

Use `via agent send` for ad-hoc messages; tasks are for structured workflow.

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
| `via task list\|create\|show\|claim\|update\|done`            | Tasks on the active board                         |

## Sandbox notes

These commands use local Unix sockets and files under the **instance** runtime
directory (`instances/<pid>/`) for the agent bus; tasks use `workspaces/` under
the via data directory.

If you see "VIA_SESSION is not set", you are not inside a via-launched pane
(agent bus commands only). `via task` can still run from the project directory.
