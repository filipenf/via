---
name: via-editor
description: >-
  Pull Neovim LSP diagnostics and via session state via CLI when VIA_SESSION is set. Use at the start of a via-backed
  session, before marking work done, after editing files Neovim may have open, or when checking errors with via session
  diagnostics. Also check the via task board and agent inbox at session start before large work.
---

# via editor skill

Use this skill when working in a repository where **via** is running with Neovim (live editor pane). via exposes a small
CLI so agents can inspect editor state without scraping the terminal UI.

## When to use

- At the start of a via-backed session (when `VIA_SESSION` is set)
- Before saying work is done or handing control back to the user
- After editing files that Neovim may still have open
- When you need to confirm there are no remaining errors or warnings from LSP, treesitter, or other diagnostic sources

## Setup

via installs this skill under your home directory for the configured agent when a session starts. You can refresh or
inspect paths with:

```bash
via plugin install
via plugin status
```

## Session resolution

When via launches Neovim and the agent, it exports `VIA_SESSION` into their environment. Commands you run inherit it and
target that session automatically — no arguments needed.

This is the only resolution mechanism. If `VIA_SESSION` is not set (you are not in a terminal/agent launched by via),
the commands print an error and exit; they do not guess a session.

## Recommended workflow

1. Confirm the session resolves:

   ```bash
   via session get
   ```

2. Check the task board and mailbox (structured work first):

   ```bash
   via task list
   via agent inbox --peek
   ```

   - Prefer tasks **already assigned to you** (`via task list`, then claim only if needed).
   - Claim unassigned work only when your role is expected to pick it up (e.g. coder /
     orchestrator triage) and nothing is already assigned to you. Reviewers and ad-hoc
     helpers should not auto-claim the queue.
   - Before large multi-step work with no matching board item, create one with a rich body so progress survives
     handoff/restart:

     ```bash
     via task create "<short title>" -m "<goal / scope / acceptance>"
     via task claim <id>
     ```

   - While working a task, append **Status updates** with `via task update <id> --append '…'`
     (does not replace the body). via itself auto-appends notes on claim / assign /
     review / done — still add your own progress notes. Keep progress on the task, not
     only in chat.

   Tiny one-line fixes do not need a task. Prefer the board for durable or multi-step work. Full lifecycle guidance
   lives in the **via-agents** skill.

3. Pull diagnostics (JSON on stdout). This refreshes unchanged Neovim buffers from disk with `:checktime` before reading
   diagnostics:

   ```bash
   via session diagnostics --json
   ```

4. For a specific file:

   ```bash
   via session diagnostics --file src/lib.rs --json
   ```

5. If `summary.errors > 0`, fix issues and re-run diagnostics before finishing.

If files were edited outside Neovim and you want to refresh buffers without reading diagnostics:

```bash
via session refresh
via session refresh --file src/lib.rs
```

## Command reference

| Command                                        | Purpose                                                                     |
| ---------------------------------------------- | --------------------------------------------------------------------------- |
| `via session list`                             | List all running via sessions                                               |
| `via session get`                              | Show the session resolved from `VIA_SESSION`                                |
| `via session diagnostics [--file PATH] --json` | Refresh unchanged Neovim buffers with `:checktime`, then export diagnostics |
| `via session refresh [--file PATH] [--json]`   | Ask Neovim to reload externally changed buffers                             |
| `via plugin install`                           | Install or update the via plugin skills                                     |
| `via plugin status`                            | Show install paths and state                                                |
| `via plugin cleanup`                           | Remove the base skills from every known location                            |
| `via plugin path`                              | Print the primary install root                                              |

## Output shape

`via session diagnostics --json` returns:

- `repo` — resolved repository root
- `path` — buffer path diagnostics were collected from
- `summary` — `{ errors, warnings, infos, hints }`
- `items` — list of `{ lnum, col, message, severity, source, code, ... }`

## Sandbox notes

These commands talk to via over **local Unix sockets**. If the agent runs in a sandbox, allow local socket access.

If you see "VIA_SESSION is not set", you are not running inside a terminal or agent launched by via; the diagnostics
commands are unavailable.

## Related via features

- `:ViaBufferSend` / `<leader>ab` — push buffer or visual selection to the agent (explicit context)
- Diagnostics CLI — pull structured state on demand (this skill)
- Task board / agent bus — see the **via-agents** skill (`via task …`, `via agent …`)
