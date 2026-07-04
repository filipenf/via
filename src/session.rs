use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};

const MANIFEST_BASENAME: &str = "session.json";
const INSTANCES_DIR: &str = "instances";

/// Environment variable exported into child processes (Neovim, agent panes, the ACP subprocess)
/// pointing at this **instance** manifest. CLI subcommands prefer it over `--repo` matching.
pub const VIA_SESSION_ENV: &str = "VIA_SESSION";

/// Live via **instance** metadata (ephemeral process: pid, sockets, agent bus).
///
/// Durable task state lives under [`crate::workspace`] (`workspaces/<id>/boards/…`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionManifest {
    pub pid: u32,
    pub cwd: PathBuf,
    pub nvim_socket: PathBuf,
    pub editor_socket: PathBuf,
    /// Per-instance directory with the agent registry and per-agent mailboxes.
    #[serde(default)]
    pub agents_dir: PathBuf,
    /// When false, spawn/coordinated handoff is unavailable (primary agent is PTY-only).
    #[serde(default)]
    pub orchestration_enabled: bool,
    /// Stable workspace id (hash of `cwd`) for durable task storage.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Active task board id within the workspace.
    #[serde(default)]
    pub board_id: Option<String>,
    #[serde(default)]
    pub started_at_unix: Option<u64>,
}

impl SessionManifest {
    fn from_config(config: &Config) -> Self {
        let workspace = crate::workspace::workspace_for_cwd(&config.working_directory).ok();
        let board_id = workspace
            .as_ref()
            .and_then(|ws| crate::workspace::ensure_active_board(ws).ok());
        Self {
            pid: std::process::id(),
            cwd: config.working_directory.clone(),
            nvim_socket: config.nvim_socket_path.clone(),
            editor_socket: config.editor_socket_path.clone(),
            agents_dir: config.agents_dir.clone(),
            orchestration_enabled: config.orchestration_enabled,
            workspace_id: workspace.map(|ws| ws.id),
            board_id,
            started_at_unix: crate::util::unix_seconds_now(),
        }
    }

    fn is_live(&self) -> bool {
        self.nvim_socket.exists()
    }
}

pub struct SessionGuard {
    instance_dir: PathBuf,
}

impl SessionGuard {
    pub fn create(config: &Config) -> Result<Self> {
        let manifest = SessionManifest::from_config(config);
        let instance_dir = config::instance_dir(std::process::id());
        fs::create_dir_all(&instance_dir)
            .with_context(|| format!("create instance directory {}", instance_dir.display()))?;
        let path = instance_dir.join(MANIFEST_BASENAME);
        write_atomic_json(&path, &manifest)?;
        Ok(Self { instance_dir })
    }
}

/// Path this process writes (and exports via [`VIA_SESSION_ENV`]) for its instance manifest.
pub fn manifest_path() -> PathBuf {
    config::instance_dir(std::process::id()).join(MANIFEST_BASENAME)
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.instance_dir);
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

/// Resolve the live **instance** a CLI invocation should target, solely from the
/// [`VIA_SESSION_ENV`] manifest inherited from the via process that spawned this command.
///
/// There is no path-based or single-instance fallback: if the variable is unset (or points at an
/// instance that is no longer live), this returns an error so the failure is explicit.
pub fn resolve_session() -> Result<SessionManifest> {
    let Some(value) = std::env::var_os(VIA_SESSION_ENV) else {
        bail!(
            "{VIA_SESSION_ENV} is not set. Run this command from a terminal or agent launched by via."
        );
    };

    let path = PathBuf::from(value);
    let manifest = read_manifest(&path).with_context(|| {
        format!(
            "read instance manifest from {VIA_SESSION_ENV} ({})",
            path.display()
        )
    })?;

    if !manifest.is_live() {
        bail!(
            "the via instance referenced by {VIA_SESSION_ENV} is no longer live ({}).",
            path.display()
        );
    }

    Ok(manifest)
}

fn discover_manifest_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let instances_dir = config::via_data_dir().join(INSTANCES_DIR);

    if let Ok(entries) = fs::read_dir(&instances_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest = path.join(MANIFEST_BASENAME);
            if manifest.is_file() {
                paths.push(manifest);
            }
        }
    }

    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn read_manifest(path: &Path) -> Result<SessionManifest> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read instance manifest {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse instance manifest {}", path.display()))
}

fn write_atomic_json(path: &Path, manifest: &SessionManifest) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(manifest).context("serialize instance manifest")?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, serialized)
        .with_context(|| format!("write instance manifest {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("rename instance manifest to {}", path.display()))?;
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
            agents_dir: PathBuf::from("/tmp/instances/42/agents"),
            orchestration_enabled: true,
            workspace_id: None,
            board_id: None,
            started_at_unix: Some(1),
        };
        let encoded = serde_json::to_string(&manifest).unwrap();
        let decoded: SessionManifest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(manifest, decoded);
    }
}
