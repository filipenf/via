---
name: via-orc
description: >-
  Orchestrate multi-agent work in a via session: the main interactive agent stays the human point of
  contact, spawning orchestrator / coder / reviewer helpers and synthesizing their results. Use for
  complex multi-step work when VIA_SESSION is set — not every coding session. Links to via-agents for
  bus/board CLI details and board usage for durable shared context.
---

# via orchestration skill

Use this skill for **complex, multi-step work** that benefits from parallel helpers. Everyday edits
and simple tasks do not need it. The task board (see **via-agents**) is still useful on any task for
shared context between you and the human — orchestration is optional; the board is not only for orc.

## Role model

| Role | Who | Responsibility |
| ---- | --- | -------------- |
| **Human POC** | Primary interactive pane (`agent`) | Talk to the user, decide when to orchestrate, spawn helpers, synthesize results back to the human |
| **orchestrator** | Spawned ACP helper | Plan / decompose / coordinate helper work when useful |
| **coder** | Spawned ACP helper | Implement changes |
| **reviewer** | Spawned ACP helper | Review diffs / call out risks |

**You (the main `agent` pane) are the only human point of contact.** Do not expect the user to chat
with helper panes. Helpers report via the agent bus and the task board; you summarize and ask the
human for decisions or sign-off.

This is intentionally different from “make the spawned orchestrator the hub.” The orchestrator is a
helper you may spawn — not a replacement for the human-facing pane.

## When to start orchestration

Start when work needs coordinated handoff (plan → implement → review), parallel investigation, or a
clear review gate. Skip for tiny one-line fixes and single-agent interactive coding.

ACP helpers need an ACP-capable spawn mapping (configured agent in the known table, or
`acp_agent` / `--acp-agent`). Spawn is unavailable without that mapping.

## How to run the loop

1. **Confirm identity and who’s already running** — see **via-agents** (`via agent whoami`,
   `via agent list`).
2. **Put durable work on the board** — create / claim / append status updates so humans and helpers
   share context. Prefer the board over ephemeral chat for multi-step plans. Full CLI and lifecycle
   live in **via-agents**.
3. **Spawn helpers** (presets fill role/command when `--id` is `orchestrator`, `reviewer`, or
   `coder`):

   ```bash
   via agent spawn --id orchestrator
   via agent spawn --id reviewer
   via agent spawn --id coder
   ```

   Config presets and `--model` overrides: **via-agents**.
4. **Assign and message** — `via agent assign` / `via agent send` (details in **via-agents**). Keep
   the human conversation on this pane; nudge helpers on the bus.
5. **Review gate** — move tasks to `review` when ready for human/sign-off; synthesize helper findings
   for the user. Do not mark work `done` yourself when a review gate is expected.
6. **Tear down** when orchestration is finished:

   ```bash
   via agent terminate --id reviewer
   via agent terminate --id coder
   via agent terminate --id orchestrator
   ```

   The primary PTY `agent` pane cannot be terminated.

## Board vs messages (orchestration)

| Need | Use |
| ---- | --- |
| Durable plan, handoff, review gate | Task board (`via task …`) — **via-agents** |
| Live prompt / nudge to a helper | `via agent send` — **via-agents** |

Prefer the board for structured workflow; use the bus for ad-hoc prompts. Helpers also get a compact
board snapshot on spawn — still read `via task show <id>` before picking up work.

## Related skills

- **via-agents** — `via agent` / `via task` CLI reference, spawn presets, inbox, board lifecycle
- **via-editor** — diagnostics and session CLI when Neovim is attached
