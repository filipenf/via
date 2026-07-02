use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};

const MANIFEST_BASENAME: &str = "session.json";
const FOREGROUND_MANIFEST_PREFIX: &str = "via-session-";

/// Environment variable exported into child processes (Neovim, agent panes, the ACP subprocess)
/// pointing at this session's manifest. CLI subcommands prefer it over `--repo` matching.
pub const VIA_SESSION_ENV: &str = "VIA_SESSION";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionManifest {
    pub pid: u32,
    pub cwd: PathBuf,
    pub nvim_socket: PathBuf,
    pub editor_socket: PathBuf,
    /// Per-process directory with the agent registry and per-agent mailboxes.
    #[serde(default)]
    pub agents_dir: PathBuf,
    /// When false, spawn/coordinated handoff is unavailable (primary agent is PTY-only).
    #[serde(default)]
    pub orchestration_enabled: bool,
    #[serde(default)]
    pub started_at_unix: Option<u64>,
}

impl SessionManifest {
    fn from_config(config: &Config) -> Self {
        Self {
            pid: std::process::id(),
            cwd: config.working_directory.clone(),
            nvim_socket: config.nvim_socket_path.clone(),
            editor_socket: config.editor_socket_path.clone(),
            agents_dir: config.agents_dir.clone(),
            orchestration_enabled: config.orchestration_enabled,
            started_at_unix: crate::util::unix_seconds_now(),
        }
    }

    fn is_live(&self) -> bool {
        self.nvim_socket.exists()
    }
}

pub struct SessionGuard {
    path: PathBuf,
}

impl SessionGuard {
    pub fn create(config: &Config) -> Result<Self> {
        let manifest = SessionManifest::from_config(config);
        let path = manifest_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime directory {}", parent.display()))?;
        }
        write_atomic_json(&path, &manifest)?;
        Ok(Self { path })
    }
}

/// Path this process writes (and exports via [`VIA_SESSION_ENV`]) for its session manifest.
pub fn manifest_path() -> PathBuf {
    manifest_path_for_runtime(config::runtime_base_dir(), std::process::id())
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn list_sessions() -> Result<Vec<SessionManifest>> {
    let mut manifests = Vec::new();
    for path in discover_manifest_paths()? {
        if let Ok(manifest) = read_manifest(&path) {
            manifests.push(manifest);
        }
    }
    manifests.sort_by(|left, right| left.cwd.cmp(&right.cwd));
    Ok(manifests)
}

pub fn list_live_sessions() -> Result<Vec<SessionManifest>> {
    Ok(list_sessions()?
        .into_iter()
        .filter(SessionManifest::is_live)
        .collect())
}

/// Resolve the session a CLI invocation should target, solely from the [`VIA_SESSION_ENV`]
/// manifest inherited from the via process that spawned this command.
///
/// There is no path-based or single-session fallback: if the variable is unset (or points at a
/// session that is no longer live), this returns an error so the failure is explicit.
pub fn resolve_session() -> Result<SessionManifest> {
    let Some(value) = std::env::var_os(VIA_SESSION_ENV) else {
        bail!(
            "{VIA_SESSION_ENV} is not set. Run this command from a terminal or agent launched by via."
        );
    };

    let path = PathBuf::from(value);
    let manifest = read_manifest(&path).with_context(|| {
        format!(
            "read session manifest from {VIA_SESSION_ENV} ({})",
            path.display()
        )
    })?;

    if !manifest.is_live() {
        bail!(
            "the via session referenced by {VIA_SESSION_ENV} is no longer live ({}).",
            path.display()
        );
    }

    Ok(manifest)
}

fn manifest_path_for_runtime(runtime_dir: PathBuf, pid: u32) -> PathBuf {
    if std::env::var_os("VIA_RUNTIME_ROOT").is_some() {
        runtime_dir.join(MANIFEST_BASENAME)
    } else {
        runtime_dir.join(format!("{FOREGROUND_MANIFEST_PREFIX}{pid}.json"))
    }
}

fn discover_manifest_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let data_dir = config::via_data_dir();

    if let Ok(entries) = fs::read_dir(&data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            if path.is_dir() {
                // Detached runs live in `<data dir>/via-<pid>/session.json`.
                if name.starts_with("via-") {
                    let manifest = path.join(MANIFEST_BASENAME);
                    if manifest.is_file() {
                        paths.push(manifest);
                    }
                }
            } else if name.starts_with(FOREGROUND_MANIFEST_PREFIX) && name.ends_with(".json") {
                // Foreground runs write `<data dir>/via-session-<pid>.json` directly.
                paths.push(path);
            }
        }
    }

    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn read_manifest(path: &Path) -> Result<SessionManifest> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read session manifest {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse session manifest {}", path.display()))
}

fn write_atomic_json(path: &Path, manifest: &SessionManifest) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(manifest).context("serialize session manifest")?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, serialized)
        .with_context(|| format!("write session manifest {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("rename session manifest to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip_json() {
        let manifest = SessionManifest {
            pid: 42,
            cwd: PathBuf::from("/repo"),
            nvim_socket: PathBuf::from("/tmp/via-nvim-42.sock"),
            editor_socket: PathBuf::from("/tmp/via-editor-42.sock"),
            agents_dir: PathBuf::from("/tmp/via-42/agents"),
            orchestration_enabled: true,
            started_at_unix: Some(1),
        };
        let encoded = serde_json::to_string(&manifest).unwrap();
        let decoded: SessionManifest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(manifest, decoded);
    }
}
