# Architecture of via

via is a native Rust GUI/terminal hybrid that places a running Neovim instance
and an AI coding agent (or reviewer) into the same window, using a
high-performance terminal emulator surface. The goal is zero-jank navigation and
explicit, structured context sharing instead of scraping or fragile PTY
injection.

## High-level goals (from the original brief)

- Unified environment: one programmatically controlled window with split panes.
- Semantic navigation: OSC 8 hyperlinks and scanned references let you Ctrl-click
  file paths / symbols in agent output and instantly focus the right location in
  Neovim; external OSC 8 hyperlinks open in the system browser.
- Shared context without manual shuffling: explicit buffer/selection/diagnostics
  hand-off from editor to agent (and back via links + review flows).
- Low overhead: the coordination layer should be invisible once you're in flow.

Success criteria called out early:

- Clicking a filename in the agent pane focuses Neovim in <50 ms.
- Adding another "agent-like" surface (reviewer, etc.) should only require a new
  entry in the message router.

Design philosophy: "The editor is the heart, the agent is the brain, and the
terminal wrapper is the nervous system."

**Orchestration policy:** via **transports** — panes, sockets, ACP sessions, and
the file-backed mailbox — it does not encode multi-agent workflows in Rust.
Agents **orchestrate** themselves via skills, CLI (`via agent`), and
(eventually) MCP tools. The primary interactive pane is always a PTY; spawned
helpers use ACP when the configured driver supports it.

## Core components and data flow

```
+------------------+          Unix socket / RPC           +------------------+
|   Neovim         | <----------------------------------> |   via (Rust)     |
| (editor pane)    |   (nvim-rs + embedded Lua bridges)   |                  |
+------------------+                                      |   Mediator       |
                                                          |   (tokio mpsc)   |
+------------------+          PTY (normal mode) or        |                  |
|   Agent          | <----------------------------------> |   - editor_state |
| (claude, etc.)   |   stdio JSON-RPC (ACP mode)          |   - acp_client   |
+------------------+                                      |   - lsp bridge   |
                                                          +--------+---------+
                                                                   |
                                                                   | UiCommand / Event
                                                                   v
                                                          +------------------+
                                                          |  GhosttyUi       |
                                                          |  (winit +        |
                                                          |   libghostty-vt) |
                                                          |                  |
                                                          |  - TerminalPanes |
                                                          |    (editor, agent|
                                                          |     , review)    |
                                                          |  - layout/split  |
                                                          |  - font + render |
                                                          |  - link handling |
                                                          |  - ACP ratatui   |
                                                          |    sub-pane      |
                                                          +------------------+
```

### Mediator (src/mediator.rs)

The central async coordinator. It owns:

- A `Config`.
- `EditorState` (current buffer, diagnostics per path, visual selection, LSP
  client summaries).
- Optional `AcpClient` (when `VIA_AGENT` ends with `acp`).
- PTY sessions for agents in normal mode and the review backend.
- Listeners for Neovim RPC (editor events, diagnostics, LSP clients) and the LSP
  socket bridge.
- Channels to drive the UI (`EventSender`, `UiCommand` receiver).

Key paths:

- On `BufferSendRequested` (from `:ViaBufferSend` / `<leader>ab`): always
  injects `@path` / `@path:start-end` into the primary PTY `agent` pane — never
  a spawned ACP helper. Targeted ACP context uses `via agent send --to <id>`
  (or Lua `require('via').agent.send`).
- Incoming Neovim events (diagnostics, visual selection, LSP client list)
  update `editor_state` and are forwarded where relevant.
- Reference navigation (file or symbol clicks) results in `nvim::open_file` /
  `open_symbol` RPC calls and a focus change command back to the UI.
- ACP tool results and session updates are turned into `@tool_result ...` lines
  for the (still-light) ACP UI surface.

The mediator is spawned once; `MediatorHandle` gives the UI the sending side and
a shutdown future.

### Ghostty UI layer (src/ui/ghostty.rs and submodules)

This is the "native window host". It owns the winit event loop, softbuffer
surface, and libghostty-vt `Terminal` instances (one per pane).

