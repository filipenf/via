# Agent Client Protocol (ACP) in via

This document summarizes our investigation into Zed's **Agent Client Protocol (ACP)** and the current/future relationship between `via` and ACP.

## What is ACP?

ACP is an open, LSP-like protocol that standardizes communication between code editors and AI coding agents. Instead of every editor scraping PTYs or inventing ad-hoc text protocols (`@file\n`, etc.), ACP defines a structured bidirectional JSON-RPC channel (over stdio or sockets) for:

- Editor → Agent: context updates (`active_buffer`, workspace roots, diagnostics, etc.)
- Agent → Editor: tool calls, permission requests, streaming responses, session management
- Capabilities negotiation on startup

Major editors (Zed, JetBrains) and several agents already implement it. The **ACP Registry** (launched Jan 2026) makes agents discoverable across all ACP clients.

## Current State in via (as of May 2026)

### What works so far

- `src/acp.rs`: A minimal ACP client (`AcpClient`) that can:
  - Spawn an agent subprocess (`opencode acp`, `cursor-agent acp`, etc.)
  - Perform the `initialize` handshake
  - Create sessions via `new_session`
  - Push structured context via `context/update`
- Integration into `Mediator`:
  - `connect_acp(command, args)` method
  - `acp_client` + `acp_session_id` fields
  - `BufferSendRequested` events are routed through ACP when a client is
    connected (otherwise fall back to legacy PTY injection)

### Current State (May 2026)

We have activated the **ACP-only path** for agents whose command ends with `acp`:

- `VIA_AGENT="opencode acp"` (or `cursor-agent acp`, `claude acp`, etc.) results in a single-pane layout (only Neovim).
- `via` spawns the agent as a stdio JSON-RPC subprocess.
- Performs the `initialize` handshake and creates a session.
- Starts a background reader task that logs every incoming JSON-RPC line at `info` level (visible with `RUST_LOG=info`).
- Context is pushed via structured `context/update` messages when the user invokes `:ViaBufferSend` or presses `<leader>ab`.
- Legacy PTY agents continue to work unchanged (`VIA_AGENT="claude"` etc. still get the two-pane layout and raw PTY injection).

The explicit `:ViaBufferSend` mechanism is the single source of truth for injecting context on both paths.

### Phase 2 (ACP/PTY hybrid) (in progress)

Hybrid mode support: Our goal is to have the best possible user experience,
supporting different usage types:

- Non-ACP mode for simple PTY/stdin/out coordination between nvim and the agent
- ACP mode: for agents that support ACP

ACP mode gives us more control over the agent, but it means we have to implement
the UI for prompts and responses. On PTY mode, the agent is exposed in a
separate pane, stdin/out is given directly to the user, but we can inject
context into it (ie current selection/buffer, etc)

Current status:

- PTY mode works by opening a side pane with the agent rendered in it, giving
  the user direct stdin/stdout and neovim can push context to it - current
  buffer/selection via (<leader>ab).
- ACP mode is partially implemented, mediator communicates with it and sends
  context.
- ACP mode now has a small read-only UI spike: when `VIA_AGENT` ends with
  `acp`, via opens a second pane backed by a Ratatui `Buffer` and paints those
  cells through via's existing native renderer. This is static placeholder
  content, not live ACP transcript/input yet.
- ACP Pane is now rendering, user an prompt and get responses back from the
  agent. UI is pretty basic still, need to match neovim's styling, allow
  for model selection, diff viewing, etc. When the user sends a message to
  the agent, we embed the current visual selection
- Prompt box scaling to multiline with the user input
- ACP pane scrolling

Next steps:

- Match neovim's styling on the ACP pane
- Improve ACP pane layout
- Mode selection (plan/build/etc)
- Model selection
- Tool request handling

### Open questions

- Library choice: Ratatui is the current direction. The first spike uses
  Ratatui as an in-process widget/buffer layer and converts Ratatui cells into
  via's pixel buffer instead of letting Ratatui own stdout/raw terminal state.
- Alternative architecture: spawning `via --agent-tui` as a subprocess remains
  viable, especially if the in-process bridge becomes too complex. That path
  would let Ratatui render normally to a PTY but would require a separate Unix
  socket IPC protocol between the parent via process and the TUI child.
- Prompt input: start with custom prompt editing or the small `tui-input` crate
  for single-line input. `ratatui-textarea` is a better fit only if ACP prompts
  need multiline editing, selection, undo/redo, or richer editor behavior.

## References

- [Zed ACP documentation](https://zed.dev/acp)
- [ACP Registry announcement](https://zed.dev/blog/acp-registry)
- Agent commands: `opencode acp`, `cursor-agent acp`, `claude acp`

---

_This document was created after the initial ACP spike and the decision to remain PTY-first with explicit context injection._
