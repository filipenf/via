# via

A Rust terminal application that bridges Neovim and AI agents via libghostty's
VT engine.

## Why via?

Via is my attempt at making terminal-only AI-assisted coding better. There are
plenty of Neovim plugins that put the AI assistant in a side pane, but my
experience working with them wasn't very enjoyable: too much flickering, weird
pane positioning, and so on. I thought wrapping Neovim + an agent in a dedicated
terminal window could deliver a smoother UX.

The idea is simple: glue the best code editor together with the AI coding
harness of your choice, and add a few features that make the combination feel
like a lightweight IDE.

By default you get an editor pane and an agent pane that automatically adjust
when the window is resized, plus simple shortcuts to switch panes or give either
one fullscreen focus.

It is currently my daily driver for AI-assisted coding. I use it mostly with
cursor-agent (~80%) and opencode (~20%), and I also test other agents like
claude-code and crush.

### Neovim to Agent

- Send the current selection or buffer with `<leader>ab` (or `:ViaBufferSend`)
- Multiple agents via the ACP protocol
- Agent orchestration (e.g. spawn a reviewer, take the feedback, apply changes)

### Agent to Neovim

- Hold Ctrl in the agent pane to highlight clickable filenames, symbols, and
  OSC 8 hyperlinks.
- Ctrl+click on a filename to open that file in Neovim and focus the Neovim pane
  (enter fullscreen Neovim if the agent was fullscreen; otherwise keep the
  split).
- Ctrl+click on a symbol to open the symbol search pane in Neovim with the same
  focus behavior.
- Ctrl+click on an external OSC 8 hyperlink to open it in the system browser.

## Multi-agent orchestration

The **default** layout is Neovim plus one **interactive PTY agent**
(`--agent opencode`). For everyday edits you work in that pane; no ACP process
runs at startup.

**Orchestration is opt-in.** Spawn an ACP orchestrator and helpers when you need
automatic multi-agent handoff:

```bash
via agent spawn --id orchestrator          # preset role: orchestrator
via agent spawn --id reviewer              # preset role: reviewer
via agent spawn --id coder                 # preset role: coder
via agent send --to reviewer -m "review this diff"
```

Spawned agents resolve to ACP when the configured driver supports it (`opencode`
→ `opencode acp`). If your main driver doesn't support ACP (e.g. claude, crush),
you can pick a different agent for orchestration with `--acp-agent`. The primary
PTY pane keeps the id `agent`; the coordinator is `orchestrator`.

**Design policy:** Via provides the transports (panes, bus, ACP). Agents
orchestrate themselves using skills and the `via agent` CLI; multi-agent
workflows are not encoded in the mediator.

### Per-agent models and drivers

Spawned helpers are independent ACP panes. Each `[agents.<id>]` preset can set a
**role**, an optional **model**, and an optional **command** — so you can mix
drivers and models per role (e.g. a planning orchestrator on a strong model, a
fast reviewer, a coding specialist on Composer).

Configure defaults in `~/.config/via/via.conf`:

```toml
# Primary PTY agent (interactive pane). Not an ACP process.
agent = "agent"

# Default ACP driver for spawned helpers when a preset omits `command`.
# Used when `agent` is not in the built-in ACP table (opencode, agent, cursor-agent).
# acp_agent = "cursor-agent acp"

[agents.orchestrator]
role = "orchestrator"
model = "claude-opus-4-8-thinking-high"

[agents.reviewer]
role = "reviewer"
model = "gpt-5.3-codex-fast"

[agents.coder]
role = "coder"
model = "composer-2.5"
# command = "opencode"   # optional; see “Command resolution” below
```

**Command resolution** (you usually do **not** need `command = "… acp"`):

| Preset `command` | Resolved spawn |
| --- | --- |
| *(omitted)* | Primary `agent` value, upgraded to ACP when known (`opencode` → `opencode acp`, `agent` → `agent acp`) |
| `opencode` or `agent` | Same binary with `acp` appended automatically |
| `opencode acp` | Used as-is (explicit is fine) |
| `cursor-agent acp` | Used as-is — pick a **different** driver than the primary PTY agent |

Built-in ACP upgrade covers `opencode`, `agent`, and `cursor-agent` only. For
other primaries (e.g. `claude`, `crush`), set `acp_agent` globally or
`command = "… acp"` on each helper preset.

**Model resolution** (applied after `session/new` via ACP `session/set_config_option`):

1. `via agent spawn --model …` / `via agent assign --model …` (one-shot override)
2. `[agents.<id>] model = "…"` in `via.conf`
3. Agent binary default

The active model is shown in the helper pane header. Requires an ACP-capable
spawn and an agent that exposes a `model` config option (support varies).

**Listing model slugs** (via does not ship a model picker yet — ask the driver):

```bash
agent models      # cursor-agent slugs, e.g. composer-2.5
opencode models   # opencode ids, e.g. opencode/claude-opus-4-8
```

Use the slug (left column / full id), not the display name, in `via.conf` or
`--model`.