Major submodules:

- `layout.rs`: split vs maximized modes, `vertical_split_layout`,
  `vertical_split_fits`, `trailing_pane_cols`, focus-after-reference helpers.
  All the math that keeps the editor at ≥80 columns and respects the agent's
  `min:max` pane width preference.
- `pane_controller.rs` + `pane.rs`: wraps a `TerminalPane` (the libghostty
  surface), handles mouse (Ctrl-held reference cues, Ctrl-click for OSC 8 /
  scanned references, drag for selection, wheel), key forwarding, alt-screen
  detection for paging behavior, and review pane special cases.
- `links.rs`: the OSC 8 / "reference target" scanner.
  `reference_target_from_row` and URI forms turn `src/main.rs:42` or
  `` `Foo::bar` `` or `symbol://...` into `FileTarget` or `Symbol` that the
  mediator then resolves via Neovim RPC. This runs on visible rows; performance
  matters.
- `render.rs`: pixel buffer drawing, borders, inactive dim, selection highlight,
  damage tracking, color helpers. Also hosts a few color/luminance tests.
- `font.rs`: cosmic-text + swash based glyph rasterization with tunable
  hinting/shaping/coverage/boost. The actual terminal cells come from
  libghostty-vt; via renders the glyph bitmaps.
- `acp_modal.rs`: in-process Ratatui overlay for ACP permission / ask-question
  prompts (drawn via `draw_ratatui_buffer`). ACP agent transcript/input lives in
  the PTY-hosted `via --acp-tui` child (`src/acp_tui/`), not an in-window Buffer
  pane.
- `input.rs`: key and clipboard normalization.

Layout modes (Alt+1/2/Shift variants + Alt+Shift+3 for split direction) live in
the `WinitGhosttyApp`. The UI also owns an optional review pane (either a `nvim`
review command or the `hunk` tool).

Mouse handling for references ultimately emits `PaneCommand::OpenRequested`,
`SymbolOpenRequested`, or URL-open commands. File and symbol commands route
through the mediator to Neovim RPC + focus commands; external URLs launch in the
system browser without changing Neovim focus.

### Agent side: Normal mode (PTY) vs ACP mode (src/acp.rs + mediator + pty.rs)

- Normal mode (PTY, e.g. `VIA_AGENT="opencode"`): a `PtySession` is spawned for
  the agent. Output is fed to a libghostty `Terminal` in the agent pane. Context
  is injected by writing text into the PTY.
- ACP mode (spawned helpers, e.g. `opencode acp`): `AcpClient` spawns the agent
  as a stdio JSON-RPC subprocess. After `initialize` + `new_session`, prompts
  arrive via `via agent send --to <id>` / Lua `agent.send` (or typed into the
  ACP TUI). The agent UI is a PTY pane running `via --acp-tui` (plain ratatui),
  glued to the mediator over a side Unix socket. Permission / ask-question
  modals stay in-process unless **auto-approve** matches (see below). Tool
  permissions and results flow through the mediator.

**ACP permission auto-approve** (`src/acp_auto_approve.rs`, wired from
`AcpRuntime::handle_agent_event`): when a spawned helper sends
`session/request_permission`, via evaluates allow rules before opening the modal.
Built-ins (always on): any shell command whose executable is `via`; ACP tool kinds
`read` and `search`; read-only shell (`ls`, `pwd`, `cat`, `head`, `tail`, `rg`,
`fd`, and `git status|diff|log|show`). User extensions in `via.conf`
`[auto_approve]` add `commands` (base executable names) and `kinds` — they never
disable built-ins. Kinds `edit`, `delete`, and `move` are never auto-approved.
Miss → modal (never auto-deny). On match, via replies with JSON-RPC
`allow-once` when available. Policy is global from config, not per-agent state.
Contract: board task via:nce2.

`AcpClient` is intentionally minimal (handshake, session create, prompt,
tool result serialization). It is not a full agent SDK.

### Two coordination modes

- **PTY = interactive / manual (default primary).** `--agent opencode` starts a
  PTY pane (bus id `agent`). Bus sends are mailbox-only; read with
  `via agent inbox`.
