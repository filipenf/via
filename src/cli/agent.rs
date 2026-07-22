use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::agent_bus;
use crate::config::PRIMARY_PTY_AGENT_ID;
use crate::session;

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List the agents currently running in this via session.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Ask via to spawn a new agent pane.
    Spawn {
        /// Unique id for the new agent.
        #[arg(long)]
        id: String,
        /// Optional role label (e.g. "reviewer", "coder").
        #[arg(long)]
        role: Option<String>,
        /// Optional command to run (defaults to the configured agent).
        #[arg(long)]
        command: Option<String>,
    },
    /// Close a sub-agent pane and tear down its session.
    Terminate {
        /// Agent id to terminate (not the primary PTY agent).
        #[arg(long)]
        id: String,
    },
    /// Send a message to another agent's mailbox (and deliver to ACP recipients).
    Send {
        /// Recipient agent id. Defaults to the orchestrator.
        #[arg(long)]
        to: Option<String>,
        /// Message body.
        #[arg(long, short = 'm')]
        message: String,
        /// Do not steal focus when delivering to an ACP recipient.
        #[arg(long)]
        no_focus: bool,
        /// Do not notify the mediator to deliver the message (mailbox only).
        #[arg(long)]
        no_notify: bool,
    },
    /// Spawn an agent if needed and deliver a message, optionally claiming a task.
    /// The board↔bus bridge: `--task TID` sets the task's assignee in the same
    /// call as delivery. `--id human` hands to the human (mailbox notify, no
    /// ACP prompt, no spawn).
    Assign {
        /// Agent id to assign to. Reserved `human` hands to the user.
        #[arg(long)]
        id: String,
        /// Optional role label (used when spawning).
        #[arg(long)]
        role: Option<String>,
        /// Optional command to run (used when spawning).
        #[arg(long)]
        command: Option<String>,
        /// Message body to deliver.
        #[arg(long, short = 'm')]
        message: String,
        /// Task id to claim on behalf of the agent (sets assignee + delivers).
        #[arg(long)]
        task: Option<String>,
        /// Do not steal focus when delivering to an ACP recipient.
        #[arg(long)]
        no_focus: bool,
    },
    /// Read messages addressed to this agent (from `VIA_AGENT_ID`).
    Inbox {
        #[arg(long)]
        json: bool,
        /// Show messages without removing them from the mailbox.
        #[arg(long)]
        peek: bool,
        /// Block until a message arrives or this many seconds elapse.
        #[arg(long, value_name = "SECONDS")]
        wait: Option<u64>,
    },
    /// Print this agent's identity and session.
    Whoami {
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::List { json } => run_list(json),
        AgentCommand::Spawn { id, role, command } => run_spawn(id, role, command),
        AgentCommand::Terminate { id } => run_terminate(id),
        AgentCommand::Send {
            to,
            message,
            no_focus,
            no_notify,
        } => run_send(to, message, !no_focus, !no_notify),
        AgentCommand::Assign {
            id,
            role,
            command,
            message,
            task,
            no_focus,
        } => run_assign(id, role, command, message, task, !no_focus),
        AgentCommand::Inbox { json, peek, wait } => run_inbox(json, peek, wait),
        AgentCommand::Whoami { json } => run_whoami(json),
    }
}

/// This agent's own id, from the env via injects into each pane.
fn self_id() -> Option<String> {
    std::env::var(agent_bus::VIA_AGENT_ID_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

fn run_list(json: bool) -> Result<()> {
    let session = session::resolve_session()?;
    let agents = agent_bus::read_registry(&session.agents_dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&agents)?);
        return Ok(());
    }

    if agents.is_empty() {
        println!("no agents registered in this session");
        return Ok(());
    }

    for agent in agents {
        let role = agent.role.as_deref().unwrap_or("-");
        let primary = if agent.primary { " (primary)" } else { "" };
        println!("{}\trole={}{}", agent.id, role, primary);
    }
    Ok(())
}

