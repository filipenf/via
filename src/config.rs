use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Embedded copy of `nvim/context_bridge.lua` (see `include_str!` below). At runtime we write it
/// to a path under the via runtime directory (see `runtime_base_dir`) so Neovim can `luafile` it.
static EMBEDDED_CONTEXT_BRIDGE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Directory for sockets, the context bridge script, and other per-process files.
///
/// After a detached start this is `/tmp/via-<pid>/` from `VIA_RUNTIME_ROOT`. Otherwise it matches
/// [`std::env::temp_dir`] unless overridden per-path via environment variables.
pub fn runtime_base_dir() -> PathBuf {
    env::var_os("VIA_RUNTIME_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    pub agent_command: Option<String>,
    pub nvim_socket_path: PathBuf,
    pub editor_socket_path: PathBuf,
    pub nvim_context_bridge_path: PathBuf,
    pub lsp_bridge_socket_path: PathBuf,
    pub working_directory: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let nvim_command = env::var("VIA_NVIM").unwrap_or_else(|_| "nvim".to_owned());
        let agent_command = env::var("VIA_AGENT").ok();
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
            nvim_socket_path,
            editor_socket_path,
            nvim_context_bridge_path,
            lsp_bridge_socket_path,
            working_directory,
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
            let path =
                runtime_base_dir().join(format!("via-context-bridge-{}.lua", std::process::id()));
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
}
