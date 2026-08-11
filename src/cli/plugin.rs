use anyhow::Result;
use clap::Subcommand;

use crate::plugin::{self, SkillState};

#[derive(Subcommand)]
pub enum PluginCommand {
    /// Show whether core via skills are present in known agent skill roots.
    Status,
    /// Print known agent skill roots (where skills CLIs / manual installs place files).
    Path,
}

pub fn run(command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Status => run_status(),
        PluginCommand::Path => run_path(),
    }
}

fn run_status() -> Result<()> {
    let agent_command = std::env::var("VIA_AGENT").unwrap_or_default();
    if agent_command.is_empty() {
        println!("session agent: (VIA_AGENT not set)");
    } else {
        let family = plugin::detect_agent_family(&agent_command);
        println!("session agent: {} (from VIA_AGENT)", family.label());
        let missing = plugin::missing_core_skills(family)?;
        if missing.is_empty() {
            println!("session core skills: ok");
        } else {
            println!("session core skills missing: {}", missing.join(", "));
            println!("hint: see README — install via npx skills, skills-rs, or manual copy");
        }
    }
    for entry in plugin::status_all()? {
        let label = match entry.state {
            SkillState::Missing => "missing",
            SkillState::Installed => "ok",
            SkillState::Unreadable => "unreadable",
        };
        println!("{label}\t{}\t{}", entry.name, entry.path.display());
    }
    Ok(())
}

fn run_path() -> Result<()> {
    for root in plugin::all_known_roots() {
        println!("{}", root.display());
    }
    Ok(())
}
