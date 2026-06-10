mod agent;
mod session;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub use agent::AgentCommand;
pub use session::SessionCommand;

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

    /// Agent pane columns as one value or min:max, for example `100` or `80:120`.
    #[arg(long = "agent-pane-cols")]
    pub agent_pane_cols: Option<crate::config::AgentPaneCols>,

    /// Review tool backend (`nvim` or `hunk`).
    #[arg(long = "review-backend")]
    pub review_backend: Option<crate::config::ReviewBackend>,

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
            agent_pane_cols: self.agent_pane_cols,
            review_backend: self.review_backend,
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
}

pub async fn run(command: Command) -> Result<()> {
    match command {
        Command::Session { command } => session::run(command).await,
        Command::Agent { command } => agent::run(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::agent::SkillCommand;
    use clap::Parser;
    use std::path::PathBuf;

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
            }) if path == PathBuf::from("src/main.rs")
        ));
    }

    #[test]
    fn parses_agent_skill_show() {
        let cli = Cli::try_parse_from(["via", "agent", "skill", "show"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Skill {
                    command: Some(SkillCommand::Show),
                },
            })
        ));
    }

    #[test]
    fn parses_agent_skill_default() {
        let cli = Cli::try_parse_from(["via", "agent", "skill"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Skill { command: None },
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
