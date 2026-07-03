//! File-backed task board for structured work items.
//!
//! One JSON file per task lives directly under a work-session tasks directory
//! (see [`crate::workspace`]), using the same atomic-write pattern as [`crate::agent_bus`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::util::now_millis;

/// Lifecycle status for a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    #[serde(alias = "doing", alias = "inprogress")]
    InProgress,
    Review,
    Done,
    Blocked,
}

/// A structured work item tracked in the session store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Fields supplied when creating a task.
#[derive(Debug, Clone)]
pub struct CreateTask {
    pub title: String,
    pub id: Option<String>,
    pub assignee: Option<String>,
    pub blocked_by: Vec<String>,
    pub created_by: Option<String>,
    pub body: Option<String>,
}

/// Partial update applied to an existing task.
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub title: Option<String>,
    pub status: Option<TaskStatus>,
    pub assignee: Option<Option<String>>,
    pub blocked_by: Option<Vec<String>>,
    pub body: Option<Option<String>>,
}

/// Optional filters for [`list_tasks`].
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub assignee: Option<String>,
}

fn task_path(tasks_dir: &Path, id: &str) -> PathBuf {
    tasks_dir.join(format!("{}.json", sanitize_id(id)))
}

/// Create a new task with status `queued`.
pub fn create_task(tasks_dir: &Path, input: CreateTask) -> Result<Task> {
    let title = input.title.trim();
    if title.is_empty() {
        bail!("task title must not be empty");
    }

    let id = match input.id {
        Some(id) => {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                bail!("task id must not be empty");
            }
            if task_path(tasks_dir, trimmed).exists() {
                bail!("task already exists: {trimmed}");
            }
            trimmed.to_string()
        }
        None => unique_task_id(),
    };

    let now = now_millis();
    let task = Task {
        id: id.clone(),
        title: title.to_string(),
        status: TaskStatus::Queued,
        assignee: input.assignee,
        blocked_by: input.blocked_by,
        created_at: now,
        updated_at: now,
        created_by: input.created_by,
        body: input.body,
    };

    write_task(tasks_dir, &task)?;
    Ok(task)
}

/// Load a single task by id.
pub fn get_task(tasks_dir: &Path, id: &str) -> Result<Option<Task>> {
    let path = task_path(tasks_dir, id);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(
            serde_json::from_str(&contents)
                .with_context(|| format!("parse task {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read task {}", path.display())),
    }
}

/// List tasks, optionally filtered, ordered oldest-first by `created_at`.
pub fn list_tasks(tasks_dir: &Path, filter: &TaskFilter) -> Result<Vec<Task>> {
    let entries = match std::fs::read_dir(tasks_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("read tasks dir {}", tasks_dir.display()));
        }
    };

    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read task {}", path.display()))?;
        match serde_json::from_str::<Task>(&contents) {
            Ok(task) => tasks.push(task),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "skipping unparseable task file");
            }
        }
    }

    tasks.retain(|task| matches_filter(task, filter));
    tasks.sort_by_key(|task| task.created_at);
    Ok(tasks)
}

/// Apply a partial update to an existing task.
pub fn update_task(tasks_dir: &Path, id: &str, update: TaskUpdate) -> Result<Task> {
    let mut task = get_task(tasks_dir, id)?.with_context(|| format!("task not found: {id}"))?;

    if let Some(title) = update.title {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            bail!("task title must not be empty");
        }
        task.title = trimmed.to_string();
    }
    if let Some(status) = update.status {
        task.status = status;
    }
    if let Some(assignee) = update.assignee {
        task.assignee = assignee;
    }
    if let Some(blocked_by) = update.blocked_by {
        task.blocked_by = blocked_by;
    }
    if let Some(body) = update.body {
        task.body = body;
    }

    task.updated_at = now_millis();
    write_task(tasks_dir, &task)?;
    Ok(task)
}

/// Assign a task to `assignee` and move it to `in_progress`.
pub fn claim_task(tasks_dir: &Path, id: &str, assignee: &str) -> Result<Task> {
    let trimmed = assignee.trim();
    if trimmed.is_empty() {
        bail!("assignee must not be empty");
    }
    update_task(
        tasks_dir,
        id,
        TaskUpdate {
            assignee: Some(Some(trimmed.to_string())),
            status: Some(TaskStatus::InProgress),
            ..TaskUpdate::default()
        },
    )
}

/// Mark a task as `done`.
pub fn done_task(tasks_dir: &Path, id: &str) -> Result<Task> {
    update_task(
        tasks_dir,
        id,
        TaskUpdate {
            status: Some(TaskStatus::Done),
            ..TaskUpdate::default()
        },
    )
}

