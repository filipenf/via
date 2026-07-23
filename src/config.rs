use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

use anyhow::{Context, Result as AnyResult};
use serde::Deserialize;

/// Ensures the embedded Lua files are written to disk exactly once per process.
static LUA_ASSETS_INITIALIZED: OnceLock<()> = OnceLock::new();

/// Ephemeral per-process directory: `<data dir>/instances/<pid>/`.
///
/// Holds sockets, agent bus, logs, and the instance manifest. A separate top-level
/// `instances/` tree makes stale runtime dirs easy to prune.
pub fn instance_dir(pid: u32) -> PathBuf {
    via_data_dir().join("instances").join(pid.to_string())
}

/// Directory for sockets, the context bridge script, and other per-process files.
///
/// Set explicitly via `VIA_RUNTIME_ROOT` after detached bootstrap; otherwise
/// [`instance_dir`] for the current pid.
pub fn runtime_base_dir() -> PathBuf {
    env::var_os("VIA_RUNTIME_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| instance_dir(std::process::id()))
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

/// Declarative defaults for common spawned agent ids (`orchestrator`, `reviewer`, `coder`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPreset {
    #[serde(default)]
    pub role: Option<String>,
    /// Optional launch command override; otherwise resolved from the session primary agent.
    #[serde(default)]
    pub command: Option<String>,
    /// Optional default model slug for ACP sessions (e.g. `composer-2.5`).
    #[serde(default)]
    pub model: Option<String>,
}

/// Resolved spawn fields after applying `[agents.<id>]` presets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpawnPreset {
    pub role: Option<String>,
    pub command: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub nvim_command: String,
    /// Primary interactive agent (always launched as a PTY pane; must not end with `acp`).
    pub agent_command: Option<String>,
    /// Explicit ACP launch override for unknown agents (e.g. `claude-code-acp`).
    pub acp_agent: Option<String>,
    /// True when the configured agent can be launched in ACP form for spawned helpers
    /// (known-agent table or `acp_agent` override). Does not upgrade the primary pane.
    pub orchestration_enabled: bool,
    /// Agent pane width bounds in terminal columns (vertical split only).
    pub agent_pane_cols: Option<AgentPaneCols>,
    pub review_backend: ReviewBackend,
    /// Mouse wheel sensitivity multiplier; higher scrolls faster, lower slower.
    pub scroll_sensitivity: f32,
    pub nvim_socket_path: PathBuf,
    pub editor_socket_path: PathBuf,
    /// Per-process directory holding the agent registry and per-agent mailboxes.
    pub agents_dir: PathBuf,
    pub nvim_context_bridge_path: PathBuf,
    pub nvim_via_module_path: PathBuf,
    pub lsp_bridge_socket_path: PathBuf,
    pub working_directory: PathBuf,
    /// Optional local directory holding a user plugin (extra skills/agents/workflows),
    /// overlaid on top of the embedded base skills at install time.
    pub plugin_dir: Option<PathBuf>,
    /// Spawn presets keyed by agent id (built-ins merged with `via.conf` `[agents.*]`).
    pub agent_presets: HashMap<String, AgentPreset>,
}

#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    pub nvim: Option<String>,
    pub agent: Option<String>,
    /// Override ACP launch command for agents without a built-in mapping.
    pub acp_agent: Option<String>,
    pub agent_pane_cols: Option<AgentPaneCols>,
    pub review_backend: Option<ReviewBackend>,
    pub scroll_sensitivity: Option<f32>,
    pub plugin_dir: Option<String>,
    pub agent_presets: HashMap<String, AgentPreset>,
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
    acp_agent: Option<String>,
    agent_pane_cols: Option<AgentPaneCols>,
    review_backend: Option<ReviewBackend>,
    scroll_sensitivity: Option<f32>,
    plugin_dir: Option<String>,
    #[serde(default)]
    agents: HashMap<String, AgentPreset>,
}

