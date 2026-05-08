This project brief outlines a lightweight, high-performance coordination layer that bridges **Neovim** and **AI Agents** using a custom terminal wrapper.

## Project Name: **via**

**Core Concept:** A Rust-based terminal application wrapping libghostty that acts as a semantic mediator between Neovim (the editor) and AI Agents (the logic) via the **Agent Communication Protocol (ACP)**.

### 1. Primary Objectives

* **Unified Environment:** Host Neovim and Agent outputs in a single, programmatically controlled window (split-pane layout).
* **Semantic Navigation:** Use **OSC 8 (Terminal Hyperlinks)** to allow users to click file paths in the agent's output and instantly open them in the active Neovim session.
* **Shared Context:** Synchronize Neovim's state (active buffer, cursor position, diagnostics) with background agents to eliminate manual context-shuffling.

### 2. High-Level Architecture

* **The Wrapper (Rust):** The parent process that initializes libghostty. It manages the PTYs (Pseudo-Terminals) for Neovim and the Agent.
* **The Mediator (Async Rust/Tokio):** A message router that listens to:
  * **Neovim RPC:** Receives buffer/state updates.
  * **ACP Stream:** Receives agent suggestions and file references.
* **The UI (libghostty):** Renders the TUI and handles mouse events/hyperlink clicks.

### 3. Key Technical Features

| Feature            | Implementation                                                                                             |
| ------------------ | ---------------------------------------------------------------------------------------------------------- |
| **Terminal Host**  | **libghostty** via C/Rust FFI for GPU-accelerated rendering and modern protocol support.                   |
| **Agent Protocol** | **ACP** (Agent Communication Protocol) for flexible, multi-agent future-proofing.                          |
| **Editor Sync**    | **nvim-rs** crate to send :drop <file> commands to Neovim over a Unix socket.                              |
| **Navigation**     | **OSC 8 Injection**; the wrapper parses agent output and "wraps" file paths in clickable escape sequences. |

### 4. Implementation Roadmap (The "Lean" Approach)

* **Phase 1 (The Shell):** Build a basic Rust app that spawns Neovim in a libghostty surface.
* **Phase 2 (The Hook):** Implement an OSC 8 click-handler that triggers an RPC call to Neovim.
* **Phase 3 (The Context):** Create a Neovim Lua autocmd that "pushes" the current file path to the Rust wrapper on BufEnter.
* **Phase 4 (The Agent):** Connect a single ACP-capable agent (e.g., Claude or Goose) to the background stream.

### 5. Success Criteria

 1. **Zero-Jank Navigation:** Clicking a file name in the agent's terminal pane opens that file in Neovim in **<50ms**.
 2. **Stateless Flexibility:** Adding a second agent (like a Reviewer) requires only a new entry in the message router, not a rewrite of the UI.
 3. **Low Overhead:** The coordination layer stays out of the way, letting the user stay in "flow" within the Neovim environment.

> **Design Philosophy:** "The editor is the heart, the agent is the brain, and the terminal wrapper is the nervous system."

### 6. TODOs

[x] Fix viewport scrolling
[x] Improve navigation: Shortcuts should be Alt 1 (neovim), Alt 2 (agent). Alt+Shift+1 (neovim full), Alt+Shift+2 (agent full), Alt+Shift+3 split horizontal/vertical toggle
[ ] Static linking or alternative ways for distributing, ideally single binary
[ ] GPU rendering
