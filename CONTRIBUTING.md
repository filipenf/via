# Contributing to via

Thanks for your interest in via! via is an experimental Rust terminal
application that hosts Neovim and AI coding agents side-by-side in a single
native window, using libghostty's VT engine for rendering and input. It
emphasizes easy integration between agent and nvim while staying out of the way
of the developer

## Code of Conduct

Be kind, assume good intent, and keep discussions technical. We are a small
project; constructive feedback is appreciated.

## Prerequisites

You will need:

- A recent **stable Rust** toolchain (the project uses Rust 2024 edition).
- **Zig 0.15.2** — required to build the vendored `libghostty-vt` terminal
  engine at compile time.
- **git** — the `libghostty-vt-sys` build script uses it to fetch Ghostty
  sources.
- On Linux (the primary development and release target): system development
  libraries for fonts, Wayland/X11, and input.

### System libraries (Debian/Ubuntu example, matching CI)

```sh
sudo apt-get update
sudo apt-get install -y \
  libfontconfig1-dev \
  libfreetype6-dev \
  libwayland-dev \
  libx11-dev \
  libx11-xcb-dev \
  libxcb1-dev \
  libxcb-render0-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libxkbcommon-dev \
  pkg-config
```

### Zig + tools via mise (recommended)

The project pins Zig (and rust-analyzer) in `.mise.toml`:

```sh
mise install
```

The `.mise.toml` also defines handy tasks (run with `mise <task>` or
`mise run <task>`):

```sh
mise test
mise fmt
mise clippy
mise dev     # foreground run (no detach)
mise bench
mise build
mise clean
```

List available tasks with `mise tasks`. These activate the pinned tools (Zig,
etc.) for you.

Otherwise, install Zig 0.15.2 manually and ensure it is on `PATH`.

## Building

```sh
cargo build --release
./target/release/via
```

`libghostty-vt` is statically linked; no extra runtime `.so` search paths are
needed.

**First builds are slow.** The dependency
`libghostty-vt = { git = "...", rev = "..." }` (see [Cargo.toml](Cargo.toml))
triggers a checkout of Ghostty sources + a Zig build of the VT core during
`cargo build`. Results are cached under `target/`.

For local iteration without detach (Linux detaches by default to free the
terminal):

```sh
VIA_FOREGROUND=1 cargo run -- ...
# or, if using mise:
mise dev
```

## Testing, formatting, and linting

