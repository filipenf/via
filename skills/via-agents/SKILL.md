---
name: via-agents
description: >-
  Coordinate with other AI agents in a via session: discover running agents, spawn new
  role-based agents (orchestrator, reviewer, coder), and message them. Use when VIA_SESSION
  is set and you need another agent's help, want to delegate a sub-task, or were asked to
  review/hand off work to another agent.
---

# via agents skill

via can run several agent panes side by side (an orchestrator plus helpers such as a
reviewer or coder). This skill lets agents talk to each other through via using the `via`
CLI. It works whenever `VIA_SESSION` is set (you are running inside a via-launched pane).

## Your identity

Each agent pane gets `VIA_AGENT_ID` and `VIA_AGENT_ROLE` in its environment.

```bash
via agent whoami        # who am I, and which session
```

The primary pane's id is `orchestrator`. Messages addressed to no one default to it.

## Discover other agents

```bash
via agent list           # id + role of every agent in this session
via agent list --json    # machine-readable
```

## Spawn a new agent

Ask via to open another agent pane with a role. The id must be unique; reusing an id is a
no-op. The command defaults to the configured agent if omitted.

```bash
via agent spawn --id reviewer --role reviewer
via agent spawn --id coder --role coder --command "opencode"
```

## Terminate a sub-agent

When a helper is done (e.g. a reviewer finished), close its pane and tear down the session:

```bash
via agent terminate --id reviewer
```

From Neovim (after `require('via')`):

```lua
require('via').agent.del("reviewer")
-- or :ViaAgentDel reviewer
```

The primary `orchestrator` pane cannot be terminated this way.

## Send a message

Messages always go to the recipient's **mailbox** (the durable source of truth). By default
via also delivers them live; how depends on the recipient's mode:

- **ACP agents** (spawned with a command ending in `acp`) are delivered the full message as a
  prompt, so their next turn starts **automatically** — no human needed. This is how
  automatic handoff works.
- **PTY agents** (interactive panes, e.g. a human-driven `opencode`/`claude`) get a
  lightweight ping (`[via] new message from <sender>; run via agent inbox to read`). They act
  manually by reading their inbox; via never auto-submits into an interactive pane.

Pass `--no-notify` for a silent mailbox-only delivery, or `--no-focus` to deliver without
stealing focus.

```bash
via agent send --to reviewer -m "Please review the changes on this branch for correctness."
via agent send -m "Done with the refactor, handing back."   # to the orchestrator
```

To get automatic, hands-off handoff, spawn the helper in ACP mode, e.g.
`via agent spawn --id reviewer --role reviewer --command "opencode acp"`.

via delivers bus messages as ACP prompts once the sub-agent handshake finishes. If you
`via agent send` immediately after spawn, the message is queued in the mailbox and
auto-delivered when the reviewer session connects — you do not need to sleep first.

## Read your messages

```bash
via agent inbox          # read and clear your mailbox
via agent inbox --peek   # read without clearing
via agent inbox --json   # machine-readable
```

A typical PTY reviewer loop: when pinged, run `via agent inbox`, do the work, then
`via agent send --to orchestrator -m "review complete: ..."`. An ACP reviewer instead
receives the request as a prompt directly and replies with another `via agent send` — the
mailbox/inbox is still available for durability.

## Command reference

| Command | Purpose |
|---------|---------|
| `via agent whoami` | Show this agent's id, role, and session |
| `via agent list [--json]` | List agents running in this session |
| `via agent spawn --id ID [--role R] [--command CMD]` | Ask via to open a new agent pane |
| `via agent terminate --id ID` | Close a sub-agent pane and tear down its session |
| `via agent send [--to ID] -m TEXT [--no-focus] [--no-notify]` | Queue a message (and ping the pane) |
| `via agent inbox [--json] [--peek]` | Read (and clear) your mailbox |

## Sandbox notes

These commands talk to via over **local Unix sockets and files** under the session's runtime
directory. If you run in a sandbox, allow local socket and filesystem access.

If you see "VIA_SESSION is not set", you are not inside a via-launched agent pane and these
commands are unavailable.
