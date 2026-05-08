# via

A Rust terminal application that bridges Neovim and AI agents via libghostty's VT engine.

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

Neovim bridge scripts (`nvim/*.lua`) are embedded at compile time; the context
bridge is written to a temp file when needed. Override with `VIA_NVIM_CONTEXT_BRIDGE`
if you want to load a custom script from disk during development.