With mise (recommended if you're already using it for Zig):

```sh
mise test
mise fmt
mise clippy
```

Raw cargo commands (always work):

```sh
cargo test                 # unit tests (co-located with sources under #[cfg(test)])
cargo fmt -- --check
cargo clippy -- -D warnings
```

We use:

- `rustfmt.toml` at the root (edition + a few style choices).
- `clippy.toml` (currently minimal; MSRV will be recorded here later).

All tests live in `src/**` as `#[cfg(test)] mod tests`. There is no separate
`tests/` crate today (the combination of native winit window + PTYs + Neovim
makes fully hermetic GUI integration tests expensive to maintain).

We have Criterion micro-benchmarks for the two most user-visible
performance-sensitive surfaces:

- Layout calculations (`src/ui/ghostty/layout.rs`): split decisions, column
  reservation, focus-after-reference.
- Link / OSC 8 / symbol scanning (`src/ui/ghostty/links.rs`):
  `reference_target_from_row` and URI variants, exercised on every visible row
  of agent output.

**If your change touches layout math or the reference scanner, run `cargo bench`
locally (before/after) and include a short summary or `target/criterion/` report
link in the PR description.** The CI runs a quick, non-blocking informational
`cargo bench` pass on every PR for visibility (see `.github/workflows/ci.yml`);
it uses reduced sample counts so it finishes in reasonable time. For serious
measurements use the defaults on your machine.

`cargo bench` (or `mise bench`) requires the same system libs + Zig as a normal
build.

## Running a full via session locally

via launches Neovim in one pane and your agent in another (or a single-pane ACP
layout).

ACP agents (spawned orchestration / helpers):

```sh
VIA_FOREGROUND=1 cargo run -- --agent opencode
via agent spawn --id orchestrator --role orchestrator
```

Normal mode / PTY agents (primary interactive pane; context injected via
PTY):

```sh
VIA_FOREGROUND=1 cargo run -- --agent claude
```

In Neovim:

- `<leader>ab` or `:ViaBufferSend` — explicitly send the current buffer (or
  visual selection) to the agent. This is the single source of truth for
  context.
- Shift-click file paths or `symbol://Foo::bar` references in the agent output
  to open them in the Neovim pane.

See the main [README.md](README.md) for configuration (`~/.config/via/via.conf`,
env vars, `--agent-pane-cols`, review backends, font tweaks, etc.) and the
embedded Neovim Lua bridges (`nvim/*.lua`).

## Architecture pointers

- Original lightweight design brief: [project.md](project.md)
- ACP investigation and status: [acp.md](acp.md)
- Core runtime pieces (read these for context before touching):
  - `src/mediator.rs` — Tokio-based event router. Handles Neovim RPC events, ACP
    client, editor state (`src/editor.rs`), LSP bridge, PTY output, and UI
    commands.
  - `src/ui/ghostty.rs` (and `src/ui/ghostty/*`) — winit event loop +
    libghostty-vt surfaces, pane layout/splits/fullscreen, input routing, OSC 8
    link extraction, Ratatui-backed ACP pane, review terminal toggling (hunk or
    nvim), font rendering with cosmic-text.
  - `src/acp.rs` — small ACP JSON-RPC client (initialize, new_session,
    context/update, prompt, tool results).
  - `src/nvim.rs` + `nvim/` — nvim-rs RPC client + embedded Lua templates for
    file/symbol open, diagnostics export, review.
  - `src/config.rs`, `src/session.rs`, `src/cli/` — layered config (CLI > env >
    via.conf > defaults), session manifests (for cross-process `via session *`
    commands), subcommand implementations.
  - `src/pty.rs` — portable-pty wrapper with coalesced output notification.
  - `src/lsp_bridge.rs` — Unix socket bridge allowing the agent (via the skill)
    to perform LSP requests in the live Neovim session.

Key invariants the tests and benches protect:

- Layout must always reserve at least the editor minimum columns; agent pane
  respects its min/max range.
- Clicking references (file + optional `:line`, or symbol URIs) must be fast and
  focus the editor pane appropriately.
- Explicit context (`:ViaBufferSend`) is the contract for both PTY and ACP
  paths.

## Good places to start contributing

- Pane layout and resize edge cases (`src/ui/ghostty/layout.rs` and its tests).
- Link/symbol scanning robustness and performance (`src/ui/ghostty/links.rs`).
- Completing the ACP pane UX (model/mode selection, tool call rendering, diff
  preview, styling to match Neovim) — see `src/ui/ghostty/acp_pane.rs`,
  `acp_modal.rs`, and `mediator.rs`.
- Review backend improvements or the "hunk" backend.
- Diagnostics / session CLI ergonomics and the Lua side (`nvim/diagnostics.lua`,
  `src/cli/session.rs`).
- Making the first-time build experience better (docs, caching, clearer errors
  from the ghostty build script).
- More tests, especially property-style or boundary cases for layout and link
  parsing; Criterion benchmarks for the two hot paths (layout math and per-row
  reference scanning).
- Documentation, examples, and contributor onboarding.

If you're touching layout math or the main reference scanner, please add or
update a benchmark.

## Submitting a pull request

1. Fork the repo and create a topic branch.
2. Make your change + tests.
3. Run locally:

   ```sh
   cargo fmt -- --check
   cargo clippy -- -D warnings
   cargo test
   ```

   (If adding layout/links changes: also `cargo bench` / `mise bench` and note
   before/after numbers in the PR.)
4. Push and open a PR against `main`.
5. In the PR description, explain the motivation, user impact, and any
   performance or behavior changes. Link related issues.

CI (once added) will enforce formatting, clippy, and tests on Linux. The release
workflow remains separate (triggered by GitHub releases).

Small, reviewable PRs are strongly preferred. We can always iterate.

## Release process (for maintainers)

- Create a GitHub release with a tag like `v0.3.0`.
- The existing [`.github/workflows/release.yml`](.github/workflows/release.yml)
  builds on `ubuntu-24.04`, packages the binary + README into a
  `via-<tag>-linux-x86_64.tgz` (plus SHA256), and uploads the assets.
- Currently only linux-x86_64 is produced. Cross-platform binaries or source
  builds for macOS/Windows are future work.

## Questions?

Open a GitHub issue or discussion. Thank you for helping make via better!
