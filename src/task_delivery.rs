//! Deliver task lifecycle events to agent mailboxes (and the mediator for ACP recipients).

use crate::agent_bus;
use crate::config::PRIMARY_PTY_AGENT_ID;
use crate::session::{self, SessionManifest};
use crate::task_store::{Task, TaskStatus};

const TASK_SENDER: &str = "tasks";

/// After a task is created or updated, notify the assignee and/or human reviewer when appropriate.
///
/// No-op when no live via instance is running (`VIA_SESSION` unset or stale). Delivery failures
/// are logged as warnings so the task write still succeeds.
pub fn deliver_task_notifications(task: &Task, previous: Option<&Task>, from: Option<&str>) {
    let Ok(session) = session::resolve_session() else {
        return;
    };
    let from = from.unwrap_or(TASK_SENDER);

    notify_task_changed(&session, task, previous);

    if entered_review(task, previous) {
        try_notify(
            &session,
            from,
            PRIMARY_PTY_AGENT_ID,
            format_task_message(task, "ready for review"),
            true,
        );
        notify_review_gate(&session, task);
    }

    if assignee_should_be_notified(task, previous, from) {
        if let Some(assignee) = task.assignee.as_deref() {
            try_notify(
                &session,
                from,
                assignee,
                format_task_message(task, "task update"),
                false,
            );
        }
    }
}

fn entered_review(task: &Task, previous: Option<&Task>) -> bool {
    task.status == TaskStatus::Review
        && previous.is_none_or(|prev| prev.status != TaskStatus::Review)
}

fn assignee_should_be_notified(task: &Task, previous: Option<&Task>, from: &str) -> bool {
    let Some(assignee) = task.assignee.as_deref() else {
        return false;
    };
    // `human` is a reserved role, not a registry pane. The review-gate path
    // (entered_review → notify primary agent + open review surface) handles
    // delivery to the human; skip the assignee-notify to avoid a spurious
    // "no agent named 'human' is registered" warning.
    if assignee == crate::config::HUMAN_ASSIGNEE_ID {
        return false;
    }
    if assignee == from {
        return false;
    }
    match previous {
        None => true,
        Some(prev) => prev.assignee != task.assignee || prev.status != task.status,
    }
}

fn format_task_message(task: &Task, headline: &str) -> String {
    let status = task_status_label(task.status);
    let mut lines = vec![format!(
        "[task:{}] {} — status={} title={}",
        task.id, headline, status, task.title
    )];
    if let Some(body) = task.body.as_deref().filter(|s| !s.is_empty()) {
        lines.push(String::new());
        lines.push(body.to_string());
    }
    lines.join("\n")
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Review => "review",
        TaskStatus::Done => "done",
        TaskStatus::Blocked => "blocked",
    }
}

fn try_notify(session: &SessionManifest, from: &str, to: &str, message: String, focus: bool) {
    if let Err(error) = agent_bus::send_to_registered_agent(session, from, to, message, focus, true)
    {
        eprintln!("warning: task saved but failed to notify '{to}': {error}");
    }
}

/// Tell the running via instance to open its review surface (Neovim diff, hunk
/// pane, …) for this task. Fire-and-forget: the mailbox notify above is the
/// durable fallback if the mediator is unavailable.
fn notify_review_gate(session: &SessionManifest, task: &Task) {
    let payload = serde_json::json!({
        "type": "review_requested",
        "task_id": task.id,
        "title": task.title,
    });
    if let Err(error) = agent_bus::notify_editor_socket(&session.editor_socket, &payload) {
        eprintln!(
            "warning: task saved but failed to signal review gate for '{}': {error}",
            task.id
        );
    }
}

/// Emit a granular `task_created` / `task_updated` signal over the editor socket
/// so the mediator (and eventually the Neovim PM UI) can patch its view without
/// a full reload. Fire-and-forget: the signal is an optimization, not a
/// correctness path — the store is the source of truth.
///
/// Public so `via agent assign` can emit the board signal without triggering
/// the assignee-notify path in [`deliver_task_notifications`] — `assign` owns
/// the explicit message delivery itself.
pub fn notify_task_changed(session: &SessionManifest, task: &Task, previous: Option<&Task>) {
    let payload = match previous {
        None => serde_json::json!({
            "type": "task_created",
            "id": task.id,
        }),
        Some(prev) => {
            let fields = changed_fields(prev, task);
            if fields.is_empty() {
                return;
            }
            serde_json::json!({
                "type": "task_updated",
                "id": task.id,
                "fields": fields,
            })
        }
    };
    if let Err(error) = agent_bus::notify_editor_socket(&session.editor_socket, &payload) {
        eprintln!(
            "warning: task saved but failed to signal task change for '{}': {error}",
            task.id
        );
    }
}

