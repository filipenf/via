use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as create;
use nvim_rs::rpc::handler::Dummy;
use tokio::io::WriteHalf;
use tokio::net::UnixStream;
use tokio::time::{Duration, sleep};


type NvimWriter = Compat<WriteHalf<UnixStream>>;

const OPEN_FILE_LUA_TEMPLATE: &str = include_str!("../nvim/open_file.lua");
const OPEN_SYMBOL_LUA_TEMPLATE: &str = include_str!("../nvim/open_symbol.lua");

pub async fn open_file(socket_path: &Path, working_directory: &Path, target: FileTarget) -> Result<()> {
    if !socket_path.exists() {
        bail!(
            "Neovim RPC socket does not exist at {}. Start via with the same VIA_NVIM_SOCKET before using --open.",
            socket_path.display()
        );
    }

    let (nvim, io_handle) = connect(socket_path).await?;
    let command = file_open_command(&target, working_directory);

    nvim.command(&command)
        .await
        .with_context(|| format!("failed to send Neovim command `{command}`"))?;

    io_handle.abort();
    Ok(())
}

pub async fn open_symbol(socket_path: &Path, symbol: &str) -> Result<()> {
    if !socket_path.exists() {
        bail!(
            "Neovim RPC socket does not exist at {}. Start via with the same VIA_NVIM_SOCKET before opening symbols.",
            socket_path.display()
        );
    }

    let (nvim, io_handle) = connect(socket_path).await?;
    let command = symbol_open_command(symbol);

    nvim.command(&command)
        .await
        .with_context(|| format!("failed to send Neovim command `{command}`"))?;

    io_handle.abort();
    Ok(())
}

async fn connect(
    socket_path: &Path,
) -> Result<(
    nvim_rs::Neovim<NvimWriter>,
    tokio::task::JoinHandle<Result<(), Box<nvim_rs::error::LoopError>>>,
)> {
    let handler = Dummy::<NvimWriter>::new();
    let mut last_error = None;

    for _ in 0..50 {
        match create::new_path(socket_path, handler.clone()).await {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                last_error = Some(error);
                sleep(Duration::from_millis(20)).await;
            }
        }
    }

    Err(last_error
        .context("Neovim socket did not become available")?
        .into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    pub path: PathBuf,
    pub line: Option<u32>,
}

impl FileTarget {
    pub fn parse(input: &str, working_directory: &Path) -> Self {
        if let Some((path, line)) = parse_location(input) {
            Self {
                path: resolve_path(path, working_directory),
                line: Some(line),
            }
        } else {
            Self {
                path: resolve_path(input, working_directory),
                line: None,
            }
        }
    }
}

fn parse_location(input: &str) -> Option<(&str, u32)> {
    let input = input.trim_end_matches(':');
    let (path, last) = input.rsplit_once(':')?;

    if let Ok(last_line) = last.parse::<u32>() {
        if let Some((path, maybe_line)) = path.rsplit_once(':') {
            if let Some(line) = parse_line_start(maybe_line) {
                return Some((path, line));
            }
        }

        return Some((path, last_line));
    }

    parse_line_range_start(last).map(|line| (path, line))
}

fn parse_line_start(segment: &str) -> Option<u32> {
    segment
        .parse::<u32>()
        .ok()
        .or_else(|| parse_line_range_start(segment))
}

fn parse_line_range_start(segment: &str) -> Option<u32> {
    let (start, end) = segment.split_once('-')?;
    let start = start.parse::<u32>().ok()?;
    let end = end.parse::<u32>().ok()?;

    (start <= end).then_some(start)
}

fn resolve_path(path: &str, working_directory: &Path) -> PathBuf {
    if let Some(path) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(path);
        }
    }

    let path = PathBuf::from(path);

    if path.is_absolute() {
        path
    } else {
        working_directory.join(path)
    }
}

fn file_open_command(target: &FileTarget, working_directory: &Path) -> String {
    let path = target
        .path
        .strip_prefix(working_directory)
        .unwrap_or(&target.path);
    let path = lua_string_literal(&path.to_string_lossy());
    let line = target
        .line
        .map(|line| line.to_string())
        .unwrap_or_else(|| "nil".to_string());
    let replacements: [(&str, &str); 2] = [("__PATH__", path.as_str()), ("__LINE__", line.as_str())];

    lua_command(OPEN_FILE_LUA_TEMPLATE, replacements.as_slice())
}

fn symbol_open_command(symbol: &str) -> String {
    let symbol = lua_string_literal(symbol);
    let replacements: [(&str, &str); 1] = [("__SYMBOL__", symbol.as_str())];

    lua_command(OPEN_SYMBOL_LUA_TEMPLATE, replacements.as_slice())
}

fn lua_command(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut command = template.trim().to_string();

    for (needle, value) in replacements {
        command = command.replace(needle, value);
    }

    format!("lua {command}")
}

fn lua_string_literal(input: &str) -> String {
    let mut quoted = String::from("\"");

    for ch in input.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }

    quoted.push('"');
    quoted
}

pub fn log_socket_warning(_socket_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_path_with_line() {
        assert_eq!(
            FileTarget::parse("src/main.rs:42", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            }
        );
    }

    #[test]
    fn parses_path_with_line_and_column() {
        assert_eq!(
            FileTarget::parse("src/main.rs:42:7", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            }
        );
    }

    #[test]
    fn parses_path_with_line_range() {
        assert_eq!(
            FileTarget::parse("src/main.rs:3-8", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(3),
            }
        );
    }

    #[test]
    fn parses_path_with_line_range_and_column() {
        assert_eq!(
            FileTarget::parse("src/main.rs:3-8:5", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(3),
            }
        );
    }

    #[test]
    fn parses_path_with_trailing_colon_after_location() {
        assert_eq!(
            FileTarget::parse("src/main.rs:42:7:", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            }
        );
    }

    #[test]
    fn expands_tilde_home_paths() {
        let home = std::env::var_os("HOME").expect("HOME should be set in test environment");

        assert_eq!(
            FileTarget::parse("~/.worktrees/project/src/main.rs:42", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from(home).join(".worktrees/project/src/main.rs"),
                line: Some(42),
            }
        );
    }

    #[test]
    fn keeps_colon_when_suffix_is_not_a_line() {
        assert_eq!(
            FileTarget::parse("src/main.rs:not-a-line", Path::new("/repo")),
            FileTarget {
                path: PathBuf::from("/repo/src/main.rs:not-a-line"),
                line: None,
            }
        );
    }

    #[test]
    fn builds_file_open_command_with_window_fallback() {
        let command = file_open_command(
            &FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            },
            Path::new("/repo"),
        );
        assert!(command.starts_with("lua local path = \"src/main.rs\"; local line = 42;"));
        assert!(command.contains("vim.cmd('drop +' .. line .. ' ' .. escaped)"));
    }

    #[test]
    fn builds_symbol_open_command_with_telescope_fallback() {
        assert_eq!(
            symbol_open_command("Foo::bar"),
            "lua local s = \"Foo::bar\"; local ok, builtin = pcall(require, 'telescope.builtin'); if ok and builtin.lsp_workspace_symbols then\n  builtin.lsp_workspace_symbols({ query = s })\nelse\n  vim.lsp.buf.workspace_symbol(s)\nend"
        );
    }

    #[test]
    fn escapes_symbol_for_lua_literal() {
        assert_eq!(
            lua_string_literal("Foo\"\\\n\t"),
            "\"Foo\\\"\\\\\\n\\t\""
        );
    }
}
