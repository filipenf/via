//! The via plugin: skills (and, later, agents/workflows/tools) projected into an agent's skill
//! directory so the running agent can drive via.
//!
//! Design goals:
//! - Keep skill bodies **out of the binary**. Core skills live as files under the repo's
//!   `skills/` tree (or any directory the user points at).
//! - Install is **explicit** (`via plugin install`). Startup only warns when core skills are
//!   missing so forks in agent skill dirs are never overwritten silently.
//! - Users can fork installed skills; reinstall skips existing dirs unless `--force`.
//! - Install copies into **every available agent** skill root on the host (cursor, claude,
//!   opencode, crush, …), similar to `npx skills add --agent '*'`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const SKILL_FILE: &str = "SKILL.md";

/// Core skills that via expects to be installed for full functionality.
pub const CORE_SKILLS: &[&str] = &["via-editor", "via-agents", "via-orc"];

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

fn binary_on_path(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn config_home_exists(segments: &[&str]) -> bool {
    home_dir().join(segments.join("/")).exists()
}

/// Whether this agent family looks installed / in use on the host.
fn family_available(family: AgentFamily) -> bool {
    match family {
        AgentFamily::Cursor => {
            // Prefer `cursor-agent` — a bare `agent` on PATH is too ambiguous.
            binary_on_path("cursor-agent") || config_home_exists(&[".cursor"])
        }
        AgentFamily::OpenCode => {
            binary_on_path("opencode") || config_home_exists(&[".config", "opencode"])
        }
        AgentFamily::Claude => {
            binary_on_path("claude")
                || binary_on_path("claude-code-acp")
                || config_home_exists(&[".claude"])
        }
        AgentFamily::Crush => {
            binary_on_path("crush") || config_home_exists(&[".config", "agents"])
        }
        AgentFamily::Unknown => false,
    }
}

/// Agent families detected on this host (cursor / claude / opencode / crush).
pub fn available_families() -> Vec<AgentFamily> {
    [
        AgentFamily::Cursor,
        AgentFamily::OpenCode,
        AgentFamily::Claude,
        AgentFamily::Crush,
    ]
    .into_iter()
    .filter(|f| family_available(*f))
    .collect()
}

/// Deduped skill roots for every available agent. If none are detected, falls back to all
/// known roots so a fresh machine still gets a useful install.
pub fn install_roots() -> (Vec<AgentFamily>, Vec<PathBuf>) {
    let families = available_families();
    if families.is_empty() {
        return (Vec::new(), all_known_roots());
    }
    let mut roots = Vec::new();
    for family in &families {
        for root in skill_roots(*family) {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    // Shared discovery path used by several agents.
    let agents = home_dir().join(AGENTS_ROOT.join("/"));
    if !roots.contains(&agents) {
        roots.push(agents);
    }
    (families, roots)
}

/// Resolve a directory that contains skill folders (`via-editor/`, …).
///
/// Accepts either a plugin/repo root with a `skills/` subdirectory, or a path that is itself
/// the `skills/` directory (contains `via-editor/SKILL.md` directly).
pub fn resolve_skills_dir(source: &Path) -> Result<PathBuf> {
    let nested = source.join("skills");
    if nested.is_dir() && skill_dir_looks_valid(&nested.join("via-editor")) {
        return Ok(nested);
    }
    if source.is_dir() && skill_dir_looks_valid(&source.join("via-editor")) {
        return Ok(source.to_path_buf());
    }
    bail!(
        "no via skills found under {} (expected skills/via-editor/SKILL.md or via-editor/SKILL.md). \
         Pass --from <path-to-via-checkout-or-skills-dir>",
        source.display()
    );
}

fn skill_dir_looks_valid(skill_dir: &Path) -> bool {
    skill_dir.join(SKILL_FILE).is_file()
}

/// Default source for skills when the user did not pass `--from` / plugin_dir.
///
/// Order: `VIA_SKILLS_DIR` → `{CARGO_MANIFEST_DIR}/skills` when that path still exists.
pub fn default_skills_source() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VIA_SKILLS_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        let skills = Path::new(manifest).join("skills");
        if skills.is_dir() {
            return Some(skills);
        }
    }
    None
}

/// Resolve the skills source directory from explicit path, env, or build-time checkout.
pub fn resolve_install_source(from: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = from {
        return resolve_skills_dir(path);
    }
    if let Some(path) = default_skills_source() {
        return resolve_skills_dir(&path);
    }
    bail!(
        "cannot find via skills to install. Pass --from <path> pointing at the via repo (or its \
         skills/ directory), or set VIA_SKILLS_DIR / VIA_PLUGIN_DIR"
    );
}

/// Result of an install into the host's available agent skill roots.
#[derive(Debug)]
pub struct InstallReport {
    pub families: Vec<AgentFamily>,
    pub roots: Vec<PathBuf>,
    pub written: Vec<PathBuf>,
    pub fallback_all_roots: bool,
}

/// Install skills from `skills_dir` into every available agent skill root on the host
/// (cursor, claude, opencode, crush, …), similar to `npx skills add --agent '*'`.
///
/// When `force` is false, existing destination skill directories are left untouched (fork-safe).
pub fn install(skills_dir: &Path, force: bool) -> Result<InstallReport> {
    let skills_dir = resolve_skills_dir(skills_dir)?;
    let skill_entries = list_skill_dirs(&skills_dir)?;
    if skill_entries.is_empty() {
        bail!("no skill directories found in {}", skills_dir.display());
    }

    let (families, roots) = install_roots();
    let fallback_all_roots = families.is_empty();
    let mut written = Vec::new();

    for root in &roots {
        for src in &skill_entries {
            let Some(name) = src.file_name() else {
                continue;
            };
            let dest = root.join(name);
            // Fork-safe: only skip when a real SKILL.md is already present.
            // Empty/broken dirs (no SKILL.md) are treated as missing and replaced.
            if dest.join(SKILL_FILE).is_file() && !force {
                continue;
            }
            if dest.exists() {
                fs::remove_dir_all(&dest)
                    .with_context(|| format!("remove existing skill {}", dest.display()))?;
            }
            written.extend(copy_dir_all(src, &dest)?);
        }
    }

    Ok(InstallReport {
        families,
        roots,
        written,
        fallback_all_roots,
    })
}

fn list_skill_dirs(skills_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(skills_dir)
        .with_context(|| format!("read skills dir {}", skills_dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() && skill_dir_looks_valid(&path) {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
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

/// Remove the core skills from every known skill root (all families). Extra user skills are
/// left untouched.
pub fn cleanup() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for root in all_known_roots() {
        for name in CORE_SKILLS {
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
    use crate::test_support::env_lock;

    fn write_skill(skills_root: &Path, name: &str, body: &str) {
        let dir = skills_root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_FILE), body).unwrap();
    }

    fn with_temp_home<F: FnOnce(&Path, &Path)>(f: F) {
        let _guard = env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "via-plugin-test-{}-{}",
            std::process::id(),
            crate::util::now_millis()
        ));
        let home = tmp.join("home");
        let source = tmp.join("skills");
        let empty_bin = tmp.join("empty-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&empty_bin).unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("PATH");
        // SAFETY: serialized by env_lock; restored before returning.
        // Empty PATH so host agent binaries do not leak into availability detection.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("PATH", &empty_bin);
        }

        f(&home, &source);

        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
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
    fn install_copies_into_all_available_agent_roots() {
        with_temp_home(|home, source| {
            // Mark cursor + claude as available via config dirs; leave opencode absent.
            fs::create_dir_all(home.join(".cursor")).unwrap();
            fs::create_dir_all(home.join(".claude")).unwrap();

            write_skill(source, "via-editor", "---\nname: via-editor\n---\n");
            write_skill(source, "via-agents", "---\nname: via-agents\n---\n");
            write_skill(source, "via-orc", "---\nname: via-orc\n---\n");
            write_skill(source, "custom-role", "custom skill body");

            let report = install(source, false).unwrap();
            assert!(!report.written.is_empty());
            assert!(report.families.contains(&AgentFamily::Cursor));
            assert!(report.families.contains(&AgentFamily::Claude));
            assert!(!report.families.contains(&AgentFamily::OpenCode));

            for root in [
                home.join(".cursor/skills"),
                home.join(".claude/skills"),
                home.join(".agents/skills"),
            ] {
                assert!(
                    root.join("via-editor").join(SKILL_FILE).exists(),
                    "missing via-editor under {}",
                    root.display()
                );
                assert!(root.join("via-orc").join(SKILL_FILE).exists());
                assert!(root.join("custom-role").join(SKILL_FILE).exists());
            }
            assert!(!home
                .join(".config/opencode/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
        });
    }

    #[test]
    fn install_falls_back_to_all_roots_when_no_agents_detected() {
        with_temp_home(|home, source| {
            write_skill(source, "via-editor", "body");
            let report = install(source, false).unwrap();
            assert!(report.fallback_all_roots);
            assert!(home
                .join(".cursor/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
            assert!(home
                .join(".claude/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
            assert!(home
                .join(".config/opencode/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
        });
    }

    #[test]
    fn install_skips_existing_without_force() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".cursor")).unwrap();
            write_skill(source, "via-editor", "new body");
            let dest = home.join(".cursor/skills/via-editor");
            fs::create_dir_all(&dest).unwrap();
            fs::write(dest.join(SKILL_FILE), "forked body").unwrap();

            let report = install(source, false).unwrap();
            let body = fs::read_to_string(dest.join(SKILL_FILE)).unwrap();
            assert_eq!(body, "forked body");
            assert!(
                !report.written.iter().any(|p| p.starts_with(&dest)),
                "must not rewrite an existing skill without --force"
            );
            assert!(home
                .join(".agents/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
        });
    }

    #[test]
    fn install_force_overwrites() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".cursor")).unwrap();
            write_skill(source, "via-editor", "new body");
            let dest = home.join(".cursor/skills/via-editor");
            fs::create_dir_all(&dest).unwrap();
            fs::write(dest.join(SKILL_FILE), "forked body").unwrap();

            let report = install(source, true).unwrap();
            assert!(!report.written.is_empty());
            let body = fs::read_to_string(dest.join(SKILL_FILE)).unwrap();
            assert_eq!(body, "new body");
        });
    }

    #[test]
    fn resolve_skills_dir_accepts_repo_root_or_skills_dir() {
        with_temp_home(|_home, source| {
            write_skill(source, "via-editor", "---\nname: via-editor\n---\n");
            assert_eq!(resolve_skills_dir(source).unwrap(), source);

            let repo = source.parent().unwrap().join("repo");
            let nested = repo.join("skills");
            fs::create_dir_all(&nested).unwrap();
            write_skill(&nested, "via-editor", "---\nname: via-editor\n---\n");
            assert_eq!(resolve_skills_dir(&repo).unwrap(), nested);
        });
    }

    #[test]
    fn missing_core_skills_reports_absent() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".cursor")).unwrap();
            write_skill(source, "via-editor", "editor");
            install(source, false).unwrap();
            let missing = missing_core_skills(AgentFamily::Cursor).unwrap();
            assert!(missing.contains(&"via-agents"));
            assert!(missing.contains(&"via-orc"));
            assert!(!missing.contains(&"via-editor"));

            assert!(home
                .join(".cursor/skills/via-editor")
                .join(SKILL_FILE)
                .exists());
        });
    }

    #[test]
    fn status_is_presence_only() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".claude")).unwrap();
            write_skill(source, "via-editor", "anything");
            install(source, false).unwrap();
            let claude_root = home.join(".claude/skills");
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
    fn install_replaces_broken_dest_without_force() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".cursor")).unwrap();
            write_skill(source, "via-editor", "repaired body");
            let dest = home.join(".cursor/skills/via-editor");
            fs::create_dir_all(&dest).unwrap();
            // Exists but missing SKILL.md — should not stick.
            let report = install(source, false).unwrap();
            assert!(report.written.iter().any(|p| p.starts_with(&dest)));
            let body = fs::read_to_string(dest.join(SKILL_FILE)).unwrap();
            assert_eq!(body, "repaired body");
        });
    }

    #[test]
    fn cleanup_removes_core_skills() {
        with_temp_home(|home, source| {
            fs::create_dir_all(home.join(".cursor")).unwrap();
            for name in CORE_SKILLS {
                write_skill(source, name, "body");
            }
            install(source, false).unwrap();
            let removed = cleanup().unwrap();
            assert!(!removed.is_empty());
            for name in CORE_SKILLS {
                assert!(!home
                    .join(".cursor/skills")
                    .join(name)
                    .join(SKILL_FILE)
                    .exists());
            }
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