/// Compute which task fields changed between `prev` and `next`.
fn changed_fields(prev: &Task, next: &Task) -> Vec<String> {
    let mut fields = Vec::new();
    if prev.title != next.title {
        fields.push("title".to_string());
    }
    if prev.status != next.status {
        fields.push("status".to_string());
    }
    if prev.assignee != next.assignee {
        fields.push("assignee".to_string());
    }
    if prev.body != next.body {
        fields.push("body".to_string());
    }
    if prev.blocked_by != next.blocked_by {
        fields.push("blocked_by".to_string());
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{session_with_socket, temp_dir};
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    fn sample_task(status: TaskStatus, assignee: Option<&str>) -> Task {
        Task {
            id: "t1".to_string(),
            title: "Do thing".to_string(),
            status,
            assignee: assignee.map(str::to_string),
            blocked_by: Vec::new(),
            created_at: 1,
            updated_at: 2,
            created_by: None,
            body: Some("details".to_string()),
        }
    }

    #[test]
    fn notify_assignee_on_create() {
        let task = sample_task(TaskStatus::Queued, Some("coder"));
        assert!(assignee_should_be_notified(&task, None, TASK_SENDER));
    }

    #[test]
    fn skip_self_notification() {
        let task = sample_task(TaskStatus::InProgress, Some("coder"));
        assert!(!assignee_should_be_notified(&task, None, "coder"));
    }

    #[test]
    fn notify_on_status_change() {
        let prev = sample_task(TaskStatus::Queued, Some("coder"));
        let next = sample_task(TaskStatus::InProgress, Some("coder"));
        assert!(assignee_should_be_notified(&next, Some(&prev), TASK_SENDER));
    }

    #[test]
    fn skip_when_unchanged() {
        let prev = sample_task(TaskStatus::InProgress, Some("coder"));
        let next = sample_task(TaskStatus::InProgress, Some("coder"));
        assert!(!assignee_should_be_notified(
            &next,
            Some(&prev),
            TASK_SENDER
        ));
    }

    #[test]
    fn review_transition_detected_once() {
        let prev = sample_task(TaskStatus::InProgress, Some("coder"));
        let next = sample_task(TaskStatus::Review, Some("coder"));
        assert!(entered_review(&next, Some(&prev)));
        assert!(!entered_review(&next, Some(&next)));
    }

    #[test]
    fn message_includes_body() {
        let task = sample_task(TaskStatus::Review, None);
        let text = format_task_message(&task, "ready for review");
        assert!(text.contains("[task:t1]"));
        assert!(text.contains("details"));
    }

    /// `notify_review_gate` writes a `review_requested` JSON line to the editor
    /// socket so the mediator can open the review surface.
    #[tokio::test]
    async fn notify_review_gate_writes_socket_signal() {
        let dir = temp_dir("gate");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path.clone());

        let listener = UnixListener::bind(&socket_path).unwrap();
        let join = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            lines
                .next_line()
                .await
                .ok()
                .flatten()
                .expect("expected a review_requested line")
        });

        let task = sample_task(TaskStatus::Review, Some("coder"));
        notify_review_gate(&session, &task);

        let line = tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("timed out waiting for editor socket signal")
            .expect("reader task panicked");

        let payload: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(payload["type"], "review_requested");
        assert_eq!(payload["task_id"], "t1");
        assert_eq!(payload["title"], "Do thing");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `notify_review_gate` tolerates a missing mediator (no listener) without
    /// panicking — the mailbox notify is the durable fallback.
    #[test]
    fn notify_review_gate_tolerates_missing_listener() {
        let dir = temp_dir("gate-missing");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path);

        let task = sample_task(TaskStatus::Review, None);
        // No listener bound — should log a warning, not panic.
        notify_review_gate(&session, &task);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changed_fields_detects_all_field_changes() {
        let prev = sample_task(TaskStatus::Queued, Some("coder"));

        // Status change only.
        let next = sample_task(TaskStatus::InProgress, Some("coder"));
        assert_eq!(changed_fields(&prev, &next), vec!["status"]);

        // Assignee change only.
        let next = sample_task(TaskStatus::Queued, Some("reviewer"));
        assert_eq!(changed_fields(&prev, &next), vec!["assignee"]);

        // Title change only.
        let mut next = prev.clone();
        next.title = "New title".to_string();
        assert_eq!(changed_fields(&prev, &next), vec!["title"]);

        // Body change only.
        let mut next = prev.clone();
        next.body = Some("new body".to_string());
        assert_eq!(changed_fields(&prev, &next), vec!["body"]);

        // blocked_by change only.
        let mut next = prev.clone();
        next.blocked_by = vec!["t2".to_string()];
        assert_eq!(changed_fields(&prev, &next), vec!["blocked_by"]);

        // Multiple fields.
        let mut next = prev.clone();
        next.status = TaskStatus::Review;
        next.assignee = Some("human".to_string());
        assert_eq!(changed_fields(&prev, &next), vec!["status", "assignee"]);

        // No changes.
        assert!(changed_fields(&prev, &prev).is_empty());
    }

    /// `notify_task_changed` emits `task_created` when `previous` is None.
    #[tokio::test]
    async fn notify_task_changed_emits_task_created() {
        let dir = temp_dir("task-created");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path.clone());

        let listener = UnixListener::bind(&socket_path).unwrap();
        let join = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            lines
                .next_line()
                .await
                .ok()
                .flatten()
                .expect("expected a line")
        });

        let task = sample_task(TaskStatus::Queued, Some("coder"));
        notify_task_changed(&session, &task, None);

        let line = tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("timed out")
            .expect("reader panicked");
        let payload: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(payload["type"], "task_created");
        assert_eq!(payload["id"], "t1");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `notify_task_changed` emits `task_updated` with the changed field names.
    #[tokio::test]
    async fn notify_task_changed_emits_task_updated_with_fields() {
        let dir = temp_dir("task-updated");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path.clone());

        let listener = UnixListener::bind(&socket_path).unwrap();
        let join = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            lines
                .next_line()
                .await
                .ok()
                .flatten()
                .expect("expected a line")
        });

        let prev = sample_task(TaskStatus::Queued, Some("coder"));
        let next = sample_task(TaskStatus::Review, Some("human"));
        notify_task_changed(&session, &next, Some(&prev));

        let line = tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("timed out")
            .expect("reader panicked");
        let payload: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(payload["type"], "task_updated");
        assert_eq!(payload["id"], "t1");
        let fields = payload["fields"].as_array().expect("fields array");
        let field_names: Vec<&str> = fields.iter().map(|f| f.as_str().unwrap()).collect();
        assert!(field_names.contains(&"status"));
        assert!(field_names.contains(&"assignee"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `notify_task_changed` skips the signal when nothing changed.
    #[tokio::test]
    async fn notify_task_changed_skips_when_no_fields_changed() {
        let dir = temp_dir("task-noop");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path.clone());

        // Bind a listener so the socket connect succeeds; we expect no line.
        let listener = UnixListener::bind(&socket_path).unwrap();
        let join = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            lines.next_line().await.ok().flatten()
        });

        let task = sample_task(TaskStatus::Queued, Some("coder"));
        notify_task_changed(&session, &task, Some(&task));

        // No signal should be written — the reader should get None (EOF) or time out.
        let result = tokio::time::timeout(Duration::from_millis(200), join).await;
        match result {
            Ok(Ok(None)) | Ok(Err(_)) => {} // EOF or error — no line was written
            Ok(Ok(Some(line))) => panic!("unexpected signal: {line}"),
            Err(_) => {} // timed out waiting — also fine, means no signal was sent
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `notify_task_changed` tolerates a missing mediator without panicking.
    #[test]
    fn notify_task_changed_tolerates_missing_listener() {
        let dir = temp_dir("task-missing");
        let socket_path = dir.join("editor.sock");
        let session = session_with_socket(socket_path);

        let task = sample_task(TaskStatus::Queued, None);
        notify_task_changed(&session, &task, None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
