//! File-backed task board for structured work items.
//!
//! One file per task lives directly under a work-session tasks directory
//! (see [`crate::workspace`]). New tasks are written as Markdown with YAML
//! frontmatter (`<id>.md`), using the same atomic-write pattern as
//! [`crate::agent_bus`].

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

/// YAML frontmatter for a task file. The `id` is NOT stored here — it is
/// derived from the filename stem. The `body` lives after the closing `---`
/// fence, not in the frontmatter. Skip rules mirror [`Task`] so files stay minimal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskFrontmatter {
    title: String,
    status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    blocked_by: Vec<String>,
    created_at: u64,
    updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
}

fn task_path(tasks_dir: &Path, id: &str) -> PathBuf {
    tasks_dir.join(format!("{}.md", sanitize_id(id)))
}

/// Return the on-disk Markdown path for a task id.
pub fn task_file_path(tasks_dir: &Path, id: &str) -> PathBuf {
    task_path(tasks_dir, id)
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
        None => unique_task_id(tasks_dir)?,
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

/// Load a single task by id. Returns `None` if the Markdown file does not
/// exist. Read paths are side-effect-free.
pub fn get_task(tasks_dir: &Path, id: &str) -> Result<Option<Task>> {
    let path = task_path(tasks_dir, id);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let task = parse_md_task(id, &contents)
                .with_context(|| format!("parse task {}", path.display()))?;
            Ok(Some(task))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read task {}", path.display())),
    }
}

/// List tasks, optionally filtered, ordered oldest-first by `created_at`.
/// Files with extensions other than `.md` are ignored.
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
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("read task {}", path.display()));
            }
        };
        let parsed = match parse_md_task(stem, &contents) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "skipping unparseable task file");
                continue;
            }
        };
        tasks.push(parsed);
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
    let serialized = task_to_md(task)?;
    write_atomic(&path, serialized.as_bytes())?;
    Ok(())
}

/// Serialize a [`Task`] to the on-disk Markdown-with-YAML-frontmatter format.
///
/// Layout: `---\n<yaml>\n---\n[\n<body>]`. When `body` is `None` the file ends
/// at the closing fence (no blank line); when `body` is `Some(s)` a blank
/// line separator is emitted followed by `s`. This distinguishes `None`
/// (absent body) from `Some("")` (present-but-empty body) on round-trip.
fn task_to_md(task: &Task) -> Result<String> {
    let fm = TaskFrontmatter {
        title: task.title.clone(),
        status: task.status,
        assignee: task.assignee.clone(),
        blocked_by: task.blocked_by.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        created_by: task.created_by.clone(),
    };
    let yaml = serde_yaml::to_string(&fm).context("serialize task frontmatter")?;
    let mut out = String::with_capacity(yaml.len() + 16);
    out.push_str("---\n");
    out.push_str(&yaml);
    out.push_str("---\n");
    if let Some(body) = &task.body {
        out.push('\n');
        out.push_str(body);
    }
    Ok(out)
}

/// Parse a Markdown-with-YAML-frontmatter task file into a [`Task`].
///
/// The file layout is `---\n<yaml>\n---\n[\n<body>]`. The splitter cuts on the
/// FIRST closing `---` line only: the body is free-form markdown and may
/// itself contain a `---` line (a markdown horizontal rule). A greedy match
/// on the closing fence would truncate the body at the first in-body rule and
/// corrupt the task, so [`str::splitn`] with `n = 2` is used to stop after the
/// first hit.
///
/// Body semantics: if nothing follows the closing fence, `body` is `None`
/// (absent). If a blank line follows and then nothing, `body` is `Some("")`
/// (present but empty). Otherwise `body` is the text after the blank-line
/// separator. The `id` is taken from the filename stem, not the frontmatter.
fn parse_md_task(id: &str, contents: &str) -> Result<Task> {
    let after_open = contents
        .strip_prefix("---\n")
        .with_context(|| format!("task {id} missing opening frontmatter fence"))?;

    let mut parts = after_open.splitn(2, "\n---\n");
    let yaml = parts.next().unwrap_or("");
    let body_rest = parts
        .next()
        .with_context(|| format!("task {id} missing closing frontmatter fence"))?;

    let fm: TaskFrontmatter =
        serde_yaml::from_str(yaml).with_context(|| format!("task {id} parse frontmatter"))?;

    let body = if body_rest.is_empty() {
        None
    } else {
        Some(
            body_rest
                .strip_prefix('\n')
                .unwrap_or(body_rest)
                .to_string(),
        )
    };

    Ok(Task {
        id: id.to_string(),
        title: fm.title,
        status: fm.status,
        assignee: fm.assignee,
        blocked_by: fm.blocked_by,
        created_at: fm.created_at,
        updated_at: fm.updated_at,
        created_by: fm.created_by,
        body,
    })
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

/// Length of auto-generated task ids (base36). Kept short so the Neovim
/// `:ViaTasks` id column (`via:` + id, width 20) never truncates them.
const AUTO_TASK_ID_LEN: usize = 4;
const AUTO_TASK_ID_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const AUTO_TASK_ID_ATTEMPTS: u32 = 64;

/// Allocate a short opaque id that is not already used on this board.
///
/// Ids are 4 base36 characters (~1.6M space). Collisions are resolved by
/// retrying with a different mix of time/pid/attempt; explicit `--id` values
/// are unchanged and may still be long human-readable names.
fn unique_task_id(tasks_dir: &Path) -> Result<String> {
    let mut seed = now_millis()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(std::process::id()));
    for attempt in 0..AUTO_TASK_ID_ATTEMPTS {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(u64::from(attempt) + 1);
        let id = encode_short_id(seed);
        if !task_path(tasks_dir, &id).exists() {
            return Ok(id);
        }
    }
    bail!("could not allocate a unique short task id");
}

