//! The via plugin: the skills (and, later, agents/workflows/tools) that via projects into an
//! agent's skill directory so the running agent can drive via.
//!
//! Design goals:
//! - Keep a tiny **embedded base** (the `via-editor` and `via-agents` skills) so the agent bus
//!   works out of the box, with nothing to download.
//! - Let users **customize** by pointing via at a local plugin directory (`plugin_dir`). Its
//!   `skills/<name>/` entries are overlaid into the agent's skill roots; the embedded base is
//!   always re-asserted on top so the base stays the same.
//!
//! "Install" simply projects skills into the agent family's skill directories (where cursor,
//! opencode, etc. already auto-discover them). The plugin directory can hold more than skills
//! (agents, workflows, MCP config) for future use without bloating the binary.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SKILL_FILE: &str = "SKILL.md";

/// Embedded base skills: `(skill name, SKILL.md body)`. Always installed.
const BASE_SKILLS: &[(&str, &str)] = &[
    ("via-editor", include_str!("../skills/via-editor/SKILL.md")),
    ("via-agents", include_str!("../skills/via-agents/SKILL.md")),
];

// Skill roots under `$HOME`. Each gets agent skill directories appended.
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

/// Skill root directories where the plugin's skills should be projected for this family.
fn skill_roots(family: AgentFamily) -> Vec<PathBuf> {
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

/// Install the plugin for `family`: overlay the user's `plugin_dir/skills/*` (if any), then the
/// embedded base skills, into each skill root. Returns every path written.
pub fn install(family: AgentFamily, plugin_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();

    for root in skill_roots(family) {
        // 1. User-provided skills first (so the base can re-assert on top).
        if let Some(dir) = plugin_dir {
            let user_skills = dir.join("skills");
            if user_skills.is_dir() {
                for entry in fs::read_dir(&user_skills)
                    .with_context(|| format!("read plugin skills {}", user_skills.display()))?
                    .flatten()
                {
                    let src = entry.path();
                    if !src.is_dir() {
                        continue;
                    }
                    let Some(name) = src.file_name() else {
                        continue;
                    };
                    let dest = root.join(name);
                    written.extend(copy_dir_all(&src, &dest)?);
                }
            }
        }

        // 2. Embedded base skills, always (re)written so the base is stable.
        for (name, body) in BASE_SKILLS {
            let path = root.join(name).join(SKILL_FILE);
            if write_if_changed(&path, body)? {
                written.push(path);
            }
        }
    }

    Ok(written)
}

/// Primary install path (the first skill root) for `family`.
pub fn primary_root(family: AgentFamily) -> Result<PathBuf> {
    skill_roots(family)
        .into_iter()
        .next()
        .context("HOME is not set; cannot resolve plugin install path")
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

/// Report the state of each embedded base skill across `family`'s skill roots.
pub fn status(family: AgentFamily) -> Result<Vec<SkillInstallStatus>> {
    let mut out = Vec::new();
    for root in skill_roots(family) {
        for (name, body) in BASE_SKILLS {
            let path = root.join(name).join(SKILL_FILE);
            let state = if !path.exists() {
                SkillState::Missing
            } else {
                match fs::read(&path) {
                    Ok(existing) if existing == body.as_bytes() => SkillState::Installed,
                    Ok(_) => SkillState::Outdated,
                    Err(_) => SkillState::Unreadable,
                }
            };
            out.push(SkillInstallStatus { path, state });
        }
    }
    Ok(out)
}

/// Remove the embedded base skills from every known skill root (all families). User-provided
/// skills are left untouched since via does not track their names.
pub fn cleanup() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for root in all_known_roots() {
        for (name, _) in BASE_SKILLS {
            let dir = root.join(name);
            let path = dir.join(SKILL_FILE);
            if !path.exists() {
                continue;
            }
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            removed.push(path);
            if dir.is_dir() && is_dir_empty(&dir)? {
                fs::remove_dir(&dir).ok();
            }
        }
    }
    Ok(removed)
}

fn all_known_roots() -> Vec<PathBuf> {
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

fn copy_dir_all(src: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
        .flatten()
    {
        let from = entry.path();
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = dest.join(name);
        if from.is_dir() {
            written.extend(copy_dir_all(&from, &to)?);
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
            written.push(to);
        }
    }
    Ok(written)
}

fn write_if_changed(path: &Path, body: &str) -> Result<bool> {
    if let Ok(existing) = fs::read(path) {
        if existing == body.as_bytes() {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, body).with_context(|| format!("write skill {}", path.display()))?;
    Ok(true)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
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
        let roots = skill_roots(AgentFamily::Cursor);
        assert_eq!(roots.len(), 2);
        assert!(roots[0].ends_with(".cursor/skills"));
        assert!(roots[1].ends_with(".agents/skills"));
    }

    #[test]
    fn install_writes_base_skills_and_user_overlay() {
        let tmp = std::env::temp_dir().join(format!(
            "via-plugin-test-{}-{}",
            std::process::id(),
            crate::util::now_millis()
        ));
        let home = tmp.join("home");
        let plugin = tmp.join("plugin");
        fs::create_dir_all(plugin.join("skills/custom-role")).unwrap();
        fs::write(
            plugin.join("skills/custom-role").join(SKILL_FILE),
            "custom skill body",
        )
        .unwrap();

        // Point HOME at the temp dir for the duration of this test.
        // SAFETY: single-threaded test; restored before returning.
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };

        let written = install(AgentFamily::Cursor, Some(&plugin)).unwrap();
        assert!(!written.is_empty());

        let cursor_skills = home.join(".cursor/skills");
        assert!(cursor_skills.join("via-editor").join(SKILL_FILE).exists());
        assert!(cursor_skills.join("via-agents").join(SKILL_FILE).exists());
        assert!(cursor_skills.join("custom-role").join(SKILL_FILE).exists());

        // Restore HOME.
        unsafe {
            match prev {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn base_skills_have_frontmatter() {
        for (_, body) in BASE_SKILLS {
            assert!(body.starts_with("---\n"), "skill missing frontmatter");
        }
    }
}