fn write_task(tasks_dir: &Path, task: &Task) -> Result<()> {
    let path = task_path(tasks_dir, &task.id);
    let serialized = serde_json::to_vec_pretty(task).context("serialize task")?;
    write_atomic(&path, &serialized)
}

fn matches_filter(task: &Task, filter: &TaskFilter) -> bool {
    if let Some(status) = filter.status
        && task.status != status
    {
        return false;
    }
    if let Some(assignee) = &filter.assignee
        && task.assignee.as_deref() != Some(assignee.as_str())
    {
        return false;
    }
    true
}

fn unique_task_id() -> String {
    format!("task-{}-{}", now_millis(), std::process::id())
}

fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized.starts_with('.') {
        format!("_{sanitized}")
    } else {
        sanitized
    }
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

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "via-task-store-{}-{}-{}",
            label,
            std::process::id(),
            crate::util::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn task_status_serializes_in_progress_as_snake_case() {
        let json = serde_json::to_string(&TaskStatus::InProgress).unwrap();
        assert_eq!(json, "\"in_progress\"");
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"doing\"").unwrap(),
            TaskStatus::InProgress
        );
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"inprogress\"").unwrap(),
            TaskStatus::InProgress
        );
    }

    #[test]
    fn create_and_get_round_trip() {
        let dir = temp_dir("create");
        let task = create_task(
            &dir,
            CreateTask {
                title: "Implement task store".to_string(),
                id: Some("phase2-store".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: Some("orchestrator".to_string()),
                body: Some("First milestone".to_string()),
            },
        )
        .unwrap();

        assert_eq!(task.id, "phase2-store");
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.created_by.as_deref(), Some("orchestrator"));

        let loaded = get_task(&dir, "phase2-store").unwrap().unwrap();
        assert_eq!(loaded, task);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_rejects_duplicate_id() {
        let dir = temp_dir("duplicate");
        create_task(
            &dir,
            CreateTask {
                title: "One".to_string(),
                id: Some("dup".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        let err = create_task(
            &dir,
            CreateTask {
                title: "Two".to_string(),
                id: Some("dup".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_filters_by_status_and_assignee() {
        let dir = temp_dir("list");
        create_task(
            &dir,
            CreateTask {
                title: "Queued".to_string(),
                id: Some("t1".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();
        claim_task(&dir, "t1", "coder").unwrap();
        create_task(
            &dir,
            CreateTask {
                title: "Also queued".to_string(),
                id: Some("t2".to_string()),
                assignee: Some("reviewer".to_string()),
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        let in_progress = list_tasks(
            &dir,
            &TaskFilter {
                status: Some(TaskStatus::InProgress),
                assignee: None,
            },
        )
        .unwrap();
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].id, "t1");

        let reviewer = list_tasks(
            &dir,
            &TaskFilter {
                status: None,
                assignee: Some("reviewer".to_string()),
            },
        )
        .unwrap();
        assert_eq!(reviewer.len(), 1);
        assert_eq!(reviewer[0].id, "t2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claim_and_done_update_status() {
        let dir = temp_dir("claim-done");
        create_task(
            &dir,
            CreateTask {
                title: "Work".to_string(),
                id: Some("work".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        let claimed = claim_task(&dir, "work", "agent").unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        assert_eq!(claimed.assignee.as_deref(), Some("agent"));
        assert!(claimed.updated_at >= claimed.created_at);

        let finished = done_task(&dir, "work").unwrap();
        assert_eq!(finished.status, TaskStatus::Done);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_partial_fields() {
        let dir = temp_dir("update");
        create_task(
            &dir,
            CreateTask {
                title: "Old title".to_string(),
                id: Some("upd".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        let updated = update_task(
            &dir,
            "upd",
            TaskUpdate {
                title: Some("New title".to_string()),
                status: Some(TaskStatus::Review),
                blocked_by: Some(vec!["other-task".to_string()]),
                ..TaskUpdate::default()
            },
        )
        .unwrap();

        assert_eq!(updated.title, "New title");
        assert_eq!(updated.status, TaskStatus::Review);
        assert_eq!(updated.blocked_by, vec!["other-task"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = temp_dir("missing");
        assert!(get_task(&dir, "nope").unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitize_id_blocks_traversal() {
        assert_eq!(sanitize_id("../etc/passwd"), "_.._etc_passwd");
        assert_eq!(
            task_path(Path::new("/tmp/tasks"), "../x").to_str().unwrap(),
            "/tmp/tasks/_.._x.json"
        );
    }
}
