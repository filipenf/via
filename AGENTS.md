# Agent Notes for via

This file captures project-specific conventions, gotchas, and workflows that are
useful when working on via with AI coding agents. It complements the
human-facing README.

## Testing

### Run the full local check

```bash
bash scripts/test-nvim.sh   # headless Neovim Lua tests
cargo test                  # Rust unit tests
mise fmt-fix
```

### Stress-test parallel-sensitive Rust tests

Tests that mutate process-wide state (e.g. `VIA_SESSION` via
`std::env::set_var`) are racy under parallel `cargo test`. After changing them,
run with extra threads repeatedly:

```bash
for i in 1 2 3 4 5; do
  cargo test 'assign_to_human|run_review' -- --test-threads=16 || break
done
```

Use the global `test_support::env_lock()` mutex to serialize env-mutating tests.

### Headless Neovim Lua test gotchas

- Scratch buffers (`nvim_create_buf(false, true)`) automatically clear
  `vim.bo.modified`. If a test asserts on the modified flag, create a
  normal-like buffer with `buftype = ""` and a name.
- Prefer `vim.split(text, "\n", { plain = true })` over `gmatch("[^\n]*")`; the
  latter yields a trailing empty line and causes round-trip drift.
- Test fixtures should mirror the real buffer's options. The `:ViaTasks` buffer
  uses `filetype = "via-tasks"` / `buftype = "acwrite"`; match `buftype` when
  buffer-option behavior matters.

## UI / Buffer Editing Conventions

### Save semantics

- **Validation can be all-or-nothing.** Parse and validate the whole buffer
  before issuing any side-effect commands. Publish diagnostics on malformed
  rows.
- **Execution is usually not transactional.** If you issue one command per row,
  a later row can fail after earlier rows succeeded.
- On partial execution failure:
  - Do **not** refresh the buffer (refreshing would clobber the user's unsaved
    edits).
  - Leave the buffer **modified** so the user can inspect and retry.
  - Report which rows succeeded and which failed.
- Be precise in doc comments: say "validation is all-or-nothing" and "execution
  is per-row, not transactional" rather than claiming the whole save is atomic.

### Defensive validation for editable IDs

When a buffer edits existing records by ID, validate every parsed ID against the
known snapshot. A typo'd ID otherwise looks like a valid row and its edits are
silently dropped. Surface unknown IDs as diagnostics, not silent skips.

## Rust

### Module layout

- Define types (`struct`, `enum`, and their `impl` blocks) at the top of the
  file: public items first, then `pub(super)` / crate-private, then module-private
  helpers. Functions and free helpers follow the type definitions.

### Environment variables in tests

- `std::env::set_var` / `remove_var` are process-global and unsafe to call from
  parallel tests without synchronization.
- Use `crate::test_support::env_lock()` to serialize env-mutating tests.
- Treat env mutation in tests as a design smell: prefer passing context
  explicitly when the code under test allows it.
