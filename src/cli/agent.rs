use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::agent_skill::{self, SkillState};

#[derive(Subcommand)]
pub enum AgentCommand {
    /// via-editor agent skill (install, diagnostics workflow).
    Skill {
        #[command(subcommand)]
        command: Option<SkillCommand>,
    },
}

#[derive(Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillCommand {
    /// Print the skill to stdout (default).
    Show,
    /// Print the primary global install path.
    Path,
    /// Install or update the skill for the current agent family.
    Install,
    /// Show install paths and state.
    Status,
    /// Remove the skill from every known global location.
    Cleanup,
}

pub fn run(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Skill { command } => {
            run_skill(command.unwrap_or(SkillCommand::Show))
        }
    }
}

fn run_skill(mode: SkillCommand) -> Result<()> {
    match mode {
        SkillCommand::Path => {
            println!("{}", agent_skill::primary_skill_path(current_family())?.display());
        }
        SkillCommand::Show => {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(agent_skill::skill_body().as_bytes())
                .context("write via editor skill to stdout")?;
            stdout.flush().context("flush stdout")?;
        }
        SkillCommand::Install => {
            let family = current_family();
            let written = agent_skill::ensure_global_skill(family)?;
            if written.is_empty() {
                println!("via-editor skill already up to date ({family:?})");
            } else {
                for path in &written {
                    println!("installed {}", path.display());
                }
            }
        }
        SkillCommand::Status => {
            let agent_command = resolve_agent_command();
            let family = agent_skill::detect_agent_family(&agent_command);
            if agent_command.is_empty() {
                println!("agent family: {family:?} (VIA_AGENT not set; showing fallback paths)");
            } else {
                println!("agent family: {family:?} (from VIA_AGENT)");
            }
            for entry in agent_skill::skill_status(family)? {
                let label = match entry.state {
                    SkillState::Missing => "missing",
                    SkillState::Installed => "ok",
                    SkillState::Outdated => "outdated",
                    SkillState::Unreadable => "unreadable",
                };
                println!("{label}\t{}", entry.path.display());
            }
        }
        SkillCommand::Cleanup => {
            let removed = agent_skill::cleanup_global_skill()?;
            if removed.is_empty() {
                println!("via-editor skill not installed in any known location");
            } else {
                for path in &removed {
                    println!("removed {}", path.display());
                }
            }
        }
    }
    Ok(())
}

fn resolve_agent_command() -> String {
    std::env::var("VIA_AGENT").unwrap_or_default()
}

fn current_family() -> agent_skill::AgentFamily {
    agent_skill::detect_agent_family(&resolve_agent_command())
}
