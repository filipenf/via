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

pub const DEFAULT_AGENT_PANE_MIN_COLS: u16 = 80;
pub const DEFAULT_AGENT_PANE_MAX_COLS: u16 = 100;

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    pub agent_command: Option<String>,
    /// Minimum agent pane width in terminal columns (vertical split only).
    pub agent_pane_min_cols: Option<u16>,
    /// Maximum agent pane width in terminal columns (vertical split only).
    pub agent_pane_max_cols: Option<u16>,
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
        let agent_pane_min_cols = env_u16("VIA_AGENT_PANE_MIN_COLS");
        let agent_pane_max_cols = env_u16("VIA_AGENT_PANE_MAX_COLS");
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
            agent_pane_min_cols,
            agent_pane_max_cols,
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

    /// Column bounds for the agent pane in vertical split mode (PTY agent only).
    pub fn agent_pane_col_limits(&self) -> Option<(u16, u16)> {
        if self.agent_command.is_none() || self.is_acp_agent() {
            return None;
        }

        let min = self
            .agent_pane_min_cols
            .unwrap_or(DEFAULT_AGENT_PANE_MIN_COLS);
        let max = self
            .agent_pane_max_cols
            .unwrap_or(DEFAULT_AGENT_PANE_MAX_COLS);
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        Some((min, max))
    }

}

fn env_u16(key: &str) -> Option<u16> {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
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
    fn agent_pane_col_limits_default_to_eighty_and_one_hundred() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("agent".to_string()),
            agent_pane_min_cols: None,
            agent_pane_max_cols: None,
            review_backend: ReviewBackend::Nvim,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
        };

        assert_eq!(config.agent_pane_col_limits(), Some((80, 100)));
    }

    #[test]
    fn agent_pane_col_limits_normalizes_bounds() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("agent".to_string()),
            agent_pane_min_cols: Some(100),
            agent_pane_max_cols: Some(80),
            review_backend: ReviewBackend::Nvim,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
        };

        assert_eq!(config.agent_pane_col_limits(), Some((80, 100)));
    }

}
