# via

A Rust terminal application that bridges Neovim and AI agents via libghostty's
VT engine.

## why via?

I built via because I didn't like the way most AI plugins for nvim work. The
fundamental problem is that managing the AI panes inside nvim is tricky, you
get a lot of flickering, screen incorrectly laid out and hard to navigate, etc.

So via is my attempt to solve those problems by bridging the AI agent with
nvim. It creates separate panes (libghostty terminals) for each and connects
them.

**neovim to Agent**

- You can send the current selection or buffer with <leader>a,b

**Agent to nvim**

- Shift+click on a file name will open that file in nvim and focus the nvim pane (fullscreen nvim if the agent was fullscreen, otherwise keep the split)
- Shift+click on a symbol will open the Symbol search pane in neovim with the same focus behavior

## Work in progress

This is very much an experimental project, although I used it as my daily "IDE"
it has some rough edges still. Some things I have planned:

- Review process: make it easier to switch between agent/review and send
  feedback to the agent directly from the vim pane (may use some existing nvim
  plugin for this)
- Diagnostics integration: `via session diagnostics --json`; global agent skill auto-install (`via agent skill install` / `status`)
- Better use of LSP: the symbol search could be further updated to highlight
  known symbols on the agent pane

## Runtime requirements

Both my work and personal laptops are Omarchy Linux (so hyprland/wayland). I
haven't tried it in other linux distros and/or other operating systems

## Build requirements

- Rust (stable)
- [Zig 0.15.2](https://ziglang.org) — required by the vendored `libghostty-vt` build.
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

## Configuration

User-facing settings can be provided as CLI flags, environment variables, or in
`~/.config/via/via.conf` using TOML syntax. Precedence is:

```text
CLI flags > environment variables > via.conf > built-in defaults
```

Example config:

```toml
nvim = "nvim"
agent = "opencode acp"
agent_pane_cols = "80:120"
review_backend = "nvim"
```

Equivalent CLI/env names:

- `--nvim` / `VIA_NVIM`
- `--agent` / `VIA_AGENT`
- `--agent-pane-cols` / `VIA_AGENT_PANE_COLS`
- `--review-backend` / `VIA_REVIEW_BACKEND`

Use `--persist` to write the resolved user-facing config to `via.conf` before
running. For example, this writes `agent = "opencode"` plus the resolved defaults
for the other user-facing settings:

```sh
via --agent opencode --persist
```

Neovim bridge scripts (`nvim/*.lua`) are embedded at compile time; the context
bridge is written to via's data directory (`$XDG_DATA_HOME/via`, or
`~/.local/share/via`) when needed. Override with `VIA_NVIM_CONTEXT_BRIDGE`
if you want to load a custom script from disk during development.

## Release

Create and publish a GitHub release for a tag, such as `v0.1.0`. The release
workflow builds `via` in release mode, packages the binary and README into
`via-<tag>-linux-x86_64.tgz`, and uploads the archive plus its SHA-256 checksum
to the release.

## Detached mode

On linux via detaches to avoid keeping the terminal waiting for it to finish.
Logs, sockets, and temp files are stored in `/tmp/via-<pid>/`

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

- `VIA_FONT_SCALE`: overrides the reported window scale used for font DPI, e.g. `1.33`, `1.6`, or `2.0`.
- `VIA_FONT_PIXEL_SCALE`: multiplies the computed glyph pixel size after DPI conversion.
- `VIA_CELL_WIDTH_SCALE`: multiplies the computed terminal cell width only.
- `VIA_CELL_HEIGHT_SCALE`: multiplies the computed terminal cell height only.
- `VIA_BASELINE_RATIO`: controls baseline placement inside each cell. Default: `0.73`.
- `VIA_FONT_HINTING`: sets cosmic-text metrics hinting. Values: `enabled` or `disabled`.
- `VIA_FONT_COVERAGE_BOOST`: controls via's glyph coverage boost. Default: `0.2`; use `0` to disable.
- `VIA_FONT_SHAPING`: selects cosmic-text shaping. Values: `advanced` or `basic`.
