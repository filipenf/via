# Agent Client Protocol (ACP) in via

This document summarizes our investigation into Zed's **Agent Client Protocol (ACP)** and the current/future relationship between `via` and ACP.

## What is ACP?

ACP is an open, LSP-like protocol that standardizes communication between code editors and AI coding agents. Instead of every editor scraping PTYs or inventing ad-hoc text protocols (`@file\n`, etc.), ACP defines a structured bidirectional JSON-RPC channel (over stdio or sockets) for:

- Editor → Agent: context updates (`active_buffer`, workspace roots, diagnostics, etc.)
- Agent → Editor: tool calls, permission requests, streaming responses, session management
- Capabilities negotiation on startup

Major editors (Zed, JetBrains) and several agents already implement it. The **ACP Registry** (launched Jan 2026) makes agents discoverable across all ACP clients.

## Current State in via (as of May 2026)

### What we built

- `src/acp.rs`: A minimal ACP client (`AcpClient`) that can:
  - Spawn an agent subprocess (`opencode acp`, `cursor-agent acp`, etc.)
  - Perform the `initialize` handshake
  - Create sessions via `new_session`
  - Push structured context via `context/update`
- Integration into `Mediator`:
  - `connect_acp(command, args)` method
  - `acp_client` + `acp_session_id` fields
  - `BufferSendRequested` events are routed through ACP when a client is connected (otherwise fall back to legacy PTY injection)
- Auto-detection logic in `main.rs` (now removed): if `VIA_AGENT` ends with `acp`, it would call `connect_acp` and skip the PTY agent pane.

### What we discovered

1. **Most major agents support ACP**:
   - `opencode acp`
   - `claude acp` / Claude Code (official adapter)
   - `cursor-agent acp`
   - Gemini CLI (registry entry)
   - Crush (PR in review, ~90% complete)

2. **ACP agents are currently "ACP-only"**:
   - Commands like `opencode acp` speak JSON-RPC on stdio and expect the client to render everything.
   - They do **not** also provide their normal interactive TUI.
   - Running them directly hangs (they wait for the first `initialize` message).

3. **The duplicate-file problem**:
   - The original automatic `BufEnter` + `CursorMoved` context pushing was appending `@path\n` on every navigation.
   - We removed the automatic pushing entirely.
   - Context is now **explicit and on-demand** via `:ViaBufferSend` (or `<leader>ab`).

4. **Hybrid PTY + ACP is not trivial today**:
   - Most agents do not expose both a TUI and a parallel ACP control channel for the same session.
   - Running two separate invocations (one PTY, one ACP) would create disconnected sessions.

### Current State (May 2026)

We have activated the **ACP-only path** for agents whose command ends with ` acp`:

- `VIA_AGENT="opencode acp"` (or `cursor-agent acp`, `claude acp`, etc.) results in a single-pane layout (only Neovim).
- `via` spawns the agent as a stdio JSON-RPC subprocess.
- Performs the `initialize` handshake and creates a session.
- Starts a background reader task that logs every incoming JSON-RPC line at `info` level (visible with `RUST_LOG=info`).
- Context is pushed via structured `context/update` messages when the user invokes `:ViaBufferSend` or presses `<leader>ab`.
- Legacy PTY agents continue to work unchanged (`VIA_AGENT="claude"` etc. still get the two-pane layout and raw PTY injection).

The explicit `:ViaBufferSend` mechanism is the single source of truth for injecting context on both paths.

## Next Steps / Future Directions

### Short term (PTY path)

- Improve the explicit context mechanism if needed (e.g. better visual-mode handling, status messages, configurable keybinding).
- Consider adding a small Lua helper that shows "context sent" feedback.
- Possibly support sending additional context (open buffers list, diagnostics summary, etc.) on explicit request.

### Medium term (ACP exploration)

When we decide to invest in ACP, possible paths include:

1. **Full ACP-only mode**
   - Remove the PTY pane for the agent entirely.
   - Build a custom renderer in `via` that displays the agent's streaming thoughts, tool calls, and permission prompts.
   - This would give us structured context, typed tool calls, and no more prompt-injection hacks.
   - Trade-off: we lose whatever nice TUI the agent provides.

2. **Parallel control channel**
   - Keep the PTY/TUI for user interaction.
   - Spawn a second ACP process (or use a side socket if the agent supports it) purely for context injection and tool results.
   - Requires agents that can share session state across invocations or expose a control socket.

3. **Hybrid detection**
   - Re-introduce the `is_acp_agent()` check, but this time **keep** the PTY pane and only use ACP for the control-plane messages.
   - Only useful if we find agents that support both modes.

4. **Expand the minimal ACP client**
   - Handle streaming `response/chunk` notifications.
   - Implement tool-call execution and permission flows.
   - Add support for images, multi-file context blocks, slash commands, etc.

### Open questions

- Are there any agents that expose both a rich TUI **and** an ACP/side-channel control interface for the same session?
- Would a thin wrapper around existing agents (that speaks ACP on one side and drives the real agent on the other) be worth building?
- How important is it to preserve the exact TUI/UX of the chosen agent vs. building a via-native experience?

## References

- [Zed ACP documentation](https://zed.dev/acp)
- [ACP Registry announcement](https://zed.dev/blog/acp-registry)
- Agent commands: `opencode acp`, `cursor-agent acp`, `claude acp`

---

*This document was created after the initial ACP spike and the decision to remain PTY-first with explicit context injection.*