use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

use anyhow::{Context, Result as AnyResult};
use serde::Deserialize;

/// Ensures the embedded Lua files are written to disk exactly once per process.
static LUA_ASSETS_INITIALIZED: OnceLock<()> = OnceLock::new();

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

/// Stable directory for shared Lua modules that Neovim can require.
/// Located under the via data directory so it is the same for every via session
/// and can be shared across all running instances.
pub fn lua_dir() -> PathBuf {
    via_data_dir().join("lua")
}

/// Ensure the runtime directory exists before sockets are bound or scripts are written.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(runtime_base_dir())
}

pub const DEFAULT_AGENT_PANE_MIN_COLS: u16 = 80;
pub const DEFAULT_AGENT_PANE_MAX_COLS: u16 = 100;
pub const DEFAULT_SCROLL_SENSITIVITY: f32 = 3.0;

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    pub agent_command: Option<String>,
    /// Agent pane width bounds in terminal columns (vertical split only).
    pub agent_pane_cols: Option<AgentPaneCols>,
    pub review_backend: ReviewBackend,
    /// Mouse wheel sensitivity multiplier; higher scrolls faster, lower slower.
    pub scroll_sensitivity: f32,
    pub nvim_socket_path: PathBuf,
    pub editor_socket_path: PathBuf,
    pub nvim_context_bridge_path: PathBuf,
    pub nvim_via_module_path: PathBuf,
    pub lsp_bridge_socket_path: PathBuf,
    pub working_directory: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    pub nvim: Option<String>,
    pub agent: Option<String>,
    pub agent_pane_cols: Option<AgentPaneCols>,
    pub review_backend: Option<ReviewBackend>,
    pub scroll_sensitivity: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPaneCols {
    pub min: u16,
    pub max: u16,
}

impl std::fmt::Display for AgentPaneCols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.min == self.max {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{}:{}", self.min, self.max)
        }
    }
}

impl FromStr for AgentPaneCols {
    type Err = String;

    fn from_str(input: &str) -> std::result::Result<Self, Self::Err> {
        let input = input.trim();
        let parse_col = |value: &str| {
            value
                .parse::<u16>()
                .map_err(|_| format!("invalid agent pane column value `{value}`"))
                .and_then(|value| {
                    if value > 0 {
                        Ok(value)
                    } else {
                        Err("agent pane columns must be greater than 0".to_owned())
                    }
                })
        };

        let (min, max) = match input.split_once(':') {
            Some((min, max)) if !min.is_empty() && !max.is_empty() && !max.contains(':') => {
                (parse_col(min)?, parse_col(max)?)
            }
            Some(_) => return Err(format!("invalid agent pane columns `{input}`")),
            None => {
                let cols = parse_col(input)?;
                (cols, cols)
            }
        };

        Ok(if min <= max {
            Self { min, max }
        } else {
            Self { min: max, max: min }
        })
    }
}

impl<'de> Deserialize<'de> for AgentPaneCols {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewBackend {
    Hunk,
    #[default]
    Nvim,
}

impl std::fmt::Display for ReviewBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hunk => f.write_str("hunk"),
            Self::Nvim => f.write_str("nvim"),
        }
    }
}

impl FromStr for ReviewBackend {
    type Err = String;

    fn from_str(input: &str) -> std::result::Result<Self, Self::Err> {
        match input.trim().to_ascii_lowercase().as_str() {
            "hunk" => Ok(Self::Hunk),
            "nvim" | "vim" | "vimdiff" => Ok(Self::Nvim),
            other => Err(format!("unknown review backend `{other}`")),
        }
    }
}

impl<'de> Deserialize<'de> for ReviewBackend {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    nvim: Option<String>,
    agent: Option<String>,
    agent_pane_cols: Option<AgentPaneCols>,
    review_backend: Option<ReviewBackend>,
    scroll_sensitivity: Option<f32>,
}

