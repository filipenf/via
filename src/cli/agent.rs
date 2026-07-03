use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::agent_bus::{self, Message};
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
    if id == crate::config::PRIMARY_PTY_AGENT_ID {
        bail!("refusing to terminate the primary agent pane");
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

    let envelope = Message {
        from: from.clone(),
        to: to.clone(),
        ts: crate::util::now_millis(),
        text: message.clone(),
    };
    // Always enqueue for durability (and so `--no-notify` / PTY agents can read it later).
    agent_bus::enqueue(&session.agents_dir, &envelope).context("queue message")?;

    if notify {
        // Mediator delivers ACP prompts; non-ACP recipients are mailbox-only.
        let payload = serde_json::json!({
            "type": "agent_send",
            "agent_id": to,
            "from": from,
            "content": message,
            "focus": focus,
        });
        if let Err(error) = agent_bus::notify_editor_socket(&session.editor_socket, &payload) {
            eprintln!("warning: queued message but failed to notify mediator: {error}");
        }
    }

    println!("sent message to '{to}'");
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
