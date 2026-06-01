use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

const SKILL_MD: &str = include_str!("../../skills/via-editor/SKILL.md");

static SKILL_PATH: OnceLock<PathBuf> = OnceLock::new();

pub struct AgentCli {
    pub subcommand: AgentSubcommand,
}

pub enum AgentSubcommand {
    Skill { mode: SkillMode },
}

pub enum SkillMode {
    Path,
    Show,
}

impl AgentCli {
    pub fn parse(args: &[String]) -> Result<Self> {
        let Some(subcommand) = args.first().map(String::as_str) else {
            bail!("missing agent subcommand (skill)");
        };

        let subcommand = match subcommand {
            "skill" => {
                let mode = match args.get(1).map(String::as_str) {
                    None | Some("show") => SkillMode::Show,
                    Some("path") => SkillMode::Path,
                    Some(other) => bail!("unknown agent skill mode `{other}` (expected path or show)"),
                };
                AgentSubcommand::Skill { mode }
            }
            other => bail!("unknown agent subcommand `{other}`"),
        };

        Ok(Self { subcommand })
    }
}

pub fn run(command: AgentCli) -> Result<()> {
    match command.subcommand {
        AgentSubcommand::Skill { mode } => run_skill(mode),
    }
}

fn run_skill(mode: SkillMode) -> Result<()> {
    match mode {
        SkillMode::Path => {
            println!("{}", skill_path()?.display());
        }
        SkillMode::Show => {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(SKILL_MD.as_bytes())
                .context("write via editor skill to stdout")?;
            stdout.flush().context("flush stdout")?;
        }
    }
    Ok(())
}

fn skill_path() -> Result<PathBuf> {
    if let Some(path) = SKILL_PATH.get() {
        return Ok(path.clone());
    }

    let path = materialize_skill_file()?;
    let _ = SKILL_PATH.set(path.clone());
    Ok(path)
}

fn materialize_skill_file() -> Result<PathBuf> {
    let path = crate::config::runtime_base_dir().join("via-editor-skill.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create skill directory {}", parent.display()))?;
    }
    fs::write(&path, SKILL_MD)
        .with_context(|| format!("write via editor skill to {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_skill_show() {
        let command = AgentCli::parse(&["skill".to_string(), "show".to_string()]).unwrap();
        assert!(matches!(
            command.subcommand,
            AgentSubcommand::Skill {
                mode: SkillMode::Show
            }
        ));
    }

    #[test]
    fn skill_body_is_non_empty() {
        assert!(SKILL_MD.contains("via session diagnostics"));
    }
}