impl From<FileConfig> for ConfigOverrides {
    fn from(config: FileConfig) -> Self {
        Self {
            nvim: config.nvim,
            agent: config.agent,
            agent_pane_cols: config.agent_pane_cols,
            review_backend: config.review_backend,
            scroll_sensitivity: config.scroll_sensitivity,
        }
    }
}

impl ConfigOverrides {
    fn from_env() -> Self {
        Self {
            nvim: env::var("VIA_NVIM").ok(),
            agent: env::var("VIA_AGENT").ok(),
            agent_pane_cols: env::var("VIA_AGENT_PANE_COLS")
                .ok()
                .and_then(|value| value.parse().ok()),
            review_backend: env::var("VIA_REVIEW_BACKEND")
                .ok()
                .and_then(|value| value.parse().ok()),
            scroll_sensitivity: env::var("VIA_SCROLL_SENSITIVITY")
                .ok()
                .and_then(|value| value.parse().ok()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedUserConfig {
    nvim_command: String,
    agent_command: Option<String>,
    agent_pane_cols: AgentPaneCols,
    review_backend: ReviewBackend,
    scroll_sensitivity: f32,
}

fn resolve_user_config_from_sources(
    cli: ConfigOverrides,
    env: ConfigOverrides,
    file: ConfigOverrides,
) -> ResolvedUserConfig {
    let nvim_command = cli
        .nvim
        .or(env.nvim)
        .or(file.nvim)
        .unwrap_or_else(|| "nvim".to_owned());
    let agent_command = cli.agent.or(env.agent).or(file.agent);
    let agent_pane_cols = cli
        .agent_pane_cols
        .or(env.agent_pane_cols)
        .or(file.agent_pane_cols);
    let review_backend = cli
        .review_backend
        .or(env.review_backend)
        .or(file.review_backend)
        .unwrap_or_default();
    let scroll_sensitivity = cli
        .scroll_sensitivity
        .or(env.scroll_sensitivity)
        .or(file.scroll_sensitivity)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_SCROLL_SENSITIVITY);

    ResolvedUserConfig {
        nvim_command,
        agent_command,
        agent_pane_cols: agent_pane_cols.unwrap_or(AgentPaneCols {
            min: DEFAULT_AGENT_PANE_MIN_COLS,
            max: DEFAULT_AGENT_PANE_MAX_COLS,
        }),
        review_backend,
        scroll_sensitivity,
    }
}

fn resolve_user_config(cli: ConfigOverrides) -> AnyResult<ResolvedUserConfig> {
    let file = config_file_overrides()?;
    let env = ConfigOverrides::from_env();
    Ok(resolve_user_config_from_sources(cli, env, file))
}

pub fn persist_resolved(cli: ConfigOverrides) -> AnyResult<PathBuf> {
    let path = config_file_path()
        .context("cannot determine via config path; set XDG_CONFIG_HOME or HOME")?;
    let config = resolve_user_config(cli)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    std::fs::write(&path, render_user_config(&config))
        .with_context(|| format!("failed to write config file {}", path.display()))?;
    Ok(path)
}

fn render_user_config(config: &ResolvedUserConfig) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "nvim = \"{}\"\n",
        toml_escape_string(&config.nvim_command)
    ));
    if let Some(agent) = &config.agent_command {
        output.push_str(&format!("agent = \"{}\"\n", toml_escape_string(agent)));
    }
    output.push_str(&format!(
        "agent_pane_cols = \"{}\"\n",
        config.agent_pane_cols
    ));
    output.push_str(&format!("review_backend = \"{}\"\n", config.review_backend));
    output.push_str(&format!(
        "scroll_sensitivity = {}\n",
        config.scroll_sensitivity
    ));
    output
}

