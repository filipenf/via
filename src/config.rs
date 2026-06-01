use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

/// Embedded copy of `nvim/context_bridge.lua` (see `include_str!` below). At runtime we write it
/// to a path under the via runtime directory (see `runtime_base_dir`) so Neovim can `luafile` it.
static EMBEDDED_CONTEXT_BRIDGE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Directory for sockets, the context bridge script, and other per-process files.
///
/// After a detached start this is `<data dir>/via-<pid>/` from `VIA_RUNTIME_ROOT`. Otherwise it is
/// the via data directory itself (see [`via_data_dir`]), unless overridden per-path via environment
/// variables.
pub fn runtime_base_dir() -> PathBuf {
    env::var_os("VIA_RUNTIME_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(via_data_dir)
}

/// via's data directory: `$XDG_DATA_HOME/via`, falling back to `$HOME/.local/share/via`, then the
/// system temp dir as a last resort.
///
/// This is derived purely from `XDG_DATA_HOME`/`HOME` (never `VIA_RUNTIME_ROOT`), so a detached
/// via process and the child commands it spawns always agree on it. It is the parent of detached
/// per-pid runtime directories and the search root for session discovery.
pub fn via_data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("via");
        }
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/via");
    }
    env::temp_dir().join("via")
}

/// Ensure the runtime directory exists before sockets are bound or scripts are written.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(runtime_base_dir())
}

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    pub agent_command: Option<String>,
    pub review_backend: ReviewBackend,
    pub nvim_socket_path: PathBuf,
    pub editor_socket_path: PathBuf,
    pub nvim_context_bridge_path: PathBuf,
    pub lsp_bridge_socket_path: PathBuf,
    pub working_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewBackend {
    Hunk,
    Nvim,
}

impl Default for ReviewBackend {
    fn default() -> Self {
        Self::Nvim
    }
}

impl FromStr for ReviewBackend {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.trim().to_ascii_lowercase().as_str() {
            "hunk" => Ok(Self::Hunk),
            "nvim" | "vim" | "vimdiff" => Ok(Self::Nvim),
            other => Err(format!("unknown review backend `{other}`")),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let nvim_command = env::var("VIA_NVIM").unwrap_or_else(|_| "nvim".to_owned());
        let agent_command = env::var("VIA_AGENT").ok();
        let review_backend = env::var("VIA_REVIEW_BACKEND")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        let nvim_socket_path = env::var_os("VIA_NVIM_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_socket_path);
        let editor_socket_path = env::var_os("VIA_EDITOR_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_editor_socket_path);
        let nvim_context_bridge_path = env::var_os("VIA_NVIM_CONTEXT_BRIDGE")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_context_bridge_path);
        let lsp_bridge_socket_path = env::var_os("VIA_LSP_BRIDGE_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_lsp_bridge_socket_path);
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Self {
            nvim_command,
            agent_command,
            review_backend,
            nvim_socket_path,
            editor_socket_path,
            nvim_context_bridge_path,
            lsp_bridge_socket_path,
            working_directory,
        }
    }

    /// Returns true if the `VIA_AGENT` command ends with the `acp` subcommand.
    /// In that case we treat the agent as an ACP-only process (no PTY pane).
    pub fn is_acp_agent(&self) -> bool {
        self.agent_command
            .as_deref()
            .map(|cmd| cmd.split_whitespace().last() == Some("acp"))
            .unwrap_or(false)
    }

    pub fn apply_cli_args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg.as_ref();
            if let Some(value) = arg.strip_prefix("--review-backend=") {
                self.apply_review_backend(value);
                continue;
            }

            if arg == "--review-backend" {
                if let Some(value) = args.next() {
                    self.apply_review_backend(value.as_ref());
                }
            }
        }
    }

    fn apply_review_backend(&mut self, value: &str) {
        match value.parse() {
            Ok(review_backend) => self.review_backend = review_backend,
            Err(error) => tracing::warn!(%error, "ignoring invalid review backend"),
        }
    }
}

fn default_nvim_socket_path() -> PathBuf {
    runtime_base_dir().join(format!("via-nvim-{}.sock", std::process::id()))
}

fn default_editor_socket_path() -> PathBuf {
    runtime_base_dir().join(format!("via-editor-{}.sock", std::process::id()))
}

fn default_nvim_context_bridge_path() -> PathBuf {
    EMBEDDED_CONTEXT_BRIDGE_PATH
        .get_or_init(|| {
            let dir = runtime_base_dir();
            let path = dir.join(format!("via-context-bridge-{}.lua", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap_or_else(|err| {
                panic!("failed to create via runtime directory {}: {err}", dir.display());
            });
            std::fs::write(&path, include_str!("../nvim/context_bridge.lua")).unwrap_or_else(
                |err| {
                    panic!(
                        "failed to write embedded nvim/context_bridge.lua to {}: {err}",
                        path.display()
                    );
                },
            );
            path
        })
        .clone()
}

fn default_lsp_bridge_socket_path() -> PathBuf {
    runtime_base_dir().join(format!("via-lsp-bridge-{}.sock", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_process_scoped() {
        let path = default_nvim_socket_path();

        assert!(path.ends_with(format!("via-nvim-{}.sock", std::process::id())));
    }

    #[test]
    fn default_editor_socket_path_is_process_scoped() {
        let path = default_editor_socket_path();

        assert!(path.ends_with(format!("via-editor-{}.sock", std::process::id())));
    }

    #[test]
    fn default_nvim_context_bridge_path_materializes_embedded_lua() {
        let path = default_nvim_context_bridge_path();

        assert!(
            path.ends_with(format!("via-context-bridge-{}.lua", std::process::id())),
            "unexpected path: {}",
            path.display()
        );
        let contents = std::fs::read_to_string(&path).expect("read context bridge");
        assert!(
            contents.contains("vim.g.via_editor_socket"),
            "expected embedded bridge lua on disk"
        );
    }

    #[test]
    fn default_lsp_bridge_socket_path_is_process_scoped() {
        let path = default_lsp_bridge_socket_path();

        assert!(path.ends_with(format!("via-lsp-bridge-{}.sock", std::process::id())));
    }

    #[test]
    fn parses_review_backend_aliases() {
        assert_eq!("hunk".parse::<ReviewBackend>(), Ok(ReviewBackend::Hunk));
        assert_eq!("nvim".parse::<ReviewBackend>(), Ok(ReviewBackend::Nvim));
        assert_eq!("vimdiff".parse::<ReviewBackend>(), Ok(ReviewBackend::Nvim));
    }

    #[test]
    fn cli_review_backend_overrides_existing_value() {
        let mut config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: None,
            review_backend: ReviewBackend::Nvim,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/repo"),
        };

        config.apply_cli_args(["--review-backend", "hunk"]);

        assert_eq!(config.review_backend, ReviewBackend::Hunk);
    }
}