fn run_spawn(id: String, role: Option<String>, command: Option<String>) -> Result<()> {
    let session = session::resolve_session()?;
    if !session.orchestration_enabled {
        bail!(
            "orchestration is unavailable for this session (agent has no ACP mapping); \
             set a known agent (e.g. opencode) or use --acp-agent"
        );
    }
    if crate::config::is_reserved_agent_id(&id) {
        bail!(
            "'{id}' is a reserved agent id (primary PTY pane or human role), \
             not a spawnable pane; choose a different --id"
        );
    }
    let mut payload = serde_json::json!({
        "type": "spawn_agent",
        "id": id,
    });
    if let Some(role) = &role {
        payload["role"] = serde_json::Value::String(role.clone());
    }
    if let Some(command) = &command {
        payload["command"] = serde_json::Value::String(command.clone());
    }
    agent_bus::notify_editor_socket(&session.editor_socket, &payload)
        .context("ask via to spawn the agent")?;

    // Confirm via actually opened the pane (socket success alone is not enough).
    for _ in 0..30 {
        if let Ok(agents) = agent_bus::read_registry(&session.agents_dir) {
            if agents.iter().any(|agent| agent.id == id) {
                // Seed the helper with active-board context (mailbox + ACP notify).
                crate::task_delivery::deliver_spawn_board_snapshot(&session, &id);
                println!("spawned agent '{id}'");
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    eprintln!(
        "warning: via accepted the spawn request but '{id}' is not in the agent registry after 3s; \
         the pane may not have opened — run `via agent list` and check ~/.local/share/via/via-*/logs/via.log"
    );
    println!("requested spawn of agent '{id}' (unconfirmed)");
    Ok(())
}

fn run_terminate(id: String) -> Result<()> {
    let session = session::resolve_session()?;
    if crate::config::is_reserved_agent_id(&id) {
        bail!(
            "'{id}' is a reserved agent id (primary PTY pane or human role), \
             not a spawnable pane; choose a different --id"
        );
    }
    if agent_bus::read_registry(&session.agents_dir)?
        .into_iter()
        .any(|record| record.id == id && record.primary)
    {
        bail!("refusing to terminate the primary agent");
    }
    let payload = serde_json::json!({
        "type": "terminate_agent",
        "id": id,
    });
    agent_bus::notify_editor_socket(&session.editor_socket, &payload)
        .context("ask via to terminate the agent")?;
    println!("requested termination of agent '{id}'");
    Ok(())
}

fn run_send(to: Option<String>, message: String, focus: bool, notify: bool) -> Result<()> {
    let session = session::resolve_session()?;
    let to = to.unwrap_or_else(|| "orchestrator".to_string());
    let from = self_id().unwrap_or_else(|| "unknown".to_string());

    let agents = agent_bus::read_registry(&session.agents_dir)?;
    if !agents.iter().any(|agent| agent.id == to) {
        bail!("no agent named '{to}' is registered in this session");
    }

    agent_bus::send_to_registered_agent(&session, from, &to, message, focus, notify)?;

    println!("sent message to '{to}'");
    Ok(())
}

/// `via agent assign`: spawn if needed + deliver a message + optionally claim a
/// task. The board↔bus bridge.
fn run_assign(
    id: String,
    role: Option<String>,
    command: Option<String>,
    message: String,
    task: Option<String>,
    focus: bool,
) -> Result<()> {
    let session = session::resolve_session()?;
    let from = self_id().unwrap_or_else(|| "unknown".to_string());
    let is_human = id == crate::config::HUMAN_ASSIGNEE_ID;

    // For `human`, the "delivery" target is the primary agent pane's mailbox.
    // For other agents, spawn if not already registered.
    if !is_human {
        let already_registered = agent_bus::read_registry(&session.agents_dir)?
            .into_iter()
            .any(|record| record.id == id);

        if !already_registered {
            run_spawn(id.clone(), role, command)?;
        }
    }

    // Set task assignee if --task was given.
    if let Some(task_id) = &task {
        assign_task(&session.cwd, &id, task_id)?;
    }

    // if is_human, enqueue directly rather than relying on the registry
    if is_human {
        let envelope = agent_bus::Message {
            from,
            to: PRIMARY_PTY_AGENT_ID.to_string(),
            ts: crate::util::now_millis(),
            text: if task.is_some() {
                format!("[assign → human] {message}")
            } else {
                message
            },
        };
        agent_bus::enqueue(&session.agents_dir, &envelope)
            .context("queue message to primary agent for human")?;
    } else {
        agent_bus::send_to_registered_agent(&session, from, id.clone(), message, focus, true)?;
    }

    if task.is_some() {
        println!("assigned task to '{id}' and delivered message");
    } else {
        println!("assigned message to '{id}'");
    }
    Ok(())
}

/// Set a task's assignee to `agent_id` on the active board. The task store
/// resolves the workspace from the given cwd (session cwd or current dir).
/// Emits only the board-change signal — the caller (`run_assign`) owns the
/// explicit message delivery.
fn assign_task(cwd: &Path, agent_id: &str, task_id: &str) -> Result<()> {
    use crate::task_store::{TaskUpdate, get_task, update_task};
    use crate::workspace::resolve_tasks_context;

    let ctx = resolve_tasks_context(cwd)?;
    let previous = get_task(&ctx.tasks_dir, task_id)?;
    let Some(_) = &previous else {
        bail!("task not found: {task_id}");
    };

    let task = update_task(
        &ctx.tasks_dir,
        task_id,
        TaskUpdate {
            assignee: Some(Some(agent_id.to_string())),
            ..TaskUpdate::default()
        },
    )?;

    // Emit the board-change signal only. `run_assign` owns the explicit message
    // delivery to the assignee — calling `deliver_task_notifications` here would
    // send a second generic "task update" message on top of the assignment text.
    if let Ok(notify_session) = session::resolve_session() {
        crate::task_delivery::notify_task_changed(&notify_session, &task, previous.as_ref());
    }

    Ok(())
}

fn run_inbox(json: bool, peek: bool, wait: Option<u64>) -> Result<()> {
    let session = session::resolve_session()?;
    let Some(id) = self_id() else {
        bail!(
            "{} is not set; run this from inside a via agent pane",
            agent_bus::VIA_AGENT_ID_ENV
        );
    };

    let timeout = wait.map(Duration::from_secs);
    let messages = agent_bus::drain_inbox_with_wait(&session.agents_dir, &id, peek, timeout)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&messages)?);
        return Ok(());
    }

    if messages.is_empty() {
        println!("inbox empty");
        return Ok(());
    }

    for message in messages {
        println!("from {}: {}", message.from, message.text);
    }
    Ok(())
}

