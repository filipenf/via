use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::plugin::{self, SkillState};

#[derive(Subcommand)]
pub enum PluginCommand {
    /// Install skills into every available agent on this host (cursor, claude, opencode, …).
    Install {
        /// Directory containing skills (via checkout, plugin root with `skills/`, or the `skills/` dir itself).
        #[arg(long = "from")]
        from: Option<PathBuf>,
        /// Overwrite existing skill directories (default: skip so forks survive).
        #[arg(long)]
        force: bool,
    },
    /// Show install paths and state of the core skills across all known agent roots.
    Status,
    /// Print skill roots that install would target on this host.
    Path,
    /// Remove the core skills from every known location.
    Cleanup,
}

pub fn run(command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install { from, force } => run_install(from, force),
        PluginCommand::Status => run_status(),
        PluginCommand::Path => run_path(),
        PluginCommand::Cleanup => run_cleanup(),
    }
}

fn run_install(from: Option<PathBuf>, force: bool) -> Result<()> {
    let source = resolve_source(from)?;
    println!("source {}", source.display());
    let report = plugin::install(&source, force)?;
    if report.fallback_all_roots {
        println!("agents: none detected; installing into all known skill roots");
    } else {
        let labels: Vec<_> = report.families.iter().map(|f| f.label()).collect();
        println!("agents: {}", labels.join(", "));
    }
    for root in &report.roots {
        println!("root {}", root.display());
    }
    if report.written.is_empty() {
        println!("via plugin already up to date; use --force to overwrite existing skills");
    } else {
        for path in &report.written {
            println!("installed {}", path.display());
        }
    }
    Ok(())
}

fn resolve_source(from: Option<PathBuf>) -> Result<PathBuf> {
    let from = from
        .or_else(plugin_dir_from_env)
        .or_else(plugin::default_skills_source);
    plugin::resolve_install_source(from.as_deref())
}

fn run_status() -> Result<()> {
    let agent_command = resolve_agent_command();
    if agent_command.is_empty() {
        println!("session agent: (VIA_AGENT not set)");
    } else {
        let family = plugin::detect_agent_family(&agent_command);
        println!("session agent: {} (from VIA_AGENT)", family.label());
    }
    let available = plugin::available_families();
    if available.is_empty() {
        println!("host agents: none detected");
    } else {
        let labels: Vec<_> = available.iter().map(|f| f.label()).collect();
        println!("host agents: {}", labels.join(", "));
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
    let (families, roots) = plugin::install_roots();
    if families.is_empty() {
        println!("# no agents detected; showing all known roots");
    } else {
        let labels: Vec<_> = families.iter().map(|f| f.label()).collect();
        println!("# agents: {}", labels.join(", "));
    }
    for root in roots {
        println!("{}", root.display());
    }
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
