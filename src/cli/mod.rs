mod agent;
mod plugin;
mod session;
mod task;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub use agent::AgentCommand;
pub use plugin::PluginCommand;
pub use session::SessionCommand;
pub use task::TaskCommand;

/// via — bridge Neovim and AI agents.
#[derive(Parser)]
#[command(name = "via", version, about, propagate_version = true)]
pub struct Cli {
    /// Neovim command to run.
    #[arg(long = "nvim")]
    pub nvim: Option<String>,

    /// Agent command to run.
    #[arg(long = "agent")]
    pub agent: Option<String>,

    /// ACP launch override for agents without a built-in mapping (e.g. `claude-code-acp`).
    #[arg(long = "acp-agent")]
    pub acp_agent: Option<String>,

    /// Agent pane columns as one value or min:max, for example `100` or `80:120`.
    #[arg(long = "agent-pane-cols")]
    pub agent_pane_cols: Option<crate::config::AgentPaneCols>,

    /// Review tool backend (`nvim` or `hunk`).
    #[arg(long = "review-backend")]
    pub review_backend: Option<crate::config::ReviewBackend>,

    /// Mouse wheel sensitivity multiplier (higher scrolls faster).
    #[arg(long = "scroll-sensitivity")]
    pub scroll_sensitivity: Option<f32>,

    /// Local directory holding a user plugin (extra skills/agents/workflows).
    #[arg(long = "plugin-dir")]
    pub plugin_dir: Option<String>,

    /// Write the resolved user-facing configuration to via.conf before running.
    #[arg(long = "persist")]
    pub persist: bool,

    /// Run the ACP agent TUI (PTY display + input surface). Does not start the GUI.
    ///
    /// Hosted by via in a PTY pane; the mediator keeps ACP client ownership. Prefer
    /// spawning `current_exe()` with this flag rather than a separate binary.
    #[arg(long = "acp-tui")]
    pub acp_tui: bool,

    /// ACP TUI: agent id (defaults to `$VIA_AGENT_ID`, then `agent`).
    #[arg(long = "agent-id")]
    pub agent_id: Option<String>,

    /// ACP TUI: role label for the header (defaults to `$VIA_AGENT_ROLE`, then agent id).
    #[arg(long)]
    pub role: Option<String>,

    /// ACP TUI: seed a demo transcript (standalone smoke without a host).
    #[arg(long)]
    pub demo: bool,

    /// ACP TUI: hide the prompt row (output / scrollback only).
    #[arg(long = "no-input")]
    pub no_input: bool,

    /// ACP TUI: control-plane Unix socket (defaults to `$VIA_ACP_UI_SOCKET`).
    #[arg(long = "socket")]
    pub socket: Option<std::path::PathBuf>,

    /// Run the remote helper daemon (detachable PTY session authority). Does not start the GUI.
    #[arg(long = "remote-serve")]
    pub remote_serve: bool,

    /// Bridge stdio to the remote helper control socket (SSH pipe target). Does not start the GUI.
    #[arg(long = "remote-proxy")]
    pub remote_proxy: bool,

    /// Keep `--remote-serve` in the foreground (no double-fork daemonize).
    #[arg(long = "remote-foreground")]
    pub remote_foreground: bool,

    /// Control socket for `--remote-serve` / `--remote-proxy`
    /// (default: `$XDG_DATA_HOME/via/remote/control.sock`).
    #[arg(long = "remote-socket")]
    pub remote_socket: Option<std::path::PathBuf>,

    /// Connect the local GUI to a remote SSH host (`local` / `VIA_REMOTE_SOCKET` for unix).
    #[arg(long = "remote", value_name = "HOST")]
    pub remote: Option<String>,

    /// Remote working directory when using `--remote` (spike: `via --remote host --cwd /path`).
    #[arg(long = "cwd", value_name = "DIR")]
    pub cwd: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn config_overrides(&self) -> crate::config::ConfigOverrides {
        let remote = self.remote.as_ref().map(|host| crate::config::RemoteMode {
            host: host.clone(),
            cwd: self.cwd.clone(),
            socket: self.remote_socket.clone(),
        });
        crate::config::ConfigOverrides {
            nvim: self.nvim.clone(),
            agent: self.agent.clone(),
            acp_agent: self.acp_agent.clone(),
            agent_pane_cols: self.agent_pane_cols,
            review_backend: self.review_backend,
            scroll_sensitivity: self.scroll_sensitivity,
            plugin_dir: self.plugin_dir.clone(),
            agent_presets: Default::default(),
            auto_approve: Default::default(),
            remote,
        }
    }

