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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManifest;
    use crate::task_store::Task;
    use std::path::PathBuf;
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

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "via-task-delivery-{label}-{}-{}",
            std::process::id(),
            crate::util::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session_with_socket(socket: PathBuf) -> SessionManifest {
        SessionManifest {
            pid: std::process::id(),
            cwd: PathBuf::from("/repo"),
            nvim_socket: PathBuf::new(),
            editor_socket: socket,
            agents_dir: PathBuf::new(),
            orchestration_enabled: false,
            workspace_id: None,
            board_id: None,
            started_at_unix: None,
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
}
