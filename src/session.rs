use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, AppContext};

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
    /// Stable workspace id (sanitized absolute `cwd`) for durable task storage.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Active task board id within the workspace.
    #[serde(default)]
    pub board_id: Option<String>,
    #[serde(default)]
    pub started_at_unix: Option<u64>,
}

impl SessionManifest {
    fn from_app(app: &AppContext) -> Self {
        let workspace = crate::workspace::workspace_for_cwd(&app.launch.working_directory).ok();
        let board_id = workspace
            .as_ref()
            .and_then(|ws| crate::workspace::ensure_active_board(ws).ok());
        Self {
            pid: std::process::id(),
            cwd: app.launch.working_directory.clone(),
            nvim_socket: app.paths.nvim_socket_path.clone(),
            editor_socket: app.paths.editor_socket_path.clone(),
            agents_dir: app.paths.agents_dir.clone(),
            orchestration_enabled: app.launch.orchestration_enabled,
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
    pub fn create(app: &AppContext) -> Result<Self> {
        let manifest = SessionManifest::from_app(app);
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

/// Rewrite `orchestration_enabled` on this process's instance manifest.
///
/// `via agent spawn` reads the flag from the manifest, so a live `via.conf`
/// reload has to publish the new value for already-running sessions.
pub fn set_orchestration_enabled(enabled: bool) -> Result<()> {
    let path = manifest_path();
    let mut manifest = read_manifest(&path)?;
    if manifest.orchestration_enabled == enabled {
        return Ok(());
    }
    manifest.orchestration_enabled = enabled;
    write_atomic_json(&path, &manifest)
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

    #[test]
    fn set_orchestration_enabled_rewrites_manifest() {
        let _lock = crate::test_support::env_lock();
        let dir = crate::test_support::temp_dir("session-orch");
        let _data_home = DataHomeGuard::set(&dir);

        let path = manifest_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let manifest = SessionManifest {
            pid: std::process::id(),
            cwd: PathBuf::from("/repo"),
            nvim_socket: PathBuf::from("/tmp/via-nvim.sock"),
            editor_socket: PathBuf::from("/tmp/via-editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            orchestration_enabled: false,
            workspace_id: Some("repo".to_string()),
            board_id: Some("default".to_string()),
            started_at_unix: Some(7),
        };
        write_atomic_json(&path, &manifest).unwrap();

        set_orchestration_enabled(true).unwrap();
        let updated = read_manifest(&path).unwrap();
        assert!(updated.orchestration_enabled);
        assert_eq!(updated.cwd, manifest.cwd);
        assert_eq!(updated.board_id.as_deref(), Some("default"));
        assert_eq!(updated.started_at_unix, Some(7));

        set_orchestration_enabled(true).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    struct DataHomeGuard(Option<std::ffi::OsString>);

    impl DataHomeGuard {
        fn set(dir: &Path) -> Self {
            let previous = std::env::var_os("XDG_DATA_HOME");
            // SAFETY: caller holds `env_lock` for the test.
            unsafe { std::env::set_var("XDG_DATA_HOME", dir) };
            Self(previous)
        }
    }

    impl Drop for DataHomeGuard {
        fn drop(&mut self) {
            // SAFETY: paired with `set`, still under `env_lock`.
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }
}