fn toml_escape_string(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

impl Config {
    pub fn load(cli: ConfigOverrides) -> AnyResult<Self> {
        let user_config = resolve_user_config(cli)?;
        let nvim_socket_path = env::var_os("VIA_NVIM_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_socket_path);
        let editor_socket_path = env::var_os("VIA_EDITOR_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_editor_socket_path);
        let nvim_context_bridge_path = env::var_os("VIA_NVIM_CONTEXT_BRIDGE")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_context_bridge_path);
        let nvim_via_module_path = env::var_os("VIA_NVIM_VIA_MODULE")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_via_module_path);
        let lsp_bridge_socket_path = env::var_os("VIA_LSP_BRIDGE_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_lsp_bridge_socket_path);
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Ok(Self {
            nvim_command: user_config.nvim_command,
            agent_command: user_config.agent_command,
            agent_pane_cols: Some(user_config.agent_pane_cols),
            review_backend: user_config.review_backend,
            scroll_sensitivity: user_config.scroll_sensitivity,
            nvim_socket_path,
            editor_socket_path,
            nvim_context_bridge_path,
            nvim_via_module_path,
            lsp_bridge_socket_path,
            working_directory,
        })
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

        let cols = self.agent_pane_cols.unwrap_or(AgentPaneCols {
            min: DEFAULT_AGENT_PANE_MIN_COLS,
            max: DEFAULT_AGENT_PANE_MAX_COLS,
        });
        Some((cols.min, cols.max))
    }
}

fn config_file_overrides() -> AnyResult<ConfigOverrides> {
    let Some(path) = config_file_path() else {
        return Ok(ConfigOverrides::default());
    };
    if !path.exists() {
        return Ok(ConfigOverrides::default());
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: FileConfig = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(config.into())
}

fn config_file_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir.join("via/via.conf"));
        }
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/via/via.conf"))
}

fn default_nvim_socket_path() -> PathBuf {
    runtime_base_dir().join(format!("via-nvim-{}.sock", std::process::id()))
}

fn default_editor_socket_path() -> PathBuf {
    runtime_base_dir().join(format!("via-editor-{}.sock", std::process::id()))
}