    /// Options for [`crate::acp_tui::run`] when `--acp-tui` is set.
    pub fn acp_tui_args(&self) -> crate::acp_tui::Args {
        crate::acp_tui::Args {
            agent_id: self.agent_id.clone(),
            role: self.role.clone(),
            demo: self.demo,
            no_input: self.no_input,
            socket: self.socket.clone(),
        }
    }

    /// Options for [`crate::remote::run`] when `--remote-serve` / `--remote-proxy` is set.
    pub fn remote_args(&self) -> crate::remote::Args {
        crate::remote::Args {
            serve: self.remote_serve,
            proxy: self.remote_proxy,
            foreground: self.remote_foreground,
            socket: self.remote_socket.clone(),
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
}

pub async fn run(command: Command) -> Result<()> {
    match command {
        Command::Session { command } => session::run(command).await,
        Command::Agent { command } => agent::run(command),
        Command::Plugin { command } => plugin::run(command),
        Command::Task { command } => task::run(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn parses_session_list_json() {
        let cli = Cli::try_parse_from(["via", "session", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Session {
                command: SessionCommand::List { json: true },
            })
        ));
    }

    #[test]
    fn parses_session_diagnostics() {
        let cli = Cli::try_parse_from([
            "via",
            "session",
            "diagnostics",
            "--file",
            "src/main.rs",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Session {
                command: SessionCommand::Diagnostics {
                    json: true,
                    file: Some(path),
                },
            }) if path == Path::new("src/main.rs")
        ));
    }

    #[test]
    fn parses_session_refresh() {
        let cli = Cli::try_parse_from([
            "via",
            "session",
            "refresh",
            "--file",
            "src/main.rs",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Session {
                command: SessionCommand::Refresh {
                    json: true,
                    file: Some(path),
                },
            }) if path == Path::new("src/main.rs")
        ));
    }

    #[test]
    fn parses_agent_list_json() {
        let cli = Cli::try_parse_from(["via", "agent", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::List { json: true },
            })
        ));
    }

    #[test]
    fn parses_agent_spawn() {
        let cli = Cli::try_parse_from([
            "via", "agent", "spawn", "--id", "reviewer", "--role", "reviewer",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Spawn {
                    id,
                    role,
                    command: None,
                    model: None,
                },
            }) if id == "reviewer" && role.as_deref() == Some("reviewer")
        ));
    }

    #[test]
    fn parses_agent_spawn_with_model() {
        let cli = Cli::try_parse_from([
            "via",
            "agent",
            "spawn",
            "--id",
            "coder",
            "--model",
            "composer-2.5",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Spawn {
                    id,
                    model,
                    role: None,
                    command: None,
                },
            }) if id == "coder" && model.as_deref() == Some("composer-2.5")
        ));
    }

    #[test]
    fn parses_agent_assign_with_model() {
        let cli = Cli::try_parse_from([
            "via",
            "agent",
            "assign",
            "--id",
            "coder",
            "--model",
            "composer-2.5",
            "-m",
            "implement",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Assign {
                    id,
                    model,
                    message,
                    role: None,
                    command: None,
                    task: None,
                    no_focus: false,
                },
            }) if id == "coder"
                && model.as_deref() == Some("composer-2.5")
                && message == "implement"
        ));
    }

    #[test]
    fn parses_agent_send() {
        let cli = Cli::try_parse_from([
            "via",
            "agent",
            "send",
            "--to",
            "reviewer",
            "-m",
            "hello",
            "--no-focus",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Send {
                    to: Some(to),
                    message,
                    no_focus: true,
                    no_notify: false,
                },
            }) if to == "reviewer" && message == "hello"
        ));
    }

    #[test]
    fn parses_agent_assign_with_task() {
        let cli = Cli::try_parse_from([
            "via",
            "agent",
            "assign",
            "--id",
            "reviewer",
            "--role",
            "reviewer",
            "-m",
            "review this",
            "--task",
            "p4-assign-cmd",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Assign {
                    id,
                    role,
                    command: None,
                    model: None,
                    message,
                    task: Some(tid),
                    no_focus: false,
                },
            }) if id == "reviewer"
                && role.as_deref() == Some("reviewer")
                && message == "review this"
                && tid == "p4-assign-cmd"
        ));
    }

    #[test]
    fn parses_agent_assign_to_human() {
        let cli =
            Cli::try_parse_from(["via", "agent", "assign", "--id", "human", "-m", "your turn"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Assign {
                    id,
                    role: None,
                    command: None,
                    model: None,
                    message,
                    task: None,
                    no_focus: false,
                },
            }) if id == "human" && message == "your turn"
        ));
    }

    #[test]
    fn parses_agent_inbox() {
        let cli = Cli::try_parse_from(["via", "agent", "inbox", "--peek", "--wait", "30"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Inbox {
                    json: false,
                    peek: true,
                    wait: Some(30),
                },
            })
        ));
    }

    #[test]
    fn parses_plugin_install_from() {
        let cli =
            Cli::try_parse_from(["via", "plugin", "install", "--from", "/tmp/my-plugin"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Install { from: Some(path) },
            }) if path == Path::new("/tmp/my-plugin")
        ));
    }

    #[test]
    fn parses_plugin_status_default() {
        let cli = Cli::try_parse_from(["via", "plugin", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Status,
            })
        ));
    }

    #[test]
    fn parses_review_backend_flag() {
        let cli = Cli::try_parse_from(["via", "--review-backend", "hunk"]).unwrap();
        assert_eq!(cli.review_backend, Some(crate::config::ReviewBackend::Hunk));
    }

    #[test]
    fn parses_acp_tui_flags() {
        let cli = Cli::try_parse_from([
            "via",
            "--acp-tui",
            "--demo",
            "--agent-id",
            "coder",
            "--role",
            "reviewer",
            "--socket",
            "/tmp/acp-ui.sock",
            "--no-input",
        ])
        .unwrap();
        assert!(cli.acp_tui);
        assert!(cli.demo);
        assert!(cli.no_input);
        assert_eq!(cli.agent_id.as_deref(), Some("coder"));
        assert_eq!(cli.role.as_deref(), Some("reviewer"));
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/tmp/acp-ui.sock"))
        );
        let args = cli.acp_tui_args();
        assert!(args.demo);
        assert!(args.no_input);
        assert_eq!(args.agent_id.as_deref(), Some("coder"));
    }

    #[test]
    fn parses_remote_helper_flags() {
        let cli = Cli::try_parse_from([
            "via",
            "--remote-serve",
            "--remote-foreground",
            "--remote-socket",
            "/tmp/via-remote.sock",
        ])
        .unwrap();
        assert!(cli.remote_serve);
        assert!(cli.remote_foreground);
        assert_eq!(
            cli.remote_socket.as_deref(),
            Some(std::path::Path::new("/tmp/via-remote.sock"))
        );
        let args = cli.remote_args();
        assert!(args.serve);
        assert!(args.foreground);
        assert!(args.wants_early_dispatch());

        let proxy = Cli::try_parse_from(["via", "--remote-proxy"]).unwrap();
        assert!(proxy.remote_proxy);
        assert!(proxy.remote_args().wants_early_dispatch());

        let host = Cli::try_parse_from(["via", "--remote", "codespace"]).unwrap();
        assert_eq!(host.remote.as_deref(), Some("codespace"));
        assert!(!host.remote_args().wants_early_dispatch());
        let overrides = host.config_overrides();
        assert_eq!(
            overrides.remote.as_ref().map(|r| r.host.as_str()),
            Some("codespace")
        );

        let with_cwd = Cli::try_parse_from([
            "via",
            "--remote",
            "local",
            "--cwd",
            "/tmp/proj",
            "--remote-socket",
            "/tmp/c.sock",
        ])
        .unwrap();
        let remote = with_cwd.config_overrides().remote.unwrap();
        assert_eq!(remote.host, "local");
        assert_eq!(
            remote.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/proj"))
        );
        assert_eq!(
            remote.socket.as_deref(),
            Some(std::path::Path::new("/tmp/c.sock"))
        );
    }

    #[test]
    fn parses_user_config_flags() {
        let cli = Cli::try_parse_from([
            "via",
            "--nvim",
            "nvim-nightly",
            "--agent",
            "opencode acp",
            "--agent-pane-cols",
            "80:120",
            "--persist",
        ])
        .unwrap();

        assert_eq!(cli.nvim.as_deref(), Some("nvim-nightly"));
        assert_eq!(cli.agent.as_deref(), Some("opencode acp"));
        assert_eq!(
            cli.agent_pane_cols,
            Some(crate::config::AgentPaneCols { min: 80, max: 120 })
        );
        assert!(cli.persist);
    }
}