**One-shot overrides** (orchestrator or any pane with `VIA_SESSION`):

```bash
via agent spawn --id coder --model composer-2.5
via agent assign --id coder --model gpt-5.3-codex --task abc -m "implement fix"
```

`--model` on `assign` applies only when via **spawns** the pane; it does not
retune an already-running helper. Terminate and respawn to change model.

**Example: mixed setup in one session**

```toml
agent = "agent"   # you work in the PTY pane

[agents.orchestrator]
role = "orchestrator"
model = "claude-opus-4-8-thinking-high"

[agents.reviewer]
role = "reviewer"
model = "gpt-5.3-codex-fast"

[agents.coder]
role = "coder"
model = "composer-2.5"
command = "opencode"   # coder uses opencode ACP while primary stays cursor-agent
```

```bash
via agent spawn --id orchestrator   # opus planner
via agent spawn --id reviewer       # fast codex reviewer
via agent spawn --id coder          # composer on opencode
# or override just this coder:
via agent spawn --id coder --model gpt-5.3-codex-high
```

**Navigation**

- `Alt+2..9` focuses the corresponding agent pane (Alt+2 is the first agent).
- `Alt+Shift+1..9` maximizes that pane (Alt+Shift+1 for the editor, Alt+Shift+2
  for the first agent, etc.).
- `Alt+J` toggles the split direction.

**Lua API for plugins**

`require('via')` is available inside any via-launched Neovim session (the module
is injected into `~/.local/share/via/lua/` at startup). Example usage:

```lua
local via = require('via')
via.agent.spawn("reviewer", "reviewer")                 -- spawn a reviewer pane
via.agent.spawn("coder", "coder", nil, "composer")      -- optional 4th arg: model slug
via.agent.del("reviewer")                               -- terminate a sub-agent when done
via.agent.send("reviewer", "please review this diff", false) -- send without stealing focus
via.agent.send("orchestrator", "hello orchestrator")    -- send after spawning orchestrator
for _, agent in ipairs(via.agent.list()) do print(agent.id) end -- discover running agents
```

**Agent-to-agent communication (the agent bus)**

Agents running inside via can discover, spawn, and message each other through
the `via agent` CLI (documented for agents in the bundled `via-agents` skill).
Each agent pane gets `VIA_AGENT_ID` and `VIA_AGENT_ROLE` in its environment.

```bash
via agent whoami                                  # this agent's id/role/session
via agent list                                    # agents running in this session
via agent spawn --id reviewer --role reviewer     # ask via to open a reviewer pane
via agent spawn --id coder --model composer       # override model for this spawn
via agent assign --id coder --model composer --task abc -m "implement"
via agent send --to reviewer -m "review this"     # queue a message + deliver it
via agent inbox                                   # read (and clear) your mailbox
```

Coordination notes:

- **PTY** panes (`agent`, or explicit non-ACP spawns) are mailbox-only on send.
- **ACP** spawned agents receive prompts automatically.
- Orchestration spawns require a known ACP mapping for the configured driver.

## Work in progress

This is still an experimental project. Although I use it as my daily "IDE", it
has some rough edges. Some things I have planned:

- Review process: make it easier to switch between agent/review and send
  feedback to the agent directly from the Neovim pane (may use an existing
  Neovim plugin for this)

## Runtime requirements

via is primarily developed and tested on Linux (Wayland compositors such as
Hyprland on Omarchy, with an X11 fallback via winit). Other Linux distributions
and operating systems (macOS, Windows) are not regularly tested; you will likely
need to build from source. See [CONTRIBUTING.md](CONTRIBUTING.md) for
prerequisites and platform notes.

## Build requirements