impl From<FileConfig> for ConfigOverrides {
    fn from(config: FileConfig) -> Self {
        Self {
            nvim: config.nvim,
            agent: config.agent,
            acp_agent: config.acp_agent,
            agent_pane_cols: config.agent_pane_cols,
            review_backend: config.review_backend,
            scroll_sensitivity: config.scroll_sensitivity,
            plugin_dir: config.plugin_dir,
            agent_presets: config.agents,
        }
    }
}

impl ConfigOverrides {
    fn from_env() -> Self {
        Self {
            nvim: env::var("VIA_NVIM").ok(),
            agent: env::var("VIA_AGENT").ok(),
            acp_agent: env::var("VIA_ACP_AGENT").ok().filter(|s| !s.is_empty()),
            agent_pane_cols: env::var("VIA_AGENT_PANE_COLS")
                .ok()
                .and_then(|value| value.parse().ok()),
            review_backend: env::var("VIA_REVIEW_BACKEND")
                .ok()
                .and_then(|value| value.parse().ok()),
            scroll_sensitivity: env::var("VIA_SCROLL_SENSITIVITY")
                .ok()
                .and_then(|value| value.parse().ok()),
            plugin_dir: env::var("VIA_PLUGIN_DIR").ok().filter(|s| !s.is_empty()),
            agent_presets: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedUserConfig {
    nvim_command: String,
    agent_command: Option<String>,
    acp_agent: Option<String>,
    agent_pane_cols: AgentPaneCols,
    review_backend: ReviewBackend,
    scroll_sensitivity: f32,
    plugin_dir: Option<String>,
    agent_presets: HashMap<String, AgentPreset>,
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
    let acp_agent = cli
        .acp_agent
        .or(env.acp_agent)
        .or(file.acp_agent)
        .filter(|s| !s.is_empty());
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
    let plugin_dir = cli
        .plugin_dir
        .or(env.plugin_dir)
        .or(file.plugin_dir)
        .filter(|s| !s.is_empty());

    ResolvedUserConfig {
        nvim_command,
        agent_command,
        acp_agent,
        agent_pane_cols: agent_pane_cols.unwrap_or(AgentPaneCols {
            min: DEFAULT_AGENT_PANE_MIN_COLS,
            max: DEFAULT_AGENT_PANE_MAX_COLS,
        }),
        review_backend,
        scroll_sensitivity,
        plugin_dir,
        agent_presets: file.agent_presets,
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
    if let Some(agent) = &config.agent_command {
        reject_acp_primary_agent(agent)?;
    }
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
    if let Some(acp_agent) = &config.acp_agent {
        output.push_str(&format!(
            "acp_agent = \"{}\"\n",
            toml_escape_string(acp_agent)
        ));
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
    if let Some(dir) = &config.plugin_dir {
        output.push_str(&format!("plugin_dir = \"{}\"\n", toml_escape_string(dir)));
    }
    for (id, preset) in &config.agent_presets {
        output.push_str(&format!("\n[agents.{id}]\n"));
        if let Some(role) = &preset.role {
            output.push_str(&format!("role = \"{}\"\n", toml_escape_string(role)));
        }
        if let Some(command) = &preset.command {
            output.push_str(&format!("command = \"{}\"\n", toml_escape_string(command)));
        }
        if let Some(model) = &preset.model {
            output.push_str(&format!("model = \"{}\"\n", toml_escape_string(model)));
        }
    }
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

/// Reject `--agent "… acp"` — the primary pane is always a PTY; spawn orchestration via ACP.
pub fn reject_acp_primary_agent(agent: &str) -> AnyResult<()> {
    if is_acp_command(agent) {
        anyhow::bail!(
            "primary agent must be a PTY command (not ending with `acp`); \
             use e.g. `--agent opencode` and spawn an orchestrator with \
             `via agent spawn --id orchestrator --role orchestrator`, or set `acp_agent` for spawn resolution"
        );
    }
    Ok(())
}

impl Config {
    pub fn load(cli: ConfigOverrides) -> AnyResult<Self> {
        let user_config = resolve_user_config(cli)?;
        let agent_command = user_config.agent_command.clone();
        if let Some(agent) = agent_command.as_deref() {
            reject_acp_primary_agent(agent)?;
        }
        let orchestration_enabled = agent_command
            .as_deref()
            .map(|agent| resolve_agent_launch(agent, user_config.acp_agent.as_deref()).acp)
            .unwrap_or(false);
        let nvim_socket_path = env::var_os("VIA_NVIM_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_nvim_socket_path);
        let editor_socket_path = env::var_os("VIA_EDITOR_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(default_editor_socket_path);
        let agents_dir = env::var_os("VIA_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_agents_dir);
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
            agent_command,
            acp_agent: user_config.acp_agent,
            orchestration_enabled,
            agent_pane_cols: Some(user_config.agent_pane_cols),
            review_backend: user_config.review_backend,
            scroll_sensitivity: user_config.scroll_sensitivity,
            nvim_socket_path,
            editor_socket_path,
            agents_dir,
            nvim_context_bridge_path,
            nvim_via_module_path,
            lsp_bridge_socket_path,
            working_directory,
            plugin_dir: user_config.plugin_dir.map(PathBuf::from),
            agent_presets: merge_agent_presets(user_config.agent_presets),
        })
    }

    /// Fill missing spawn `role` / `command` from `[agents.<id>]` presets.
    pub fn apply_spawn_preset(
        &self,
        id: &str,
        role: Option<String>,
        command: Option<String>,
        model: Option<String>,
    ) -> SpawnPreset {
        let preset = self.agent_presets.get(id);
        SpawnPreset {
            role: role.or_else(|| preset.and_then(|p| p.role.clone())),
            command: command.or_else(|| preset.and_then(|p| p.command.clone())),
            model: model.or_else(|| preset.and_then(|p| p.model.clone())),
        }
    }

    /// Column bounds for the agent pane in vertical split mode.
    pub fn agent_pane_col_limits(&self) -> Option<(u16, u16)> {
        self.agent_command.as_ref()?;

        let cols = self.agent_pane_cols.unwrap_or(AgentPaneCols {
            min: DEFAULT_AGENT_PANE_MIN_COLS,
            max: DEFAULT_AGENT_PANE_MAX_COLS,
        });
        Some((cols.min, cols.max))
    }

    /// Resolve a spawn command: explicit override, else ACP form of the configured agent.
    pub fn resolve_spawn_command(&self, explicit: Option<&str>) -> ResolvedAgentLaunch {
        match explicit {
            Some(command) => resolve_agent_launch(command, self.acp_agent.as_deref()),
            None => match &self.agent_command {
                Some(command) => resolve_agent_launch(command, self.acp_agent.as_deref()),
                None => ResolvedAgentLaunch {
                    command: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                    acp: false,
                },
            },
        }
    }
}

/// Result of resolving a user-facing agent string to a launch command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentLaunch {
    pub command: String,
    pub acp: bool,
}

/// Bus id of the default interactive PTY agent pane (`--agent opencode`, etc.).
pub const PRIMARY_PTY_AGENT_ID: &str = "agent";

/// Bus id of the ACP orchestrator when orchestration is active.
pub const ORCHESTRATOR_AGENT_ID: &str = "orchestrator";

/// Reserved assignee id for the human operator. Not a pane — never spawned,
/// never appears in `via agent list`, never receives ACP prompts. Delivery to
/// `human` is the review-gate signal (when a task is in `review`) or a mailbox
/// notify to the primary `agent` pane (otherwise). Used by `via agent assign
/// --id human --task <tid>` and `via task review <id>` to explicitly hand work
/// to the user.
pub const HUMAN_ASSIGNEE_ID: &str = "human";

/// True for ids that are reserved roles, not spawnable agent panes. Rejecting
/// these in `via agent spawn` / `terminate` keeps the registry honest about
/// what's actually running.
pub fn is_reserved_agent_id(id: &str) -> bool {
    id == PRIMARY_PTY_AGENT_ID || id == HUMAN_ASSIGNEE_ID
}

/// Built-in spawn presets; `via.conf` `[agents.*]` entries override per field.
pub fn default_agent_presets() -> HashMap<String, AgentPreset> {
    fn preset(role: &str) -> AgentPreset {
        AgentPreset {
            role: Some(role.to_string()),
            command: None,
            model: None,
        }
    }
    HashMap::from([
        (ORCHESTRATOR_AGENT_ID.to_string(), preset("orchestrator")),
        ("reviewer".to_string(), preset("reviewer")),
        ("coder".to_string(), preset("coder")),
    ])
}

fn merge_agent_presets(file: HashMap<String, AgentPreset>) -> HashMap<String, AgentPreset> {
    let mut merged = default_agent_presets();
    for (id, preset) in file {
        let entry = merged.entry(id).or_default();
        if preset.role.is_some() {
            entry.role = preset.role;
        }
        if preset.command.is_some() {
            entry.command = preset.command;
        }
        if preset.model.is_some() {
            entry.model = preset.model;
        }
    }
    merged
}

/// Resolve a single agent token to an ACP launch command when possible.
pub fn resolve_agent_launch(agent: &str, acp_override: Option<&str>) -> ResolvedAgentLaunch {
    let agent = agent.trim();
    if agent.is_empty() {
        return ResolvedAgentLaunch {
            command: String::new(),
            acp: false,
        };
    }
    if is_acp_command(agent) {
        return ResolvedAgentLaunch {
            command: agent.to_string(),
            acp: true,
        };
    }
    if let Some(override_cmd) = acp_override.filter(|s| !s.is_empty()) {
        return ResolvedAgentLaunch {
            command: override_cmd.trim().to_string(),
            acp: true,
        };
    }
    if let Some(command) = known_acp_launch_for(agent) {
        return ResolvedAgentLaunch { command, acp: true };
    }
    ResolvedAgentLaunch {
        command: agent.to_string(),
        acp: false,
    }
}

/// Built-in mapping from bare agent binary → ACP launch command.
fn known_acp_launch_for(agent: &str) -> Option<String> {
    let parts: Vec<&str> = agent.split_whitespace().collect();
    if parts.len() != 1 {
        return None;
    }
    let first = parts[0];
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase();
    match name.as_str() {
        "opencode" | "cursor-agent" | "agent" => Some(format!("{first} acp")),
        _ => None,
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

/// True when an agent command string is an ACP launch (its last token is `acp`),
/// e.g. `opencode acp`, `cursor-agent acp`. Shared by config and runtime spawning so
/// PTY vs ACP classification has a single source of truth.
pub fn is_acp_command(command: &str) -> bool {
    command.split_whitespace().last() == Some("acp")
}

/// Per-instance agent bus directory (`registry.json`, `inbox/<id>/…`).
pub fn default_agents_dir() -> PathBuf {
    runtime_base_dir().join("agents")
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
        // Base Lua assets are always re-asserted (overwrite if content differs)
        // so updates to the embedded files propagate on restart. User
        // customizations live in the plugin_dir, not here.
        for (filename, content) in [
            (
                "context_bridge.lua",
                include_str!("../nvim/context_bridge.lua"),
            ),
            ("via.lua", include_str!("../nvim/via.lua")),
            ("via/tasks.lua", include_str!("../nvim/tasks.lua")),
            ("via/vcs.lua", include_str!("../nvim/vcs.lua")),
            ("via/path_match.lua", include_str!("../nvim/path_match.lua")),
        ] {
            let path = dir.join(filename);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
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
    fn is_acp_command_matches_acp_suffix() {
        assert!(is_acp_command("opencode acp"));
        assert!(is_acp_command("cursor-agent acp"));
        assert!(is_acp_command("acp"));
        assert!(!is_acp_command("opencode"));
        assert!(!is_acp_command("opencode acp --foo"));
        assert!(!is_acp_command(""));
    }

    #[test]
    fn reserved_agent_ids_are_primary_and_human() {
        assert!(is_reserved_agent_id(PRIMARY_PTY_AGENT_ID));
        assert!(is_reserved_agent_id(HUMAN_ASSIGNEE_ID));
        assert!(!is_reserved_agent_id("orchestrator"));
        assert!(!is_reserved_agent_id("reviewer"));
        assert!(!is_reserved_agent_id("coder"));
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
            acp_agent: None,
            agent_pane_cols: Some(AgentPaneCols { min: 70, max: 90 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(0.5),
            plugin_dir: None,
            agent_presets: HashMap::new(),
        };
        let env = ConfigOverrides {
            nvim: Some("env-nvim".to_string()),
            agent: Some("env-agent".to_string()),
            acp_agent: None,
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: Some(ReviewBackend::Hunk),
            scroll_sensitivity: Some(0.75),
            plugin_dir: None,
            agent_presets: HashMap::new(),
        };
        let cli = ConfigOverrides {
            nvim: Some("cli-nvim".to_string()),
            agent: Some("cli-agent".to_string()),
            acp_agent: None,
            agent_pane_cols: Some(AgentPaneCols { min: 90, max: 110 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(2.0),
            plugin_dir: Some("/home/user/my-via-plugin".to_string()),
            agent_presets: HashMap::new(),
        };

        let config = resolve_user_config_from_sources(cli, env, file);

        assert_eq!(config.nvim_command, "cli-nvim");
        assert_eq!(config.agent_command.as_deref(), Some("cli-agent"));
        assert_eq!(config.agent_pane_cols, AgentPaneCols { min: 90, max: 110 });
        assert_eq!(config.review_backend, ReviewBackend::Nvim);
        assert_eq!(config.scroll_sensitivity, 2.0);
        assert_eq!(
            config.plugin_dir.as_deref(),
            Some("/home/user/my-via-plugin")
        );
    }

    #[test]
    fn env_overrides_file_config() {
        let file = ConfigOverrides {
            nvim: Some("file-nvim".to_string()),
            agent: Some("file-agent".to_string()),
            acp_agent: None,
            agent_pane_cols: Some(AgentPaneCols { min: 70, max: 90 }),
            review_backend: Some(ReviewBackend::Nvim),
            scroll_sensitivity: Some(0.5),
            plugin_dir: None,
            agent_presets: HashMap::new(),
        };
        let env = ConfigOverrides {
            nvim: Some("env-nvim".to_string()),
            agent: Some("env-agent".to_string()),
            acp_agent: None,
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: Some(ReviewBackend::Hunk),
            scroll_sensitivity: Some(0.75),
            plugin_dir: None,
            agent_presets: HashMap::new(),
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
            acp_agent: None,
            agent_pane_cols: AgentPaneCols { min: 80, max: 120 },
            review_backend: ReviewBackend::Hunk,
            scroll_sensitivity: 1.5,
            plugin_dir: Some("/home/user/my-via-plugin".to_string()),
            agent_presets: HashMap::new(),
        });

        assert_eq!(
            output,
            concat!(
                "nvim = \"nvim-nightly\"\n",
                "agent = \"opencode acp\"\n",
                "agent_pane_cols = \"80:120\"\n",
                "review_backend = \"hunk\"\n",
                "scroll_sensitivity = 1.5\n",
                "plugin_dir = \"/home/user/my-via-plugin\"\n",
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
            acp_agent: None,
            orchestration_enabled: false,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: default_agent_presets(),
        };

        assert_eq!(config.agent_pane_col_limits(), Some((80, 100)));
    }

    #[test]
    fn agent_pane_col_limits_normalizes_bounds() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("agent".to_string()),
            acp_agent: None,
            orchestration_enabled: false,
            agent_pane_cols: Some(AgentPaneCols { min: 80, max: 100 }),
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: default_agent_presets(),
        };

        assert_eq!(config.agent_pane_col_limits(), Some((80, 100)));
    }

    #[test]
    fn resolve_agent_launch_known_binary_to_acp() {
        let resolved = resolve_agent_launch("opencode", None);
        assert_eq!(
            resolved,
            ResolvedAgentLaunch {
                command: "opencode acp".to_string(),
                acp: true,
            }
        );
    }

    #[test]
    fn resolve_agent_launch_acp_override_for_unknown() {
        let resolved = resolve_agent_launch("claude", Some("claude-code-acp"));
        assert_eq!(
            resolved,
            ResolvedAgentLaunch {
                command: "claude-code-acp".to_string(),
                acp: true,
            }
        );
    }

    #[test]
    fn resolve_agent_launch_unknown_without_override_stays_pty() {
        let resolved = resolve_agent_launch("claude", None);
        assert_eq!(
            resolved,
            ResolvedAgentLaunch {
                command: "claude".to_string(),
                acp: false,
            }
        );
    }

    #[test]
    fn resolve_agent_launch_passes_through_acp_command() {
        let resolved = resolve_agent_launch("cursor-agent acp", None);
        assert_eq!(
            resolved,
            ResolvedAgentLaunch {
                command: "cursor-agent acp".to_string(),
                acp: true,
            }
        );
    }

    #[test]
    fn orchestration_available_for_known_agent_without_upgrading_primary() {
        let user = ResolvedUserConfig {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            agent_pane_cols: AgentPaneCols { min: 80, max: 100 },
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            plugin_dir: None,
            agent_presets: HashMap::new(),
        };
        let orchestration = user
            .agent_command
            .as_deref()
            .map(|agent| resolve_agent_launch(agent, user.acp_agent.as_deref()).acp)
            .unwrap_or(false);
        assert!(orchestration);
        assert_eq!(user.agent_command.as_deref(), Some("opencode"));
    }

    #[test]
    fn orchestration_unavailable_without_acp_mapping() {
        let orchestration = resolve_agent_launch("claude", None).acp;
        assert!(!orchestration);
    }

    #[test]
    fn resolve_spawn_command_defaults_to_acp_for_known_agent() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            orchestration_enabled: true,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: default_agent_presets(),
        };
        let launch = config.resolve_spawn_command(None);
        assert_eq!(launch.command, "opencode acp");
        assert!(launch.acp);
    }

    #[test]
    fn rejects_acp_primary_agent_command() {
        let err = reject_acp_primary_agent("opencode acp").unwrap_err();
        assert!(
            err.to_string()
                .contains("primary agent must be a PTY command")
        );
        reject_acp_primary_agent("opencode").unwrap();
    }

    #[test]
    fn parses_agent_presets_from_file_config() {
        let config: FileConfig = toml::from_str(
            r#"
[agents.reviewer]
role = "reviewer"
command = "cursor-agent acp"

[agents.coder]
role = "implementer"
model = "composer-2.5"
"#,
        )
        .unwrap();

        assert_eq!(
            config.agents.get("reviewer"),
            Some(&AgentPreset {
                role: Some("reviewer".to_string()),
                command: Some("cursor-agent acp".to_string()),
                model: None,
            })
        );
        assert_eq!(
            config.agents.get("coder"),
            Some(&AgentPreset {
                role: Some("implementer".to_string()),
                command: None,
                model: Some("composer-2.5".to_string()),
            })
        );
    }

    #[test]
    fn merge_agent_presets_file_model_merges_with_builtin_role() {
        let mut file = HashMap::new();
        file.insert(
            "coder".to_string(),
            AgentPreset {
                role: None,
                command: None,
                model: Some("composer-2.5".to_string()),
            },
        );
        let merged = merge_agent_presets(file);
        let coder = merged.get("coder").expect("coder preset");
        assert_eq!(coder.role.as_deref(), Some("coder"));
        assert_eq!(coder.model.as_deref(), Some("composer-2.5"));
    }

    #[test]
    fn file_config_model_reaches_apply_spawn_preset() {
        let file: FileConfig = toml::from_str(
            r#"
[agents.coder]
model = "composer-2.5"
"#,
        )
        .unwrap();
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            orchestration_enabled: true,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: merge_agent_presets(file.agents),
        };

        let preset = config.apply_spawn_preset("coder", None, None, None);
        assert_eq!(preset.model.as_deref(), Some("composer-2.5"));
    }

    #[test]
    fn apply_spawn_preset_fills_builtin_role() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            orchestration_enabled: true,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: default_agent_presets(),
        };

        let preset = config.apply_spawn_preset("reviewer", None, None, None);
        assert_eq!(preset.role.as_deref(), Some("reviewer"));
        assert_eq!(preset.command, None);
        assert_eq!(preset.model, None);

        let launch = config.resolve_spawn_command(preset.command.as_deref());
        assert_eq!(launch.command, "opencode acp");
        assert!(launch.acp);
    }

    #[test]
    fn file_agent_preset_overrides_builtin_command() {
        let mut file = HashMap::new();
        file.insert(
            "reviewer".to_string(),
            AgentPreset {
                role: None,
                command: Some("claude-code-acp".to_string()),
                model: Some("claude-sonnet".to_string()),
            },
        );
        let presets = merge_agent_presets(file);
        assert_eq!(
            presets.get("reviewer"),
            Some(&AgentPreset {
                role: Some("reviewer".to_string()),
                command: Some("claude-code-acp".to_string()),
                model: Some("claude-sonnet".to_string()),
            })
        );
    }

    #[test]
    fn apply_spawn_preset_includes_model_from_preset() {
        let mut presets = default_agent_presets();
        presets.insert(
            "coder".to_string(),
            AgentPreset {
                role: Some("coder".to_string()),
                command: None,
                model: Some("composer-2.5".to_string()),
            },
        );
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            orchestration_enabled: true,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: presets,
        };

        let preset = config.apply_spawn_preset("coder", None, None, None);
        assert_eq!(preset.model.as_deref(), Some("composer-2.5"));
    }

    #[test]
    fn apply_spawn_preset_explicit_model_overrides_preset() {
        let mut presets = default_agent_presets();
        presets.insert(
            "coder".to_string(),
            AgentPreset {
                role: Some("coder".to_string()),
                command: None,
                model: Some("from-preset".to_string()),
            },
        );
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            orchestration_enabled: true,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: presets,
        };

        let preset = config.apply_spawn_preset("coder", None, None, Some("from-cli".to_string()));
        assert_eq!(preset.model.as_deref(), Some("from-cli"));
    }

    #[test]
    fn renders_agent_preset_model_in_user_config() {
        let mut agent_presets = HashMap::new();
        agent_presets.insert(
            "coder".to_string(),
            AgentPreset {
                role: Some("coder".to_string()),
                command: None,
                model: Some("composer-2.5".to_string()),
            },
        );
        let output = render_user_config(&ResolvedUserConfig {
            nvim_command: "nvim".to_string(),
            agent_command: Some("opencode".to_string()),
            acp_agent: None,
            agent_pane_cols: AgentPaneCols { min: 80, max: 100 },
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            plugin_dir: None,
            agent_presets,
        });

        assert!(output.contains("[agents.coder]"));
        assert!(output.contains("model = \"composer-2.5\""));
    }

    #[test]
    fn load_rejects_acp_primary_agent() {
        let err = Config::load(ConfigOverrides {
            agent: Some("opencode acp".to_string()),
            ..ConfigOverrides::default()
        })
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("primary agent must be a PTY command")
        );
    }
}
