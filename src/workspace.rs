//! Durable workspace storage keyed by a sanitized absolute working directory.
//!
//! Terminology:
//! - **Instance** — ephemeral live via process (pid, sockets, agent bus). See [`crate::session`].
//! - **Workspace** — durable project scope (sanitized canonical `cwd` path).
//! - **Board** — a task board within a workspace; switch when changing work context.
//!
//! Layout under [`crate::config::via_data_dir`]:
//!
//! ```text
//! instances/<pid>/              # ephemeral (see config::instance_dir)
//!
//! workspaces/<workspace-id>/
//!   meta.json
//!   active_board
//!   boards/<board-id>/
//!     meta.json
//!     tasks/*.md
//! ```
//!
//! Workspace ids are browsable path-derived names (e.g. `home_username_code_via`),
//! not opaque hashes. Distinct absolute paths that sanitize to the same string would collide;
//! that is accepted for now.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::util::now_millis;

pub const VIA_TASKS_DIR_ENV: &str = "VIA_TASKS_DIR";
pub const VIA_BOARD_ENV: &str = "VIA_BOARD";
pub const DEFAULT_BOARD_ID: &str = "default";

const WORKSPACES_DIR: &str = "workspaces";
const BOARDS_DIR: &str = "boards";
const TASKS_DIR: &str = "tasks";
const ACTIVE_BOARD_FILE: &str = "active_board";
const BOARD_META_FILE: &str = "meta.json";
const WORKSPACE_META_FILE: &str = "meta.json";

/// A durable workspace tied to a canonical working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub cwd: PathBuf,
    /// `workspaces/<id>/`
    pub root: PathBuf,
}

/// Workspace-level metadata stored under `workspaces/<id>/meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub id: String,
    pub cwd: PathBuf,
    pub created_at: u64,
}

/// Metadata for a task board inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardMeta {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_at: u64,
    /// Updated whenever the board becomes active (`via task board use` / `new`).
    /// Used to restore the most recently used board when `active_board` is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
}

/// Resolved task-board location for CLI commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasksContext {
    pub workspace_id: String,
    pub board_id: String,
    pub tasks_dir: PathBuf,
}

/// Stable workspace id from a canonical working directory (sanitized absolute path).
/// `/home/username/code/via` → `home_username_code_via`.
pub fn workspace_id(cwd: &Path) -> Result<String> {
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("resolve workspace cwd {}", cwd.display()))?;
    let raw = canonical.to_string_lossy();
    let trimmed = raw.trim_start_matches(['/', '\\']);
    let id = sanitize_path_segment(trimmed);
    if id.is_empty() {
        bail!("workspace id must not be empty");
    }
    Ok(id)
}

pub fn workspace_for_cwd(cwd: &Path) -> Result<Workspace> {
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("resolve workspace cwd {}", cwd.display()))?;
    let id = workspace_id(&canonical)?;
    let workspace = Workspace {
        id: id.clone(),
        root: workspace_root_for_id(&id),
        cwd: canonical,
    };
    ensure_workspace_meta(&workspace)?;
    Ok(workspace)
}

pub fn workspace_root_for_id(id: &str) -> PathBuf {
    config::via_data_dir()
        .join(WORKSPACES_DIR)
        .join(sanitize_path_segment(id))
}

fn board_root(workspace: &Workspace, board_id: &str) -> PathBuf {
    workspace
        .root
        .join(BOARDS_DIR)
        .join(sanitize_path_segment(board_id))
}

pub fn board_tasks_dir(workspace: &Workspace, board_id: &str) -> PathBuf {
    board_root(workspace, board_id).join(TASKS_DIR)
}

/// Resolve the active board and its tasks directory for `cwd`.
pub fn resolve_tasks_context(cwd: &Path) -> Result<TasksContext> {
    if let Ok(dir) = std::env::var(VIA_TASKS_DIR_ENV) {
        let board_id =
            std::env::var(VIA_BOARD_ENV).unwrap_or_else(|_| DEFAULT_BOARD_ID.to_string());
        return Ok(TasksContext {
            workspace_id: String::new(),
            board_id,
            tasks_dir: PathBuf::from(dir),
        });
    }

    let workspace = workspace_for_cwd(cwd)?;
    let board_id = ensure_active_board(&workspace)?;
    let tasks_dir = board_tasks_dir(&workspace, &board_id);
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("create tasks directory {}", tasks_dir.display()))?;
    Ok(TasksContext {
        workspace_id: workspace.id,
        board_id,
        tasks_dir,
    })
}

pub fn list_boards(workspace: &Workspace) -> Result<Vec<BoardMeta>> {
    let dir = workspace.root.join(BOARDS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read boards dir {}", dir.display())),
    };

    let mut boards = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let meta_path = path.join(BOARD_META_FILE);
        let meta = match std::fs::read_to_string(&meta_path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parse board meta {}", meta_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BoardMeta {
                id: id.to_string(),
                title: None,
                created_at: 0,
                last_used_at: None,
            },
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read board meta {}", meta_path.display()));
            }
        };
        boards.push(meta);
    }

    boards.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(boards)
}

