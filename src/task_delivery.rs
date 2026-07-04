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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::Task;

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
}
