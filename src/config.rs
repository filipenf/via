use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    pub agent_command: Option<String>,
    pub nvim_socket_path: PathBuf,
    pub editor_socket_path: PathBuf,
    pub working_directory: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let nvim_command = env::var("SPECTRE_NVIM").unwrap_or_else(|_| "nvim".to_owned());
        let agent_command = env::var("SPECTRE_AGENT").ok();
        let nvim_socket_path = env::var_os("SPECTRE_NVIM_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_socket_path);
        let editor_socket_path = env::var_os("SPECTRE_EDITOR_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_editor_socket_path);
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Self {
            nvim_command,
            agent_command,
            nvim_socket_path,
            editor_socket_path,
            working_directory,
        }
    }
}

fn default_nvim_socket_path() -> PathBuf {
    env::temp_dir().join(format!("spectre-nvim-{}.sock", std::process::id()))
}

fn default_editor_socket_path() -> PathBuf {
    env::temp_dir().join(format!("spectre-editor-{}.sock", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_process_scoped() {
        let path = default_nvim_socket_path();

        assert!(path.ends_with(format!("spectre-nvim-{}.sock", std::process::id())));
    }

    #[test]
    fn default_editor_socket_path_is_process_scoped() {
        let path = default_editor_socket_path();

        assert!(path.ends_with(format!("spectre-editor-{}.sock", std::process::id())));
    }
}