fn encode_short_id(mut n: u64) -> String {
    let mut out = vec![b'0'; AUTO_TASK_ID_LEN];
    for i in (0..AUTO_TASK_ID_LEN).rev() {
        out[i] = AUTO_TASK_ID_ALPHABET[(n % 36) as usize];
        n /= 36;
    }
    // Alphabet is ASCII; this cannot fail.
    String::from_utf8(out).expect("short task id is ascii")
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
    use crate::test_support::temp_dir;

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

        assert_eq!(task_path(&dir, "phase2-store").extension().unwrap(), "md");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_generated_id_is_short_and_unique() {
        let dir = temp_dir("auto-id");
        let a = create_task(
            &dir,
            CreateTask {
                title: "First".to_string(),
                id: None,
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();
        let b = create_task(
            &dir,
            CreateTask {
                title: "Second".to_string(),
                id: None,
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        assert_eq!(a.id.len(), AUTO_TASK_ID_LEN);
        assert_eq!(b.id.len(), AUTO_TASK_ID_LEN);
        assert!(a.id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(b.id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a.id, b.id);
        // Fits the Neovim board column: "via:" (4) + id must be <= COL.ID (20).
        assert!(format!("via:{}", a.id).len() <= 20);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encode_short_id_is_fixed_width_base36() {
        assert_eq!(encode_short_id(0), "0000");
        assert_eq!(encode_short_id(35), "000z");
        assert_eq!(encode_short_id(36), "0010");
        assert_eq!(encode_short_id(36u64.pow(4) - 1), "zzzz");
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
            "/tmp/tasks/_.._x.md"
        );
    }

    #[test]
    fn writes_markdown_with_frontmatter_and_body() {
        let dir = temp_dir("md-write");
        let task = create_task(
            &dir,
            CreateTask {
                title: "Implement task store".to_string(),
                id: Some("md1".to_string()),
                assignee: Some("agent".to_string()),
                blocked_by: vec!["other-id".to_string()],
                created_by: Some("orchestrator".to_string()),
                body: Some("First milestone".to_string()),
            },
        )
        .unwrap();

        let path = task_path(&dir, "md1");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.starts_with("---\n"),
            "missing opening fence: {contents:?}"
        );
        assert!(
            contents.lines().any(|l| l.starts_with("title:")),
            "missing title field: {contents:?}"
        );
        assert!(
            contents.lines().any(|l| l == "status: queued"),
            "missing status: queued: {contents:?}"
        );
        let close_count = contents.lines().filter(|l| *l == "---").count();
        assert_eq!(
            close_count, 2,
            "expected exactly two fence lines: {contents:?}"
        );
        let after_close = contents.split_once("\n---\n").unwrap().1;
        assert!(
            after_close.starts_with('\n'),
            "expected blank line after closing fence: {after_close:?}"
        );
        assert!(
            after_close.trim_end().ends_with("First milestone"),
            "expected body at end: {after_close:?}"
        );

        let loaded = get_task(&dir, "md1").unwrap().unwrap();
        assert_eq!(loaded, task);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_md_task_rejects_missing_closing_fence() {
        let dir = temp_dir("no-close-fence");
        let malformed = "---\ntitle: Broken\nstatus: queued\ncreated_at: 1\nupdated_at: 1\n";
        let path = task_path(&dir, "broken1");
        write_atomic(&path, malformed.as_bytes()).unwrap();

        assert!(get_task(&dir, "broken1").is_err());
        assert!(list_tasks(&dir, &TaskFilter::default()).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn body_with_separator_and_special_chars_round_trips() {
        let dir = temp_dir("body-sep");
        let body = "intro line\n---\n- list item\nkey: value\n\nfinal line".to_string();
        let task = create_task(
            &dir,
            CreateTask {
                title: "Tricky: body with rules".to_string(),
                id: Some("tricky".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: Some(body.clone()),
            },
        )
        .unwrap();

        let loaded = get_task(&dir, "tricky").unwrap().unwrap();
        assert_eq!(loaded.body.as_deref(), Some(body.as_str()));
        assert_eq!(loaded, task);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn title_with_colon_round_trips() {
        let dir = temp_dir("title-colon");
        let task = create_task(
            &dir,
            CreateTask {
                title: "Fix: the bug".to_string(),
                id: Some("colon1".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();
        let loaded = get_task(&dir, "colon1").unwrap().unwrap();
        assert_eq!(loaded.title, "Fix: the bug");
        assert_eq!(loaded, task);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_body_semantics_none_vs_some_empty() {
        let dir = temp_dir("body-empty");

        create_task(
            &dir,
            CreateTask {
                title: "No body".to_string(),
                id: Some("n1".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();
        let none_path = task_path(&dir, "n1");
        let none_contents = std::fs::read_to_string(&none_path).unwrap();
        assert!(
            none_contents.ends_with("---\n"),
            "None body should end at closing fence: {none_contents:?}"
        );
        let none_loaded = get_task(&dir, "n1").unwrap().unwrap();
        assert_eq!(none_loaded.body, None);

        create_task(
            &dir,
            CreateTask {
                title: "Empty body".to_string(),
                id: Some("e1".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: Some(String::new()),
            },
        )
        .unwrap();
        let empty_path = task_path(&dir, "e1");
        let empty_contents = std::fs::read_to_string(&empty_path).unwrap();
        assert!(
            empty_contents.ends_with("---\n\n"),
            "Some(\"\") body should end with closing fence + blank line: {empty_contents:?}"
        );
        let empty_loaded = get_task(&dir, "e1").unwrap().unwrap();
        assert_eq!(empty_loaded.body, Some(String::new()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
