use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SKILL_NAME: &str = "via-editor";
const SKILL_FILE: &str = "SKILL.md";
const SKILL_MD: &str = include_str!("../skills/via-editor/SKILL.md");

// Skill roots under `$HOME`. Each gets `/<SKILL_NAME>` appended to form an install directory.
const AGENTS_ROOT: &[&str] = &[".agents", "skills"];
const CONFIG_AGENTS_ROOT: &[&str] = &[".config", "agents", "skills"];
const CLAUDE_ROOT: &[&str] = &[".claude", "skills"];
const CURSOR_ROOT: &[&str] = &[".cursor", "skills"];
const OPENCODE_ROOT: &[&str] = &[".config", "opencode", "skills"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFamily {
    Cursor,
    OpenCode,
    Claude,
    Crush,
    Unknown,
}

/// Infer the agent runtime from `VIA_AGENT` (or similar) command text.
pub fn detect_agent_family(agent_command: &str) -> AgentFamily {
    let lower = agent_command.to_ascii_lowercase();
    if lower.contains("cursor") {
        AgentFamily::Cursor
    } else if lower.contains("opencode") {
        AgentFamily::OpenCode
    } else if lower.contains("crush") {
        AgentFamily::Crush
    } else if lower.contains("claude") {
        AgentFamily::Claude
    } else {
        AgentFamily::Unknown
    }
}

/// User-level skill directories where `via-editor/SKILL.md` should exist for this agent.
fn global_skill_dirs(family: AgentFamily) -> Vec<PathBuf> {
    let home = home_dir();
    let dir = |root: &[&str]| home.join(root.join("/")).join(SKILL_NAME);

    match family {
        AgentFamily::Cursor => vec![dir(CURSOR_ROOT), dir(AGENTS_ROOT)],
        AgentFamily::OpenCode => vec![dir(OPENCODE_ROOT), dir(AGENTS_ROOT), dir(CLAUDE_ROOT)],
        AgentFamily::Claude => vec![dir(CLAUDE_ROOT)],
        AgentFamily::Crush => vec![dir(CONFIG_AGENTS_ROOT)],
        AgentFamily::Unknown => vec![dir(AGENTS_ROOT), dir(CONFIG_AGENTS_ROOT), dir(CLAUDE_ROOT)],
    }
}

/// Write the embedded skill into each global directory for the given agent family.
/// Skips paths that already contain identical content.
pub fn ensure_global_skill(family: AgentFamily) -> Result<Vec<PathBuf>> {
    let body = skill_body();
    let mut written = Vec::new();

    for dir in global_skill_dirs(family) {
        let path = dir.join(SKILL_FILE);
        if !needs_update(&path, body)? {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create skill directory {}", parent.display()))?;
        }
        fs::write(&path, body).with_context(|| format!("write skill to {}", path.display()))?;
        written.push(path);
    }

    Ok(written)
}

pub fn skill_body() -> &'static str {
    SKILL_MD
}

pub fn primary_skill_path(family: AgentFamily) -> Result<PathBuf> {
    global_skill_dirs(family)
        .into_iter()
        .next()
        .map(|dir| dir.join(SKILL_FILE))
        .context("HOME is not set; cannot resolve global skill path")
}

/// Every global `via-editor` directory across all supported agent families.
fn all_known_skill_dirs() -> Vec<PathBuf> {
    let families = [
        AgentFamily::Cursor,
        AgentFamily::OpenCode,
        AgentFamily::Claude,
        AgentFamily::Crush,
        AgentFamily::Unknown,
    ];
    let mut dirs = Vec::new();
    for family in families {
        for dir in global_skill_dirs(family) {
            if !dirs.iter().any(|existing| existing == &dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Remove `via-editor` from every known global skill location.
pub fn cleanup_global_skill() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();

    for dir in all_known_skill_dirs() {
        let path = dir.join(SKILL_FILE);
        if !path.exists() {
            continue;
        }
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        removed.push(path);

        if dir.is_dir() && is_dir_empty(&dir)? {
            fs::remove_dir(&dir)
                .with_context(|| format!("remove empty skill directory {}", dir.display()))?;
        }
    }

    Ok(removed)
}

pub fn skill_status(family: AgentFamily) -> Result<Vec<SkillInstallStatus>> {
    let body = skill_body().as_bytes();

    Ok(global_skill_dirs(family)
        .into_iter()
        .map(|dir| {
            let path = dir.join(SKILL_FILE);
            let state = if !path.exists() {
                SkillState::Missing
            } else {
                match fs::read(&path) {
                    Ok(existing) if existing == body => SkillState::Installed,
                    Ok(_) => SkillState::Outdated,
                    Err(_) => SkillState::Unreadable,
                }
            };
            SkillInstallStatus { path, state }
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillState {
    Missing,
    Installed,
    Outdated,
    Unreadable,
}

#[derive(Debug, Clone)]
pub struct SkillInstallStatus {
    pub path: PathBuf,
    pub state: SkillState,
}

fn needs_update(path: &Path, body: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    let existing = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(existing != body.as_bytes())
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::via_data_dir)
}

fn is_dir_empty(dir: &Path) -> Result<bool> {
    Ok(fs::read_dir(dir)?.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cursor() {
        assert_eq!(detect_agent_family("cursor-agent acp"), AgentFamily::Cursor);
    }

    #[test]
    fn detects_opencode() {
        assert_eq!(detect_agent_family("opencode acp"), AgentFamily::OpenCode);
    }

    #[test]
    fn detects_claude() {
        assert_eq!(detect_agent_family("claude"), AgentFamily::Claude);
    }

    #[test]
    fn detects_crush() {
        assert_eq!(detect_agent_family("crush"), AgentFamily::Crush);
    }

    #[test]
    fn unknown_uses_wide_paths() {
        let dirs = global_skill_dirs(AgentFamily::Unknown);
        assert_eq!(dirs.len(), 3);
        assert!(dirs[0].ends_with(".agents/skills/via-editor"));
        assert!(dirs[1].ends_with(".config/agents/skills/via-editor"));
        assert!(dirs[2].ends_with(".claude/skills/via-editor"));
    }

    #[test]
    fn cursor_uses_cursor_and_agents_paths() {
        let dirs = global_skill_dirs(AgentFamily::Cursor);
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].ends_with(".cursor/skills/via-editor"));
        assert!(dirs[1].ends_with(".agents/skills/via-editor"));
    }

    #[test]
    fn skill_body_has_frontmatter() {
        assert!(skill_body().starts_with("---\n"));
        assert!(skill_body().contains("via session diagnostics"));
    }

    #[test]
    fn all_known_dirs_are_unique() {
        let dirs = all_known_skill_dirs();
        assert_eq!(dirs.len(), 5);
        assert!(
            dirs.iter()
                .any(|p| p.ends_with(".cursor/skills/via-editor"))
        );
        assert!(
            dirs.iter()
                .any(|p| p.ends_with(".config/opencode/skills/via-editor"))
        );
    }
}
