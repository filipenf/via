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

    /// Review tool backend (`nvim` or `hunk`).
    #[arg(long = "review-backend")]
    pub review_backend: Option<crate::config::ReviewBackend>,

    #[command(subcommand)]
    pub command: Option<Command>,
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
        assert_eq!(
            cli.review_backend,
            Some(crate::config::ReviewBackend::Hunk)
        );
    }
}