fn run_whoami(json: bool) -> Result<()> {
    let session = session::resolve_session()?;
    let id = self_id();
    let role = std::env::var(agent_bus::VIA_AGENT_ROLE_ENV)
        .ok()
        .filter(|s| !s.is_empty());

    if json {
        let payload = serde_json::json!({
            "id": id,
            "role": role,
            "pid": session.pid,
            "cwd": session.cwd,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!(
        "id={} role={} pid={} cwd={}",
        id.as_deref().unwrap_or("unknown"),
        role.as_deref().unwrap_or("-"),
        session.pid,
        session.cwd.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::{CreateTask, TaskStatus, create_task, get_task};
    use crate::test_support::{temp_dir, write_session_manifest};
    use crate::workspace::resolve_tasks_context;

    /// `via agent assign --id human -m "..."` enqueues a message to the primary
    /// agent's mailbox (not an ACP prompt) and does not try to spawn a pane.
    #[tokio::test]
    async fn assign_to_human_enqueues_to_primary_mailbox() {
        let dir = temp_dir("assign-human");
        let manifest_path = write_session_manifest(&dir);
        let agents_dir = dir.join("agents");

        // Register the primary agent so the mailbox target exists.
        agent_bus::write_registry(
            &agents_dir,
            &[agent_bus::AgentRecord {
                id: crate::config::PRIMARY_PTY_AGENT_ID.to_string(),
                role: Some("primary".to_string()),
                command: Some("opencode".to_string()),
                mode: Some(agent_bus::AgentMode::Pty),
                primary: true,
            }],
        )
        .unwrap();

        // Set VIA_SESSION so resolve_session finds our temp manifest.
        // Serialize with the global env lock because `cargo test` runs these
        // tests in parallel in the same process.
        let _env_guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("VIA_SESSION", &manifest_path);
        };

        let result = run_assign(
            "human".to_string(),
            None,
            None,
            "your turn to review".to_string(),
            None,
            true,
        );

        unsafe {
            std::env::remove_var("VIA_SESSION");
        };

        result.unwrap();

        // Message should be in the primary agent's mailbox.
        let messages = agent_bus::drain_inbox(&agents_dir, "agent", true).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.contains("your turn to review"));

        // No "human" pane should have been spawned.
        let registry = agent_bus::read_registry(&agents_dir).unwrap();
        assert!(!registry.iter().any(|r| r.id == "human"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `via agent assign --id human --task TID` sets the task's assignee to
    /// "human" on the active board.
    #[tokio::test]
    async fn assign_to_human_sets_task_assignee() {
        let dir = temp_dir("assign-human-task");
        let manifest_path = write_session_manifest(&dir);
        let agents_dir = dir.join("agents");

        agent_bus::write_registry(
            &agents_dir,
            &[agent_bus::AgentRecord {
                id: crate::config::PRIMARY_PTY_AGENT_ID.to_string(),
                role: Some("primary".to_string()),
                command: Some("opencode".to_string()),
                mode: Some(agent_bus::AgentMode::Pty),
                primary: true,
            }],
        )
        .unwrap();

        // Create a task on the board for this workspace.
        let ctx = resolve_tasks_context(&dir).unwrap();
        create_task(
            &ctx.tasks_dir,
            CreateTask {
                title: "Test task".to_string(),
                id: Some("assign-test".to_string()),
                assignee: None,
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        // Serialize with the global env lock because `cargo test` runs these
        // tests in parallel in the same process.
        let _env_guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("VIA_SESSION", &manifest_path);
        };

        let result = run_assign(
            "human".to_string(),
            None,
            None,
            "please review".to_string(),
            Some("assign-test".to_string()),
            true,
        );

        unsafe {
            std::env::remove_var("VIA_SESSION");
        };

        result.unwrap();

        // Task assignee should now be "human".
        let task = get_task(&ctx.tasks_dir, "assign-test").unwrap().unwrap();
        assert_eq!(task.assignee.as_deref(), Some("human"));
        assert_eq!(task.status, TaskStatus::Queued); // status unchanged

        std::fs::remove_dir_all(&dir).ok();
    }
}