pub fn active_board_id(workspace: &Workspace) -> Result<Option<String>> {
    let path = workspace.root.join(ACTIVE_BOARD_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let id = contents.trim();
            if id.is_empty() {
                Ok(None)
            } else {
                Ok(Some(id.to_string()))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read active board {}", path.display())),
    }
}

pub fn create_board(workspace: &Workspace, id: &str, title: Option<String>) -> Result<BoardMeta> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("board id must not be empty");
    }
    let id = sanitize_path_segment(trimmed);

    let board_root = board_root(workspace, &id);
    if board_root.exists() {
        bail!("board already exists: {id}");
    }

    let tasks_dir = board_root.join(TASKS_DIR);
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("create tasks directory {}", tasks_dir.display()))?;

    let now = now_millis();
    let meta = BoardMeta {
        id: id.clone(),
        title,
        created_at: now,
        last_used_at: Some(now),
    };
    let meta_path = board_root.join(BOARD_META_FILE);
    write_atomic_json(&meta_path, &meta)?;
    Ok(meta)
}

pub fn set_active_board(workspace: &Workspace, id: &str) -> Result<()> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("board id must not be empty");
    }
    let id = sanitize_path_segment(trimmed);
    let root = board_root(workspace, &id);
    if !root.is_dir() {
        bail!("board not found: {id}");
    }
    let path = workspace.root.join(ACTIVE_BOARD_FILE);
    write_atomic(&path, format!("{id}\n").as_bytes())?;
    touch_board_last_used(workspace, &id)?;
    Ok(())
}

/// Ensure a board is active.
///
/// Reuses the current active board when its directory exists. If the pointer is
/// missing or stale, reuses the most recently used existing board (by
/// `last_used_at`, falling back to `created_at`). Creates and activates
/// `default` only when the workspace has no boards yet — creating a board is
/// otherwise an explicit `via task board new`.
pub fn ensure_active_board(workspace: &Workspace) -> Result<String> {
    if let Some(id) = active_board_id(workspace)? {
        let board_root = board_root(workspace, &id);
        if board_root.is_dir() {
            return Ok(id);
        }
        tracing::warn!(
            board = %id,
            workspace = %workspace.id,
            "active board missing on disk; reusing the latest existing board"
        );
    }

    let boards = list_boards(workspace)?;
    if let Some(board) = pick_latest_board(&boards) {
        set_active_board(workspace, &board.id)?;
        return Ok(board.id.clone());
    }

    create_board(workspace, DEFAULT_BOARD_ID, None)?;
    set_active_board(workspace, DEFAULT_BOARD_ID)?;
    Ok(DEFAULT_BOARD_ID.to_string())
}

/// Prefer the board with the newest `last_used_at` (else `created_at`).
fn pick_latest_board(boards: &[BoardMeta]) -> Option<&BoardMeta> {
    boards.iter().max_by(|left, right| {
        board_recency(left)
            .cmp(&board_recency(right))
            .then_with(|| left.id.cmp(&right.id))
    })
}

fn board_recency(board: &BoardMeta) -> u64 {
    board.last_used_at.unwrap_or(board.created_at)
}