- **ACP = orchestrated / automatic (spawned).** Spawn `orchestrator`,
  `reviewer`, etc.; via drives them over ACP. Bus sends and pane prompts go
  through `client.prompt()`. Auto handoff is ACP-only. The PTY `agent` pane
  remains for interactive work alongside spawned ACP helpers.

`--agent "… acp"` is rejected at startup; the primary pane is always a PTY.

`src/pty.rs` also provides `CoalescedOutputNotifier` so that bursty PTY readers
don't flood the UI thread with redraw events.

### Neovim integration (src/nvim.rs + nvim/\*.lua)

via embeds several small Lua files at compile time via `include_str!`:

- `context_bridge.lua`: the heart of editor → via notifications. Sets up
  autocmds for `DiagnosticChanged`, visual selection tracking,
  `LspAttach/Detach`, and the `<leader>ab` mapping. Pushes JSON over a Unix
  socket (`via_editor_socket`). Also listens on an LSP bridge socket so agents
  can drive real LSP requests (`textDocument/definition` etc.) inside the live
  Neovim session.
- `open_file.lua`, `open_symbol.lua`, `review.lua`, `diagnostics.lua`: small
  RPC-invoked scripts for jumping and for `via session diagnostics`.

The Rust side (`nvim.rs`) uses `nvim-rs` over the nvim socket to execute these
(or dynamic commands) and to parse diagnostics JSON.

Session discovery for CLI tools (`via session diagnostics`, the agent skill) is
done exclusively via the `VIA_SESSION` environment variable (written into child
envs by the main process and pointing at the **instance** manifest under
`instances/<pid>/session.json`). There is deliberately no "guess the newest
socket" fallback.

### Storage model: instances, workspaces, boards

Naming convention used in via:

| Term          | Meaning                                    | Lifetime  | On disk                              |
| ------------- | ------------------------------------------ | --------- | ------------------------------------ |
| **Instance**  | Live via process (pid, sockets, agent bus) | Ephemeral | `instances/<pid>/`                   |
| **Workspace** | Project scope (sanitized canonical `cwd`)  | Durable   | `workspaces/<id>/`                   |
| **Board**     | Task board within a workspace              | Durable   | `workspaces/<id>/boards/<board-id>/` |

Under `$XDG_DATA_HOME/via` (default `~/.local/share/via`):

```text
instances/<pid>/              # ephemeral runtime (prune stale dirs here)
  session.json                # instance manifest (VIA_SESSION)
  agents/                     # registry + inbox (agent bus)
  logs/
  *.sock

workspaces/<workspace-id>/    # durable per-project state
  meta.json                   # cwd, created_at
  active_board                # pointer to selected board
  boards/<board-id>/
    meta.json
    tasks/*.md                # one Markdown file per task (task_store)
```

- **Instance** — `SessionGuard` creates `instances/<pid>/` at startup and
  removes it on clean shutdown. Detached mode sets `VIA_RUNTIME_ROOT` to the
  same path. `via session list` discovers live instances by scanning
  `instances/*/session.json`.
- **Workspace** — id is a sanitized absolute path of the canonical working
  directory (`src/workspace.rs`), e.g. `/home/you/proj` →
  `home_you_proj`. Created lazily on first task operation. Browsable under
  `~/.local/share/via/workspaces/`.
- **Board** — startup and `via task *` **reuse** the active board (or the
  most recently used board if the pointer is stale); `default` is created
  only when the workspace has no boards yet. Explicit `via task board new
  --id …` creates and activates a new board. Board `meta.json` tracks
  `last_used_at` (updated on activate). Tasks on the active board survive
  via restarts; agent mailboxes do not (instance-scoped).

ACP **sessions** (JSON-RPC `new_session`) are a separate concept — subprocess
conversation handles inside an instance, not file-backed storage.

### Config, instances, and CLI (src/config.rs, src/session.rs, src/cli/\*)

