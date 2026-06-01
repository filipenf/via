# via editor skill

Use this skill when working in a repository where **via** is running with Neovim (live editor pane). via exposes a small CLI so agents can inspect editor state without scraping the terminal UI.

## When to use

- Before saying work is done or handing control back to the user
- After editing files that Neovim may still have open
- When you need to confirm there are no remaining errors or warnings from LSP, treesitter, or other diagnostic sources

## Setup

Load this skill at the start of a via-backed session:

```bash
via agent skill show
```

Or read the file directly:

```bash
via agent skill path
```

## Session resolution

When via launches Neovim and the agent, it exports `VIA_SESSION` into their environment. Commands you run inherit it and target that session automatically — no arguments needed.

This is the only resolution mechanism. If `VIA_SESSION` is not set (you are not in a terminal/agent launched by via), the commands print an error and exit; they do not guess a session.

## Recommended workflow

1. Confirm the session resolves:

   ```bash
   via session get
   ```

2. Pull diagnostics (JSON on stdout):

   ```bash
   via session diagnostics --json
   ```

3. For a specific file:

   ```bash
   via session diagnostics --file src/lib.rs --json
   ```

4. If `summary.errors > 0`, fix issues and re-run diagnostics before finishing.

## Command reference

| Command | Purpose |
|---------|---------|
| `via session list` | List all running via sessions |
| `via session get` | Show the session resolved from `VIA_SESSION` |
| `via session diagnostics [--file PATH] --json` | Export Neovim diagnostics |
| `via agent skill path` | Path to this skill file |
| `via agent skill show` | Print this skill to stdout |

## Output shape

`via session diagnostics --json` returns:

- `repo` — resolved repository root
- `path` — buffer path diagnostics were collected from
- `summary` — `{ errors, warnings, infos, hints }`
- `items` — list of `{ lnum, col, message, severity, source, code, ... }`

## Sandbox notes

These commands talk to via over **local Unix sockets**. If the agent runs in a sandbox, allow local socket access

If you see "VIA_SESSION is not set", you are not running inside a terminal or agent launched by via; the diagnostics commands are unavailable.

## Related via features

- `:ViaBufferSend` / `<leader>ab` — push buffer or visual selection to the agent (explicit context)
- Diagnostics CLI — pull structured state on demand (this skill)
