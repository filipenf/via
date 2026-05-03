use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as create;
use nvim_rs::rpc::handler::Dummy;
use tokio::io::WriteHalf;
use tokio::net::UnixStream;
use tokio::time::{Duration, sleep};
use tracing::{debug, warn};

type NvimWriter = Compat<WriteHalf<UnixStream>>;

pub async fn open_file(socket_path: &Path, target: FileTarget) -> Result<()> {
    if !socket_path.exists() {
        bail!(
            "Neovim RPC socket does not exist at {}. Start Spectre with the same SPECTRE_NVIM_SOCKET before using --open.",
            socket_path.display()
        );
    }

    let (nvim, io_handle) = connect(socket_path).await?;
    let command = target.drop_command();

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
        match parse_location(input) {
            Some((path, line)) => Self {
                path: resolve_path(path, working_directory),
                line: Some(line),
            },
            None => Self {
                path: resolve_path(input, working_directory),
                line: None,
            },
        }
    }

    fn drop_command(&self) -> String {
        let path = escape_fname(&self.path);

        match self.line {
            Some(line) => format!("drop +{line} {path}"),
            None => format!("drop {path}"),
        }
    }
}

fn parse_location(input: &str) -> Option<(&str, u32)> {
    let input = input.trim_end_matches(':');
    let (path, last) = input.rsplit_once(':')?;
    let last = last.parse::<u32>().ok()?;

    if let Some((path, maybe_line)) = path.rsplit_once(':') {
        if let Ok(line) = maybe_line.parse::<u32>() {
            return Some((path, line));
        }
    }

    Some((path, last))
}

fn resolve_path(path: &str, working_directory: &Path) -> PathBuf {
    let path = PathBuf::from(path);

    if path.is_absolute() {
        path
    } else {
        working_directory.join(path)
    }
}

fn escape_fname(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace(' ', "\\ ");
    escaped
}

pub fn log_socket_warning(socket_path: &Path) {
    if !socket_path.exists() {
        warn!(socket = %socket_path.display(), "Neovim RPC socket does not exist yet");
    } else {
        debug!(socket = %socket_path.display(), "Neovim RPC socket is present");
    }
}

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
    fn builds_drop_command() {
        assert_eq!(
            FileTarget {
                path: PathBuf::from("src/main.rs"),
                line: Some(42),
            }
            .drop_command(),
            "drop +42 src/main.rs"
        );
    }
}