- Rust
- [Zig 0.15.2](https://ziglang.org) — required by the vendored `libghostty-vt`
  build.
- `git` (used by the `libghostty-vt-sys` build script to fetch ghostty sources).

If you use [mise](https://mise.jdx.dev/), the project's `.mise.toml` pins the
correct Zig version automatically:

```sh
mise install
```

Otherwise install Zig 0.15.2 manually and put it on your `PATH`.

## Build

```sh
cargo build --release
./target/release/via
```

`libghostty-vt` is statically linked into the binary, so no runtime library
search path setup is needed.

### Development

```sh
cargo test                 # unit tests live next to the code
cargo fmt -- --check
cargo clippy -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development guide
(prerequisites, how to run a local session, good first areas, benchmarks, etc.)
and [ARCHITECTURE.md](ARCHITECTURE.md) for an overview of the Mediator/UI/ACP
data flows and performance-sensitive surfaces.

`cargo test` is the primary regression guard today; we are expanding Criterion
micro-benchmarks for layout calculations and link/OSC 8 scanning (the two most
user-visible hot paths).

## Configuration

User-facing settings can be provided as CLI flags, environment variables, or in
`~/.config/via/via.conf` using TOML syntax. Precedence is:

```text
CLI flags > environment variables > via.conf > built-in defaults
```

Example config:

```toml
nvim = "nvim"
agent = "agent"             # PTY primary; spawned helpers resolve to ACP separately
# acp_agent = "cursor-agent acp"  # when primary is not ACP-capable (claude, crush, …)
agent_pane_cols = "80:120"
review_backend = "nvim"
plugin_dir = "~/my-via-plugin"

[agents.orchestrator]
model = "claude-opus-4-8-thinking-high"

[agents.reviewer]
model = "gpt-5.3-codex-fast"

[agents.coder]
model = "composer-2.5"
```

Per-agent presets (`role`, optional `command`, optional `model`) are documented
in [Multi-agent orchestration](#multi-agent-orchestration) — including how to
mix drivers, list model slugs, and override with `--model` at spawn time.

Equivalent CLI/env names:

- `--nvim` / `VIA_NVIM`
- `--agent` / `VIA_AGENT`
- `--acp-agent` / `VIA_ACP_AGENT`
- `--agent-pane-cols` / `VIA_AGENT_PANE_COLS`
- `--review-backend` / `VIA_REVIEW_BACKEND`
- `--plugin-dir` / `VIA_PLUGIN_DIR`

`plugin_dir` points at a local directory with extra agent skills (a `skills/`
subdirectory of `SKILL.md` files). via overlays them on top of its built-in base
skills when installing the plugin, so you can ship your own agents/workflows
without modifying via.

Use `--persist` to write the resolved user-facing config to `via.conf` before
running. For example, this writes `agent = "opencode"` (PTY primary) plus
defaults:

```sh
via --agent opencode --persist
```

Neovim bridge scripts (`nvim/*.lua`) are embedded at compile time; the context
bridge is written to via's data directory (`$XDG_DATA_HOME/via`, or
`~/.local/share/via`) when needed. Override with `VIA_NVIM_CONTEXT_BRIDGE` if
you want to load a custom script from disk during development.

## Release

Create and publish a GitHub release for a tag such as `v0.1.0`. The release
workflow builds `via` in release mode, packages the binary and README into
`via-<tag>-linux-x86_64.tgz`, and uploads the archive plus its SHA-256 checksum
to the release.

## Detached mode

On Linux, via detaches to avoid keeping the terminal waiting for it to finish.
Runtime files for each live process live under
`$XDG_DATA_HOME/via/instances/<pid>/` (default
`~/.local/share/via/instances/<pid>/`). Stale instance directories can be pruned
in bulk from that folder.

The runtime root is also exposed as `VIA_RUNTIME_ROOT` for scripts. To skip
detaching and keep the terminal attached (for example during development), set
`VIA_FOREGROUND` to any value.

## Agent pane width

With a PTY agent, vertical split mode keeps the agent at its minimum width
(default 80 columns, up to 100) and gives any extra columns to the editor.
Override with:

```sh
VIA_AGENT_PANE_COLS=60:120 cargo run
cargo run -- --agent-pane-cols 100
```

A single value pins the agent pane to that width; `min:max` gives a range.

The editor pane keeps at least 80 columns. When the window cannot fit both that
and the agent minimum (default 80 + 80 = 160 columns total), via collapses to
editor fullscreen. Widening enough to fit both restores the split unless you
chose fullscreen manually (Alt+Shift+1).

## Font Rendering Tweaks

via follows the window scale factor reported by winit/Wayland when converting
Ghostty's point-based `font-size` into physical pixels. On fractional-scale
setups this can differ from the compositor scale you configured, so font output
can change significantly between displays.

Use these environment variables to test font rendering without code changes:

```sh
VIA_FONT_SCALE=1.6 cargo run
VIA_FONT_HINTING=enabled cargo run
VIA_FONT_COVERAGE_BOOST=0 cargo run
```

Possible tweaks:

- `VIA_FONT_SCALE`: overrides the reported window scale used for font DPI, e.g.
  `1.33`, `1.6`, or `2.0`.
- `VIA_FONT_PIXEL_SCALE`: multiplies the computed glyph pixel size after DPI
  conversion.
- `VIA_CELL_WIDTH_SCALE`: multiplies the computed terminal cell width only.
- `VIA_CELL_HEIGHT_SCALE`: multiplies the computed terminal cell height only.
- `VIA_BASELINE_RATIO`: controls baseline placement inside each cell. Default:
  `0.73`.
- `VIA_FONT_HINTING`: sets cosmic-text metrics hinting. Values: `enabled` or
  `disabled`.
- `VIA_FONT_COVERAGE_BOOST`: controls via's glyph coverage boost. Default:
  `0.2`; use `0` to disable.
- `VIA_FONT_SHAPING`: selects cosmic-text shaping. Values: `advanced` or
  `basic`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, testing,
architecture pointers, and how to submit changes. Issues and PRs are welcome —
small, focused contributions (layout, links, ACP surface, review backends,
diagnostics, docs, tests) are especially appreciated.

## License

Licensed under the Apache License, Version 2.0. See the [LICENSE](LICENSE) file
for details.