Config resolution is strict precedence: CLI > env (`VIA_*`) >
`~/.config/via/via.conf` (TOML) > built-in defaults. The primary `agent` command
must be a PTY launch (not ending with `acp`). `orchestration_enabled` reflects
whether spawned helpers can resolve to ACP (known-agent table or `acp_agent`
override). Spawn presets (`[agents.orchestrator]`, `[agents.reviewer]`,
`[agents.coder]` in `via.conf`, plus built-in defaults) fill missing `role` /
`command` / `model` when opening helper panes. Each preset may include an optional
`model` slug; on ACP connect, `establish_acp` calls `session/set_config_option`
after `session/new` and pushes the result to the pane header via
`UiCommand::AcpSessionStatus`. Explicit `--model` on `via agent spawn` or
`assign` overrides the preset for that spawn. Requires ACP orchestration and
agent support for the `model` config option.

`SessionGuard` writes (and removes on drop) a per-instance manifest under
`instances/<pid>/` so that `via session ...` subcommands and agents running
inside the instance can find the right sockets without extra flags.

CLI subcommands live under `via session` (list, get, diagnostics — **live
instances**), `via agent` (list, spawn, send, inbox, whoami — the agent bus),
`via task` (list, create, show, claim, update, done — **task boards**), and
`via plugin` (install/status the plugin skills).

### Agent bus (src/agent_bus.rs)

Inter-agent discovery and messaging, all file-backed under a per-instance
`agents_dir` (recorded in the instance manifest so CLI commands resolve it from
`VIA_SESSION`):

- `registry.json` — the list of agent panes, written by the UI whenever it
  spawns a pane (`write_agent_registry`). This is the discovery surface
  (`via agent list`, Lua `require('via').agent.list()`).
- `inbox/<id>/*.json` — per-recipient mailbox files under `<runtime>/agents/`.
  `via agent send` appends one file per message (the source of truth) and
  notifies the mediator for ACP delivery via the `agent_send` editor-socket
  event; `via agent inbox` drains it.

Each agent pane is spawned with `VIA_AGENT_ID`/`VIA_AGENT_ROLE` so an agent
knows its own identity.

### Task board (src/task_store.rs, src/workspace.rs, src/cli/task.rs)

Structured work items on top of mailbox strings. Tasks live on a **board**
within a **workspace** — not in the ephemeral instance directory.

- `Task` — `id`, `title`, `status`, `assignee`, `blocked_by`, timestamps,
  optional `body`
- Status — `queued | in_progress | review | done | blocked`
- Storage — one JSON file per task under
  `workspaces/<id>/boards/<board-id>/tasks/`
- CLI — `via task list|create|show|claim|update|done`;
  `via task board list|new|use`
- `via task` resolves workspace from instance `cwd` (when `VIA_SESSION` is set)
  or current directory; no live instance required for read/write
- **Delivery** (`src/task_delivery.rs`) — when a live instance is running,
  `create` / `claim` / `update` / `done` enqueue mailbox messages and notify the
  mediator (same path as `via agent send`):
  - assignee notified on create, assignee change, or status change (skips
    self-notification when the actor is the assignee)
  - `review` status additionally notifies the primary `agent` pane (human gate)
    and sends a `review_requested` editor-socket signal so the mediator opens
    the configured review surface (`nvim` diff or hunk pane) automatically

### Plugin (src/plugin.rs)

The skills (and, later, agents/workflows/tools) via projects into the agent's
skill directory. A tiny embedded base (`via-editor`, `via-agents`) ships in the
binary so things work out of the box; users can point via at a local plugin
directory (`plugin_dir` / `VIA_PLUGIN_DIR` / `--plugin-dir`) whose `skills/*`
are overlaid on top. `via plugin install` (also run automatically at startup)
projects them into the detected agent family's skill roots.

### Other notable pieces