fn touch_board_last_used(workspace: &Workspace, id: &str) -> Result<()> {
    let meta_path = board_root(workspace, id).join(BOARD_META_FILE);
    let mut meta = match std::fs::read_to_string(&meta_path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("parse board meta {}", meta_path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BoardMeta {
            id: id.to_string(),
            title: None,
            created_at: now_millis(),
            last_used_at: None,
        },
        Err(err) => {
            return Err(err).with_context(|| format!("read board meta {}", meta_path.display()));
        }
    };
    meta.last_used_at = Some(now_millis());
    write_atomic_json(&meta_path, &meta)
}

fn ensure_workspace_meta(workspace: &Workspace) -> Result<()> {
    let path = workspace.root.join(WORKSPACE_META_FILE);
    if path.exists() {
        return Ok(());
    }
    let meta = WorkspaceMeta {
        id: workspace.id.clone(),
        cwd: workspace.cwd.clone(),
        created_at: now_millis(),
    };
    write_atomic_json(&path, &meta)
}

fn sanitize_path_segment(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.starts_with('.') {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}

fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("serialize json")?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

    struct DataDirGuard(Option<std::path::PathBuf>);

    impl DataDirGuard {
        fn set(base: &Path) -> Self {
            let via_data = base.join("via");
            std::fs::create_dir_all(&via_data).unwrap();
            let prev = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
            unsafe { std::env::set_var("XDG_DATA_HOME", base) };
            Self(prev)
        }
    }

    impl Drop for DataDirGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
                None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
            }
        }
    }

    fn temp_repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "via-repo-{}-{}-{}",
            label,
            std::process::id(),
            crate::util::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_data_dir(label: &str, body: impl FnOnce(&Path, &Workspace)) {
        let _lock = DATA_DIR_LOCK.lock().unwrap();
        let repo = temp_repo(label);
        let data_base = std::env::temp_dir().join(format!(
            "via-data-{}-{}-{}",
            label,
            std::process::id(),
            crate::util::now_millis()
        ));
        let _guard = DataDirGuard::set(&data_base);
        let workspace = workspace_for_cwd(&repo).unwrap();
        body(&repo, &workspace);
        std::fs::remove_dir_all(&data_base).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn workspace_id_is_stable_sanitized_path() {
        let dir = temp_repo("id");
        let first = workspace_id(&dir).unwrap();
        let second = workspace_id(&dir).unwrap();
        assert_eq!(first, second);
        let canonical = dir.canonicalize().unwrap();
        let expected =
            sanitize_path_segment(canonical.to_string_lossy().trim_start_matches(['/', '\\']));
        assert_eq!(first, expected);
        assert!(!first.is_empty());
        assert!(!first.contains('/'));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_list_and_switch_boards() {
        with_data_dir("boards", |_repo, workspace| {
            create_board(workspace, "sprint-a", Some("Sprint A".to_string())).unwrap();
            create_board(workspace, "sprint-b", None).unwrap();

            let listed = list_boards(workspace).unwrap();
            assert_eq!(listed.len(), 2);
            assert!(listed.iter().any(|b| b.id == "sprint-a"));

            set_active_board(workspace, "sprint-b").unwrap();
            assert_eq!(
                active_board_id(workspace).unwrap().as_deref(),
                Some("sprint-b")
            );

            let tasks_dir = board_tasks_dir(workspace, "sprint-b");
            assert_eq!(
                tasks_dir,
                workspace.root.join("boards").join("sprint-b").join("tasks")
            );
            assert_eq!(workspace.root, workspace_root_for_id(&workspace.id));
            assert!(workspace.root.join(WORKSPACE_META_FILE).is_file());
        });
    }

    #[test]
    fn ensure_active_creates_default_board_when_empty() {
        with_data_dir("default", |_repo, workspace| {
            let id = ensure_active_board(workspace).unwrap();
            assert_eq!(id, DEFAULT_BOARD_ID);
            assert!(board_tasks_dir(workspace, DEFAULT_BOARD_ID).is_dir());
            assert!(workspace.root.join(ACTIVE_BOARD_FILE).is_file());
            assert_eq!(list_boards(workspace).unwrap().len(), 1);
        });
    }

    #[test]
    fn ensure_active_reuses_existing_board_when_pointer_stale() {
        with_data_dir("reuse", |_repo, workspace| {
            create_board(workspace, "sprint-a", None).unwrap();
            set_active_board(workspace, "sprint-a").unwrap();

            // Corrupt the active pointer to a missing board.
            write_atomic(&workspace.root.join(ACTIVE_BOARD_FILE), b"missing-board\n").unwrap();

            let id = ensure_active_board(workspace).unwrap();
            assert_eq!(id, "sprint-a");
            assert_eq!(
                active_board_id(workspace).unwrap().as_deref(),
                Some("sprint-a")
            );
            assert_eq!(list_boards(workspace).unwrap().len(), 1);
            assert!(!board_root(workspace, DEFAULT_BOARD_ID).exists());
        });
    }

    #[test]
    fn ensure_active_prefers_most_recently_used_when_reusing() {
        with_data_dir("prefer-latest", |_repo, workspace| {
            create_board(workspace, "alpha", None).unwrap();
            create_board(workspace, DEFAULT_BOARD_ID, None).unwrap();
            create_board(workspace, "zeta", None).unwrap();
            // Mark zeta as the latest used board.
            set_active_board(workspace, "zeta").unwrap();
            write_atomic(&workspace.root.join(ACTIVE_BOARD_FILE), b"gone\n").unwrap();

            let id = ensure_active_board(workspace).unwrap();
            assert_eq!(id, "zeta");
            assert_eq!(list_boards(workspace).unwrap().len(), 3);
        });
    }

    #[test]
    fn ensure_active_keeps_valid_pointer() {
        with_data_dir("keep", |_repo, workspace| {
            create_board(workspace, "sprint-a", None).unwrap();
            create_board(workspace, DEFAULT_BOARD_ID, None).unwrap();
            set_active_board(workspace, "sprint-a").unwrap();

            let id = ensure_active_board(workspace).unwrap();
            assert_eq!(id, "sprint-a");
        });
    }

    #[test]
    fn sanitize_path_segment_blocks_traversal() {
        assert_eq!(sanitize_path_segment("../etc"), "_.._etc");
    }

    #[test]
    fn sanitize_path_segment_preserves_empty_input() {
        assert_eq!(sanitize_path_segment(""), "");
    }

    #[test]
    fn create_board_rejects_empty_id() {
        with_data_dir("empty-board", |_repo, workspace| {
            let err = create_board(workspace, "", None).unwrap_err();
            assert!(err.to_string().contains("board id must not be empty"));
            let err = create_board(workspace, "   ", None).unwrap_err();
            assert!(err.to_string().contains("board id must not be empty"));
        });
    }
}
