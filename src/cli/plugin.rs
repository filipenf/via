use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::plugin::{self, AgentFamily, SkillState};

#[derive(Subcommand)]
pub enum PluginCommand {
    /// Install or update the plugin skills for the current agent family.
    Install {
        /// Local plugin directory to overlay (defaults to `VIA_PLUGIN_DIR`).
        #[arg(long = "from")]
        from: Option<PathBuf>,
    },
    /// Show install paths and state of the base skills.
    Status,
    /// Print the primary install root for the current agent family.
    Path,
    /// Remove the base skills from every known location.
    Cleanup,
}

pub fn run(command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install { from } => run_install(from),
        PluginCommand::Status => run_status(),
        PluginCommand::Path => run_path(),
        PluginCommand::Cleanup => run_cleanup(),
    }
}

fn run_install(from: Option<PathBuf>) -> Result<()> {
    let family = current_family();
    let plugin_dir = from.or_else(plugin_dir_from_env);
    let written = plugin::install(family, plugin_dir.as_deref())?;
    if written.is_empty() {
        println!("via plugin already up to date ({family:?})");
    } else {
        for path in &written {
            println!("installed {}", path.display());
        }
    }
    Ok(())
}

fn run_status() -> Result<()> {
    let agent_command = resolve_agent_command();
    let family = plugin::detect_agent_family(&agent_command);
    if agent_command.is_empty() {
        println!("agent family: {family:?} (VIA_AGENT not set; showing fallback paths)");
    } else {
        println!("agent family: {family:?} (from VIA_AGENT)");
    }
    for entry in plugin::status(family)? {
        let label = match entry.state {
            SkillState::Missing => "missing",
            SkillState::Installed => "ok",
            SkillState::Outdated => "outdated",
            SkillState::Unreadable => "unreadable",
        };
        println!("{label}\t{}", entry.path.display());
    }
    Ok(())
}

fn run_path() -> Result<()> {
    println!("{}", plugin::primary_root(current_family())?.display());
    Ok(())
}

fn run_cleanup() -> Result<()> {
    let removed = plugin::cleanup()?;
    if removed.is_empty() {
        println!("via plugin not installed in any known location");
    } else {
        for path in &removed {
            println!("removed {}", path.display());
        }
    }
    Ok(())
}

fn plugin_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("VIA_PLUGIN_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

fn resolve_agent_command() -> String {
    std::env::var("VIA_AGENT").unwrap_or_default()
}

fn current_family() -> AgentFamily {
    plugin::detect_agent_family(&resolve_agent_command())
}
