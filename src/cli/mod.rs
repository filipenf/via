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
    /// Open a file in Neovim and exit (requires a running via instance with a matching socket).
    #[arg(long)]
    pub open: Option<String>,

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

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn config_overrides(&self) -> crate::config::ConfigOverrides {
        crate::config::ConfigOverrides {
            nvim: self.nvim.clone(),
            agent: self.agent.clone(),
            acp_agent: self.acp_agent.clone(),
            agent_pane_cols: self.agent_pane_cols,
            review_backend: self.review_backend,
            scroll_sensitivity: self.scroll_sensitivity,
            plugin_dir: self.plugin_dir.clone(),
            agent_presets: Default::default(),
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
                command: AgentCommand::Spawn { id, role, command: None },
            }) if id == "reviewer" && role.as_deref() == Some("reviewer")
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