- `src/lsp_bridge.rs`: the socket side of the LSP request forwarding (agents
  ask, via forwards to Neovim's real clients, responses come back).
- `src/editor.rs`: the in-memory `EditorState` updated by the Lua bridge events.
- `src/event.rs`: the enum of all cross-layer messages (EditorEvent, AgentEvent,
  UiCommand, etc.).
- `build.rs`: currently a no-op (just rerun-if-changed). The real compile-time
  work happens inside the `libghostty-vt` crate's build script.

## Performance-sensitive surfaces (why we benchmark these)

1. Layout (`src/ui/ghostty/layout.rs`): `vertical_split_layout`, fit checks, and
   focus-after-reflow. These run on every resize and on navigation that changes
   split/max state. Bad math = jank or collapsed panes.
2. Link/reference scanning (`src/ui/ghostty/links.rs`):
    `reference_target_from_row` (and the URI variant) is conceptually per-row of
    agent output. It must be fast even when the agent dumps hundreds of lines
   containing paths and `` `symbols` ``. Rendering only scans visible agent rows
   while Ctrl is held, so clickable cues have no steady-state cost during normal
   output streaming.

We use Criterion for these (see `benches/`) so that changes have a regression
signal before they land.

Other hot-ish paths (font glyph rasterization, libghostty cell iteration +
damage, the render loop at ~60 fps) are harder to micro-benchmark in isolation
and are mitigated by the native + GPU nature of the stack.

## Current state and evolution notes (as of mid-2026)

- Hybrid support for normal mode (PTY) and ACP mode is live. Normal mode agents
  get the classic two-pane (or review) layout with raw terminal; ACP mode agents
  get the single-pane (editor-only) layout + a Ratatui transcript/prompt
  surface.
- Explicit context for the primary PTY pane is `:ViaBufferSend` / `<leader>ab`;
  ACP helpers get context via `via agent send --to <id>`. Auto-push of the
  active buffer was removed to reduce noise.
- The review backend can be `nvim` (opens a Neovim diff/review layout) or
  `hunk`.
- A "via-editor" skill is auto-installed for ACP agents so they can pull
  diagnostics without hallucinating file state.
- Multi-agent spawning: the orchestrator (primary agent) or Neovim Lua
  (`require('via').agent.spawn(id, role, command, model)`) can request additional
  agent panes at runtime. A spawned agent whose command ends in `acp` (see
  `config::is_acp_command`) gets an ACP transcript pane and a mediator-owned
  session; optional per-id `model` in `[agents.*]` or `--model` on spawn/assign
  is applied during the ACP handshake. Anything else gets a
  `PaneRole::AgentTerminal { id, label, command }` PTY pane. Spawned helpers
  join the registry and are reachable via Alt+1..9 but do not steal focus or
  reshape the active layout.
- Coordination is two-mode: PTY agents are interactive (manual
  `via agent inbox`), ACP agents are orchestrated (bus sends and pane prompts
  both go through `client.prompt()`, so a sub's turn starts automatically).
  Automatic handoff is ACP-only; see "Two coordination modes" above.
- The vendored Ghostty VT bits are pinned to a specific git rev of a
  libghostty-rs wrapper. This gives a single static binary but makes clean
  builds heavy (Zig + git).

See [acp.md](acp.md) for more on the ACP direction and open questions (prompt
editing, tool rendering, styling). Per-agent model selection via `[agents.*].model`
and `--model` is implemented for ACP spawns; UI model picker is not.

## Testing philosophy

- Unit tests are colocated and cover the pure/logic parts heavily (layout table,
  link regex-ish scanning, config precedence, CLI parsing, session manifest
  roundtrips, color math, pane mouse state machines, etc.).
- We are expanding Criterion benches for the two surfaces above.
- Full GUI + Neovim + real agent E2E is done manually today. A future hermetic
  harness (headless nvim + fake ACP agent + controlled window size) would be
  valuable

## Further reading

- [README.md](README.md) — user-facing usage, config, font knobs, detached mode,
  agent pane width rules.
- [CONTRIBUTING.md](CONTRIBUTING.md) — how to build, test, and submit.
- [project.md](project.md) — the original "lean roadmap" brief.
- [acp.md](acp.md) — ACP status and design spikes.
- Source files have targeted comments on invariants (e.g., coalescing,
  alt-screen paging, inactive dim, etc.).

If you're about to change layout math, the link scanner, or the mediator's
ACP/PTY routing, please look at the existing tests in those modules and consider
whether a new or updated benchmark is warranted.