fn ensure_lua_assets() {
    LUA_ASSETS_INITIALIZED.get_or_init(|| {
        let dir = lua_dir();
        std::fs::create_dir_all(&dir).unwrap_or_else(|err| {
            panic!(
                "failed to create via lua directory {}: {err}",
                dir.display()
            );
        });
        for (filename, content) in [
            (
                "context_bridge.lua",
                include_str!("../nvim/context_bridge.lua"),
            ),
            ("via.lua", include_str!("../nvim/via.lua")),
        ] {
            let path = dir.join(filename);
            if !path.exists() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    if f.write_all(content.as_bytes()).is_err() {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    });
}

fn default_nvim_context_bridge_path() -> PathBuf {
    ensure_lua_assets();
    lua_dir().join("context_bridge.lua")
}

fn default_nvim_via_module_path() -> PathBuf {
    ensure_lua_assets();
    lua_dir().join("via.lua")
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
            path.ends_with("context_bridge.lua"),
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
    fn parses_agent_pane_cols() {
        assert_eq!(
            "100".parse::<AgentPaneCols>(),
            Ok(AgentPaneCols { min: 100, max: 100 })
        );
        assert_eq!(
            "80:120".parse::<AgentPaneCols>(),
            Ok(AgentPaneCols { min: 80, max: 120 })
        );
        assert_eq!(
            "120:80".parse::<AgentPaneCols>(),
            Ok(AgentPaneCols { min: 80, max: 120 })
        );
    }

    #[test]
    fn rejects_invalid_agent_pane_cols() {
        for value in ["0", "80:", ":120", "80:120:140", "abc"] {
            assert!(value.parse::<AgentPaneCols>().is_err(), "{value}");
        }
    }

    #[test]
    fn cli_overrides_env_and_file_config() {
        let file = ConfigOverrides {
            nvim: Some("file-nvim".to_string()),
            agent: Some("file-agent".to_string()),
            agent_pane_cols: Some(AgentPaneCols { min: 70, max: 90 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(0.5),
        };
        let env = ConfigOverrides {
            nvim: Some("env-nvim".to_string()),
            agent: Some("env-agent".to_string()),
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: Some(ReviewBackend::Hunk),
            scroll_sensitivity: Some(0.75),
        };
        let cli = ConfigOverrides {
            nvim: Some("cli-nvim".to_string()),
            agent: Some("cli-agent".to_string()),
            agent_pane_cols: Some(AgentPaneCols { min: 90, max: 110 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(2.0),
        };

        let config = resolve_user_config_from_sources(cli, env, file);

        assert_eq!(config.nvim_command, "cli-nvim");
        assert_eq!(config.agent_command.as_deref(), Some("cli-agent"));
        assert_eq!(config.agent_pane_cols, AgentPaneCols { min: 90, max: 110 });
        assert_eq!(config.review_backend, ReviewBackend::Nvim);
        assert_eq!(config.scroll_sensitivity, 2.0);
    }

    #[test]
    fn env_overrides_file_config() {
        let file = ConfigOverrides {
            nvim: Some("file-nvim".to_string()),
            agent: Some("file-agent".to_string()),
            agent_pane_cols: Some(AgentPaneCols { min: 70, max: 90 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(0.5),
        };
        let env = ConfigOverrides {
            nvim: Some("env-nvim".to_string()),
            agent: Some("env-agent".to_string()),
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: Some(ReviewBackend::Hunk),
            scroll_sensitivity: Some(0.75),
        };

        let config = resolve_user_config_from_sources(ConfigOverrides::default(), env, file);

        assert_eq!(config.nvim_command, "env-nvim");
        assert_eq!(config.agent_command.as_deref(), Some("env-agent"));
        assert_eq!(config.agent_pane_cols, AgentPaneCols { min: 80, max: 100 });
        assert_eq!(config.review_backend, ReviewBackend::Hunk);
        assert_eq!(config.scroll_sensitivity, 0.75);
    }

    #[test]
    fn user_config_defaults_are_resolved() {
        let config = resolve_user_config_from_sources(
            ConfigOverrides::default(),
            ConfigOverrides::default(),
            ConfigOverrides::default(),
        );

        assert_eq!(config.nvim_command, "nvim");
        assert_eq!(config.agent_command, None);
        assert_eq!(config.agent_pane_cols, AgentPaneCols { min: 80, max: 100 });
        assert_eq!(config.review_backend, ReviewBackend::Nvim);
        assert_eq!(config.scroll_sensitivity, DEFAULT_SCROLL_SENSITIVITY);
    }

    #[test]
    fn renders_resolved_user_config() {
        let output = render_user_config(&ResolvedUserConfig {
            nvim_command: "nvim-nightly".to_string(),
            agent_command: Some("opencode acp".to_string()),
            agent_pane_cols: AgentPaneCols { min: 80, max: 120 },
            review_backend: ReviewBackend::Hunk,
            scroll_sensitivity: 1.5,
        });

        assert_eq!(
            output,
            concat!(
                "nvim = \"nvim-nightly\"\n",
                "agent = \"opencode acp\"\n",
                "agent_pane_cols = \"80:120\"\n",
                "review_backend = \"hunk\"\n",
                "scroll_sensitivity = 1.5\n",
            )
        );
    }

    #[test]
    fn parses_file_config() {
        let config: FileConfig = toml::from_str(
            r#"
nvim = "nvim-nightly"
agent = "opencode acp"
agent_pane_cols = "80:120"
review_backend = "hunk"
scroll_sensitivity = 1.5
"#,
        )
        .unwrap();

        assert_eq!(config.nvim.as_deref(), Some("nvim-nightly"));
        assert_eq!(config.agent.as_deref(), Some("opencode acp"));
        assert_eq!(
            config.agent_pane_cols,
            Some(AgentPaneCols { min: 80, max: 120 })
        );
        assert_eq!(config.review_backend, Some(ReviewBackend::Hunk));
        assert_eq!(config.scroll_sensitivity, Some(1.5));
    }

    #[test]
    fn agent_pane_col_limits_default_to_eighty_and_one_hundred() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("agent".to_string()),
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
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
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
        };

        assert_eq!(config.agent_pane_col_limits(), Some((80, 100)));
    }
}
