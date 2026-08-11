//! Core via skills live in the repo's `skills/` tree and are installed by the user
//! (e.g. `npx skills add`, `skills-rs`, or a manual copy) — not by via itself.
//!
//! This module only:
//! - maps the session agent command to skill root directories
//! - reports whether the core skills are present
//! - warns at startup when they are missing

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

const SKILL_FILE: &str = "SKILL.md";

/// Core skills that via expects for full agent functionality.
pub const CORE_SKILLS: &[&str] = &["via-editor", "via-agents", "via-orc"];

// Skill roots under `$HOME`.
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

impl AgentFamily {
    pub fn label(self) -> &'static str {
        match self {
            AgentFamily::Cursor => "cursor",
            AgentFamily::OpenCode => "opencode",
            AgentFamily::Claude => "claude",
            AgentFamily::Crush => "crush",
            AgentFamily::Unknown => "unknown",
        }
    }
}

/// First token of the agent command, basename only (e.g. `/bin/opencode acp` → `opencode`).
fn agent_binary(agent_command: &str) -> String {
    let Some(first) = agent_command.split_whitespace().next() else {
        return String::new();
    };
    Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase()
}

/// Infer the agent runtime from `VIA_AGENT` (or similar) command text.
pub fn detect_agent_family(agent_command: &str) -> AgentFamily {
    match agent_binary(agent_command).as_str() {
        "agent" | "cursor-agent" => AgentFamily::Cursor,
        "opencode" => AgentFamily::OpenCode,
        "claude" | "claude-code-acp" => AgentFamily::Claude,
        "crush" => AgentFamily::Crush,
        _ => AgentFamily::Unknown,
    }
}

/// Skill root directories where agents of this family look for skills.
pub fn skill_roots(family: AgentFamily) -> Vec<PathBuf> {
    let home = home_dir();
    let dir = |root: &[&str]| home.join(root.join("/"));

    match family {
        AgentFamily::Cursor => vec![dir(CURSOR_ROOT), dir(AGENTS_ROOT)],
        AgentFamily::OpenCode => vec![dir(OPENCODE_ROOT), dir(AGENTS_ROOT), dir(CLAUDE_ROOT)],
        AgentFamily::Claude => vec![dir(CLAUDE_ROOT)],
        AgentFamily::Crush => vec![dir(CONFIG_AGENTS_ROOT)],
        AgentFamily::Unknown => vec![dir(AGENTS_ROOT), dir(CONFIG_AGENTS_ROOT), dir(CLAUDE_ROOT)],
    }
}

/// Deduped skill roots for every known agent family.
pub fn all_known_roots() -> Vec<PathBuf> {
    let families = [
        AgentFamily::Cursor,
        AgentFamily::OpenCode,
        AgentFamily::Claude,
        AgentFamily::Crush,
        AgentFamily::Unknown,
    ];
    let mut roots = Vec::new();
    for family in families {
        for root in skill_roots(family) {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillState {
    Missing,
    Installed,
    Unreadable,
}

#[derive(Debug, Clone)]
pub struct SkillInstallStatus {
    pub name: String,
    pub path: PathBuf,
    pub state: SkillState,
}

/// Report the state of each core skill across all known skill roots (presence only).
pub fn status_all() -> Result<Vec<SkillInstallStatus>> {
    status_for_roots(&all_known_roots())
}

fn status_for_roots(roots: &[PathBuf]) -> Result<Vec<SkillInstallStatus>> {
    let mut out = Vec::new();
    for root in roots {
        for name in CORE_SKILLS {
            let path = root.join(name).join(SKILL_FILE);
            let state = if !path.exists() {
                SkillState::Missing
            } else {
                match fs::read(&path) {
                    Ok(_) => SkillState::Installed,
                    Err(_) => SkillState::Unreadable,
                }
            };
            out.push(SkillInstallStatus {
                name: (*name).to_string(),
                path,
                state,
            });
        }
    }
    Ok(out)
}

/// Names of core skills that are not present in any of `family`'s skill roots.
pub fn missing_core_skills(family: AgentFamily) -> Result<Vec<&'static str>> {
    let mut missing = Vec::new();
    let roots = skill_roots(family);
    for name in CORE_SKILLS {
        let found = roots
            .iter()
            .any(|root| root.join(name).join(SKILL_FILE).is_file());
        if !found {
            missing.push(*name);
        }
    }
    Ok(missing)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::via_data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_FILE), body).unwrap();
    }

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let _guard = env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "via-plugin-test-{}-{}",
            std::process::id(),
            crate::util::now_millis()
        ));
        let home = tmp.join("home");
        fs::create_dir_all(&home).unwrap();

        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialized by env_lock; restored before returning.
        unsafe { std::env::set_var("HOME", &home) };

        f(&home);

        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detects_families() {
        assert_eq!(detect_agent_family("cursor-agent acp"), AgentFamily::Cursor);
        assert_eq!(detect_agent_family("agent"), AgentFamily::Cursor);
        assert_eq!(detect_agent_family("agent acp"), AgentFamily::Cursor);
        assert_eq!(
            detect_agent_family("/home/user/.local/bin/agent"),
            AgentFamily::Cursor
        );
        assert_eq!(detect_agent_family("opencode acp"), AgentFamily::OpenCode);
        assert_eq!(detect_agent_family("claude"), AgentFamily::Claude);
        assert_eq!(detect_agent_family("crush"), AgentFamily::Crush);
        assert_eq!(detect_agent_family("something-else"), AgentFamily::Unknown);
    }

    #[test]
    fn cursor_uses_cursor_and_agents_roots() {
        let _guard = env_lock();
        let roots = skill_roots(AgentFamily::Cursor);
        assert_eq!(roots.len(), 2);
        assert!(roots[0].ends_with(".cursor/skills"));
        assert!(roots[1].ends_with(".agents/skills"));
    }

    #[test]
    fn missing_core_skills_reports_absent() {
        with_temp_home(|home| {
            write_skill(&home.join(".cursor/skills"), "via-editor", "editor");
            let missing = missing_core_skills(AgentFamily::Cursor).unwrap();
            assert!(missing.contains(&"via-agents"));
            assert!(missing.contains(&"via-orc"));
            assert!(!missing.contains(&"via-editor"));
        });
    }

    #[test]
    fn status_is_presence_only() {
        with_temp_home(|home| {
            let claude_root = home.join(".claude/skills");
            write_skill(&claude_root, "via-editor", "anything");
            let entries = status_all().unwrap();
            let editor = entries
                .iter()
                .find(|e| e.name == "via-editor" && e.path.starts_with(&claude_root))
                .expect("via-editor status");
            assert_eq!(editor.state, SkillState::Installed);
            let orc = entries
                .iter()
                .find(|e| e.name == "via-orc" && e.path.starts_with(&claude_root))
                .expect("via-orc status");
            assert_eq!(orc.state, SkillState::Missing);
        });
    }

    #[test]
    fn in_tree_core_skills_have_frontmatter() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        for name in CORE_SKILLS {
            let path = manifest.join(name).join(SKILL_FILE);
            let body = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            assert!(
                body.starts_with("---\n"),
                "{} missing frontmatter",
                path.display()
            );
        }
    }
}
