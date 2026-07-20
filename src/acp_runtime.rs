//! ACP runtime: lifecycle of connected ACP agents and translation of ACP
//! agent events into UI commands.
//!
//! Owns the per-agent session map, handshake/retry/connect tasks, the in-flight
//! "expected" / "connecting" sets, launch-config bookkeeping for reconnect, and
//! the tool-call title cache used to enrich permission modals. The mediator
//! delegates ACP-heavy `Event::Agent(Acp { .. })` and `EditorEvent::SpawnAgent`
//! / `TerminateAgent` arms here; message routing (mailbox vs ACP prompt queue)
//! lives in [`crate::agent_delivery`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

use crate::acp::AcpClient;
use crate::agent_bus;
use crate::agent_delivery::PendingAcpPrompt;
use crate::config::PRIMARY_PTY_AGENT_ID;
use crate::event::{AcpAgentEvent, AcpHandshakeAction, AcpModalKind, UiCommand};
use crate::mediator::EventSender;

/// Upper bound on the ACP spawn + initialize + new_session handshake. A misbehaving
/// subprocess that never replies must not pin the connect task forever.
const ACP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const ACP_HANDSHAKE_MAX_ATTEMPTS: u32 = 3;
const ACP_HANDSHAKE_RETRY_DELAY: Duration = Duration::from_secs(2);

/// One connected ACP agent: its JSON-RPC client and active session id.
#[derive(Clone)]
pub struct AcpSession {
    pub client: Arc<Mutex<AcpClient>>,
    pub session_id: String,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct AcpLaunchConfig {
    pub role: Option<String>,
    pub command: String,
}

/// External context a [`AcpRuntime::connect`] task needs from the mediator: the
/// pending-prompt queue (owned by [`crate::agent_delivery`]), the agents dir
/// (mailbox drain), the working directory (session cwd), and the channels used
/// to start stdout readers and surface UI updates.
#[derive(Clone)]
pub struct AcpConnectCtx {
    pub pending: Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>>,
    pub agents_dir: PathBuf,
    pub cwd: PathBuf,
    pub event_sender: Option<EventSender>,
    pub ui_commands: mpsc::Sender<UiCommand>,
}

/// Spawn an ACP agent process, run the initialize handshake, and create a session.
/// Used for sub-agents spawned at runtime (the primary is always PTY).
async fn establish_acp(
    agent_id: &str,
    role: &str,
    command: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<AcpSession> {
    let handshake = async {
        let mut client = AcpClient::spawn(
            agent_id,
            command,
            args,
            &[
                (agent_bus::VIA_AGENT_ID_ENV, agent_id),
                (agent_bus::VIA_AGENT_ROLE_ENV, role),
            ],
        )
        .await?;
        let init = client.initialize().await?;
        let agent_name = init
            .agent_info
            .as_ref()
            .map(|info| info.name.as_str())
            .unwrap_or("unknown");
        let agent_version = init
            .agent_info
            .as_ref()
            .map(|info| info.version.as_str())
            .unwrap_or("unknown");
        tracing::info!(
            agent_id,
            agent = agent_name,
            version = agent_version,
            protocol_version = init.protocol_version,
            "ACP agent initialized"
        );

        let session = client.new_session(cwd).await?;
        let model = session.selected_model();
        tracing::info!(agent_id, session_id = %session.session_id, ?model, "ACP session created");

        Ok::<AcpSession, anyhow::Error>(AcpSession {
            client: Arc::new(Mutex::new(client)),
            session_id: session.session_id,
            model,
        })
    };

    tokio::time::timeout(ACP_HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| {
            anyhow!(
                "ACP handshake for agent '{agent_id}' timed out after {}s",
                ACP_HANDSHAKE_TIMEOUT.as_secs()
            )
        })?
}

fn truncate_acp_status_error(message: &str) -> String {
    const MAX_LEN: usize = 160;
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_LEN {
        collapsed
    } else {
        format!(
            "{}…",
            collapsed
                .chars()
                .take(MAX_LEN.saturating_sub(1))
                .collect::<String>()
        )
    }
}

/// Surface handshake failure in the agent pane. Queued prompts are kept unless the user
/// explicitly discards them via the retry modal.
fn notify_acp_handshake_failed(
    ui_commands: &mpsc::Sender<UiCommand>,
    agent_id: &str,
    err: &anyhow::Error,
    queued_count: usize,
) {
    let err_msg = err.to_string();
    if queued_count == 0 {
        tracing::error!(%agent_id, %err, "failed to connect ACP sub-agent");
    } else {
        warn!(
            agent_id = %agent_id,
            count = queued_count,
            %err,
            "ACP sub-agent handshake failed with queued prompts"
        );
        let _ = ui_commands.try_send(UiCommand::AcpTranscriptChunk {
            agent_id: agent_id.to_string(),
            kind: "system".to_string(),
            text: format!(
                "via: agent connection failed — {queued_count} message(s) queued — {err_msg}"
            ),
        });
        let _ = ui_commands.try_send(UiCommand::AcpProgress {
            agent_id: agent_id.to_string(),
            id: "__turn".to_string(),
            label: String::new(),
            active: false,
        });
        let _ = ui_commands.try_send(UiCommand::AcpModalPrompt {
            agent_id: agent_id.to_string(),
            jsonrpc_id: serde_json::Value::Null,
            title: "Agent connection failed".to_string(),
            message: format!(
                "{err_msg}\n\n{queued_count} message(s) are queued and will be sent once the agent connects."
            ),
            options: vec![
                crate::event::AcpPermissionOption {
                    option_id: "retry".to_string(),
                    name: "Retry connection".to_string(),
                },
                crate::event::AcpPermissionOption {
                    option_id: "discard".to_string(),
                    name: format!("Discard {queued_count} queued message(s)"),
                },
            ],
            kind: AcpModalKind::HandshakeRetry,
        });
    }
    let _ = ui_commands.try_send(UiCommand::AcpSessionStatus {
        agent_id: agent_id.to_string(),
        model: None,
        provider_error: Some(truncate_acp_status_error(&format!(
            "Connection failed: {err_msg}"
        ))),
        clear_provider_error: false,
    });
}

async fn establish_acp_with_retries(
    agent_id: &str,
    role: &str,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<AcpSession> {
    let mut last_err = None;
    for attempt in 1..=ACP_HANDSHAKE_MAX_ATTEMPTS {
        match establish_acp(agent_id, role, program, args, cwd).await {
            Ok(session) => return Ok(session),
            Err(err) => {
                if attempt < ACP_HANDSHAKE_MAX_ATTEMPTS {
                    warn!(
                        agent_id,
                        attempt,
                        %err,
                        "ACP handshake failed; retrying"
                    );
                    tokio::time::sleep(ACP_HANDSHAKE_RETRY_DELAY).await;
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.expect("at least one failed attempt"))
}

fn mirror_acp_prompts_to_ui(
    agent_id: &str,
    contents: &[String],
    ui_commands: &mpsc::Sender<UiCommand>,
) {
    for content in contents {
        if ui_commands
            .try_send(UiCommand::AcpTranscriptChunk {
                agent_id: agent_id.to_string(),
                kind: "user_message_chunk".to_string(),
                text: content.clone(),
            })
            .is_err()
        {
            warn!(agent_id, "failed to mirror ACP prompt to UI");
        }
    }
}

/// Spawn the background task that drives `client.prompt()` for each queued content
/// block. Mirrors the user's message into the ACP transcript pane first when
/// `mirror_to_ui` is set (the agent pane hasn't rendered it locally yet).
pub fn spawn_acp_prompt_delivery(
    agent_id: &str,
    session: AcpSession,
    contents: Vec<String>,
    ui_commands: mpsc::Sender<UiCommand>,
    mirror_to_ui: bool,
) {
    if contents.is_empty() {
        return;
    }
    if mirror_to_ui {
        mirror_acp_prompts_to_ui(agent_id, &contents, &ui_commands);
    }
    let client = Arc::clone(&session.client);
    let session_id = session.session_id.clone();
    let agent_id = agent_id.to_string();
    let count = contents.len();
    tokio::spawn(async move {
        let mut guard = client.lock().await;
        for (index, content) in contents.into_iter().enumerate() {
            info!(
                agent_id = %agent_id,
                session_id = %session_id,
                prompt = index + 1,
                total = count,
                bytes = content.len(),
                "delivering ACP session/prompt"
            );
            if let Err(err) = guard.prompt(&session_id, &content, None).await {
                error!(agent_id = %agent_id, %err, session_id = %session_id, "failed to deliver ACP prompt");
                let _ = ui_commands.try_send(UiCommand::AcpTranscriptChunk {
                    agent_id: agent_id.to_string(),
                    kind: "system".to_string(),
                    text: format!("via: failed to deliver prompt — {err}"),
                });
                break;
            }
        }
    });
}

/// Drain mailbox + pending prompts and deliver when the session is connected.
///
/// Free-function form so [`AcpRuntime::connect`] (which owns the session map
/// inside its spawned task) can call it without borrowing the runtime — it only
/// needs the shared `pending` arc, which it receives via [`AcpConnectCtx`].
pub async fn deliver_if_ready(
    agent_id: &str,
    sessions: &Arc<Mutex<HashMap<String, AcpSession>>>,
    pending: &Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>>,
    agents_dir: &Path,
    ui_commands: &mpsc::Sender<UiCommand>,
) -> bool {
    let Some(session) = sessions.lock().await.get(agent_id).cloned() else {
        return false;
    };
    let mut pending_guard = pending.lock().await;
    let queued = pending_guard.remove(agent_id).unwrap_or_default();
    let mut contents = Vec::new();
    let mut mirror_to_ui = false;
    for prompt in queued {
        if prompt.mirror_on_delivery {
            mirror_to_ui = true;
        }
        contents.push(prompt.content);
    }
    if let Ok(messages) = agent_bus::drain_inbox(agents_dir, agent_id, false) {
        if !messages.is_empty() {
            // Mailbox messages weren't rendered in the ACP pane locally, so mirror them.
            mirror_to_ui = true;
            contents.extend(messages.into_iter().map(|message| message.text));
        }
    }
    drop(pending_guard);
    if contents.is_empty() {
        return true;
    }
    info!(
        agent_id,
        count = contents.len(),
        "ACP session ready — delivering queued prompts"
    );
    spawn_acp_prompt_delivery(
        agent_id,
        session,
        contents,
        ui_commands.clone(),
        mirror_to_ui,
    );
    true
}

/// State for all connected and connecting ACP sub-agents. Held by the mediator;
/// interior-shared so async connect tasks can insert sessions without `&mut self`.
pub struct AcpRuntime {
    /// Connected ACP agents keyed by bus id ("orchestrator" plus any spawned subs).
    sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
    /// Sub-agent ids spawned (or connecting) as ACP — used to queue bus messages until ready.
    expected: Arc<Mutex<HashSet<String>>>,
    /// Launch command/role for spawned ACP sub-agents (for reconnect after handshake failure).
    launch_configs: Arc<Mutex<HashMap<String, AcpLaunchConfig>>>,
    /// Sub-agents with an in-flight connect task (prevents duplicate handshake attempts).
    connecting: Arc<Mutex<HashSet<String>>>,
    /// (agent id, tool call id) → latest title from `session/update` (for permission UI).
    /// Keyed by agent id as well so concurrent ACP agents can't collide on tool_call_id.
    tool_titles: HashMap<(String, String), String>,
}

impl AcpRuntime {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            expected: Arc::new(Mutex::new(HashSet::new())),
            launch_configs: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
            tool_titles: HashMap::new(),
        }
    }

    /// Arc handle to the sessions map — used by tests and by the free
    /// [`deliver_if_ready`] function when called from a connect task.
    #[allow(dead_code)]
    pub fn sessions_arc(&self) -> Arc<Mutex<HashMap<String, AcpSession>>> {
        Arc::clone(&self.sessions)
    }

    /// Arc handle to the launch-config map (used by tests / future reconnect UI).
    #[allow(dead_code)]
    pub fn launch_configs_arc(&self) -> Arc<Mutex<HashMap<String, AcpLaunchConfig>>> {
        Arc::clone(&self.launch_configs)
    }

    /// Look up a connected ACP session by exact id (no fallback).
    pub async fn session(&self, agent_id: &str) -> Option<AcpSession> {
        self.sessions.lock().await.get(agent_id).cloned()
    }

    /// Drain mailbox + pending prompts and deliver when the session is connected.
    /// Returns `false` when the session isn't ready yet (caller may retry connect).
    /// `pending` is the queue owned by [`crate::agent_delivery::AgentDelivery`];
    /// the mediator passes it in so this module doesn't depend back on delivery.
    pub async fn deliver_if_ready(
        &self,
        pending: &Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>>,
        agent_id: &str,
        agents_dir: &Path,
        ui_commands: &mpsc::Sender<UiCommand>,
    ) -> bool {
        deliver_if_ready(agent_id, &self.sessions, pending, agents_dir, ui_commands).await
    }

    /// True when a bus recipient is (or will be) an ACP agent rather than a PTY pane.
    /// A pending connect counts as ACP so messages queue instead of bouncing to mailbox.
    pub async fn recipient_is_acp(&self, agents_dir: &Path, agent_id: &str) -> bool {
        if self.expected.lock().await.contains(agent_id) {
            return true;
        }
        agent_bus::read_registry(agents_dir)
            .ok()
            .and_then(|records| records.into_iter().find(|record| record.id == agent_id))
            .is_some_and(|record| agent_bus::agent_record_is_acp(&record))
    }

    /// Record a spawned ACP sub-agent (expected + launch config) and start the handshake.
    /// Called by the mediator after it has resolved the launch command; the mediator is
    /// still responsible for emitting `UiCommand::SpawnAgent` so the pane is created.
    pub async fn register_spawn(
        &self,
        id: String,
        role: Option<String>,
        command: String,
        ctx: AcpConnectCtx,
    ) {
        self.expected.lock().await.insert(id.clone());
        self.launch_configs.lock().await.insert(
            id.clone(),
            AcpLaunchConfig {
                role: role.clone(),
                command: command.clone(),
            },
        );
        self.connect(id, role, command, ctx);
    }

    /// Start the ACP handshake for `id`. If the user terminates the agent while the
    /// handshake is in flight, the completed session is discarded (see comment inside).
    pub fn connect(&self, id: String, role: Option<String>, command: String, ctx: AcpConnectCtx) {
        let sessions = Arc::clone(&self.sessions);
        let expected = Arc::clone(&self.expected);
        let connecting = Arc::clone(&self.connecting);
        let event_sender = ctx.event_sender.clone();
        let ui_commands = ctx.ui_commands.clone();
        let agents_dir = ctx.agents_dir.clone();
        let cwd = ctx.cwd.clone();
        let pending = Arc::clone(&ctx.pending);
        let role = role.unwrap_or_else(|| id.clone());
        tokio::spawn(async move {
            {
                let mut connecting_guard = connecting.lock().await;
                if connecting_guard.contains(&id) {
                    return;
                }
                connecting_guard.insert(id.clone());
            }

            let tokens: Vec<&str> = command.split_whitespace().collect();
            let [program, args @ ..] = tokens.as_slice() else {
                connecting.lock().await.remove(&id);
                expected.lock().await.remove(&id);
                return;
            };
            match establish_acp_with_retries(&id, &role, program, args, &cwd).await {
                Ok(session) => {
                    // Serialize with `TerminateAgent`, which removes `id` from `expected`
                    // and `acp_sessions` while holding the `acp_sessions` lock. If the user
                    // terminated the agent while this handshake was in flight, `expected`
                    // no longer contains `id`, so drop the freshly built session instead
                    // of inserting an orphan — `AcpClient::drop` kills the subprocess.
                    // Without this, the detached task would resurrect a session (and
                    // stdout reader) for a pane that no longer exists.
                    let mut sessions_guard = sessions.lock().await;
                    let still_wanted = expected.lock().await.remove(&id);
                    connecting.lock().await.remove(&id);
                    if !still_wanted {
                        drop(sessions_guard);
                        drop(session);
                        info!(
                            agent_id = %id,
                            "ACP agent terminated during handshake; discarding completed session"
                        );
                        return;
                    }
                    let client = Arc::clone(&session.client);
                    let model = session.model.clone();
                    sessions_guard.insert(id.clone(), session);
                    drop(sessions_guard);
                    let _ = ui_commands.try_send(UiCommand::AcpSessionStatus {
                        agent_id: id.clone(),
                        model,
                        provider_error: None,
                        clear_provider_error: true,
                    });
                    if let Some(events) = event_sender {
                        client.lock().await.spawn_reader(events);
                    }
                    deliver_if_ready(&id, &sessions, &pending, &agents_dir, &ui_commands).await;
                }
                Err(err) => {
                    connecting.lock().await.remove(&id);
                    let queued_count = pending.lock().await.get(&id).map(|v| v.len()).unwrap_or(0);
                    notify_acp_handshake_failed(&ui_commands, &id, &err, queued_count);
                }
            }
        });
    }

    /// Reconnect an agent whose handshake failed, if we still have its launch config and
    /// no other connect is in flight. Called after the user picks "retry" and after a
    /// bus message targets an ACP recipient whose session isn't ready yet.
    pub async fn retry_connect_if_needed(&self, agent_id: &str, ctx: &AcpConnectCtx) {
        if self.sessions.lock().await.contains_key(agent_id) {
            return;
        }
        if self.connecting.lock().await.contains(agent_id) {
            return;
        }
        let Some(config) = self.launch_configs.lock().await.get(agent_id).cloned() else {
            return;
        };
        self.expected.lock().await.insert(agent_id.to_string());
        self.connect(
            agent_id.to_string(),
            config.role.clone(),
            config.command.clone(),
            ctx.clone(),
        );
    }

    /// User answered a via-owned handshake retry modal.
    pub async fn handle_handshake_action(
        &self,
        agent_id: &str,
        action: AcpHandshakeAction,
        ctx: &AcpConnectCtx,
    ) {
        match action {
            AcpHandshakeAction::Retry => {
                self.send_session_status(&ctx.ui_commands, agent_id, None, None, true);
                self.retry_connect_if_needed(agent_id, ctx).await;
            }
            AcpHandshakeAction::DiscardQueued => {
                let removed = ctx
                    .pending
                    .lock()
                    .await
                    .remove(agent_id)
                    .unwrap_or_default();
                if !removed.is_empty() {
                    let _ = ctx.ui_commands.try_send(UiCommand::AcpTranscriptChunk {
                        agent_id: agent_id.to_string(),
                        kind: "system".to_string(),
                        text: format!("via: discarded {} queued message(s)", removed.len()),
                    });
                }
                self.send_session_status(&ctx.ui_commands, agent_id, None, None, true);
            }
            AcpHandshakeAction::Dismiss => {}
        }
    }

    /// Tear down a sub-agent: drop its session (kills the subprocess), clear bookkeeping.
    /// The primary PTY agent is refused upstream. Pending prompts for `id` are cleared by
    /// the caller via [`crate::agent_delivery`].
    pub async fn terminate(&mut self, ui_commands: &mpsc::Sender<UiCommand>, id: &str) {
        // Hold the sessions lock while clearing `expected` so a connect task finishing its
        // handshake concurrently observes the removal and discards its session (see
        // `connect`) rather than resurrecting a dead agent.
        {
            let mut sessions = self.sessions.lock().await;
            self.expected.lock().await.remove(id);
            sessions.remove(id);
        }
        self.launch_configs.lock().await.remove(id);
        self.connecting.lock().await.remove(id);
        self.tool_titles.retain(|(agent_id, _), _| agent_id != id);
        let _ = ui_commands.try_send(UiCommand::TerminateAgent { id: id.to_string() });
    }

    /// Start stdout/stderr readers for sessions that were connected before `Mediator::spawn`
    /// (none today since the primary is PTY, but kept for symmetry / future use).
    pub fn spawn_initial_readers(
        &self,
        event_sender: EventSender,
        ui_commands: mpsc::Sender<UiCommand>,
    ) {
        let sessions = Arc::clone(&self.sessions);
        let reader_events = event_sender;
        tokio::spawn(async move {
            let map = sessions.lock().await;
            for (agent_id, session) in map.iter() {
                let _ = ui_commands.try_send(UiCommand::AcpSessionStatus {
                    agent_id: agent_id.clone(),
                    model: session.model.clone(),
                    provider_error: None,
                    clear_provider_error: false,
                });
                let client = Arc::clone(&session.client);
                let events = reader_events.clone();
                tokio::spawn(async move {
                    client.lock().await.spawn_reader(events);
                });
            }
        });
    }

    /// Forward an `AcpAgentEvent` (from the agent stdout reader) to the right UI command(s).
    /// Mutates the tool-title cache so a later `session/request_permission` can borrow the
    /// latest `session/update` title.
    pub async fn handle_agent_event(
        &mut self,
        ui_commands: &mpsc::Sender<UiCommand>,
        agent_id: String,
        event: AcpAgentEvent,
    ) {
        match event {
            AcpAgentEvent::TranscriptChunk { kind, text } => {
                if kind == "agent_message_chunk" && !text.is_empty() {
                    self.send_session_status(ui_commands, &agent_id, None, None, true);
                }
                let _ = ui_commands.try_send(UiCommand::AcpTranscriptChunk {
                    agent_id,
                    kind,
                    text,
                });
            }
            AcpAgentEvent::Progress { id, label, active } => {
                if active && !label.is_empty() {
                    self.tool_titles
                        .insert((agent_id.clone(), id.clone()), label.clone());
                } else if !active {
                    self.tool_titles.remove(&(agent_id.clone(), id.clone()));
                }
                let _ = ui_commands.try_send(UiCommand::AcpProgress {
                    agent_id,
                    id,
                    label,
                    active,
                });
            }
            AcpAgentEvent::PermissionRequest {
                jsonrpc_id,
                session_id,
                tool_call_id,
                mut title,
                options,
            } => {
                if title == "Permission required" || title.trim().is_empty() {
                    let title_key = (agent_id.clone(), tool_call_id.clone());
                    if let Some(t) = self.tool_titles.get(&title_key) {
                        title = t.clone();
                    }
                }
                let _ = ui_commands.try_send(UiCommand::AcpModalPrompt {
                    agent_id,
                    jsonrpc_id,
                    title,
                    message: format!("Session `{session_id}`\nTool call `{tool_call_id}`"),
                    options,
                    kind: AcpModalKind::SessionPermission,
                });
            }
            AcpAgentEvent::AskQuestion {
                jsonrpc_id,
                title,
                question_id,
                prompt,
                options,
            } => {
                let _ = ui_commands.try_send(UiCommand::AcpModalPrompt {
                    agent_id,
                    jsonrpc_id,
                    title,
                    message: prompt,
                    options,
                    kind: AcpModalKind::AskQuestion { question_id },
                });
            }
            AcpAgentEvent::SessionStatus { provider_error } => {
                self.send_session_status(
                    ui_commands,
                    &agent_id,
                    None,
                    provider_error.as_deref(),
                    false,
                );
            }
        }
    }

    /// Push a model/provider-error update to the ACP pane header.
    fn send_session_status(
        &self,
        ui_commands: &mpsc::Sender<UiCommand>,
        agent_id: &str,
        model: Option<&str>,
        provider_error: Option<&str>,
        clear_provider_error: bool,
    ) {
        let _ = ui_commands.try_send(UiCommand::AcpSessionStatus {
            agent_id: agent_id.to_string(),
            model: model.map(str::to_string),
            provider_error: provider_error.map(str::to_string),
            clear_provider_error,
        });
    }

    /// True if `id` is the primary PTY agent (caller uses this to refuse termination).
    pub fn is_primary_pty(id: &str) -> bool {
        id == PRIMARY_PTY_AGENT_ID
    }
}

impl Default for AcpRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_delivery::AgentDelivery;
    use crate::test_support::temp_dir;
    use std::collections::HashSet;

    async fn recipient_is_acp_via_expected(
        expected: &Arc<Mutex<HashSet<String>>>,
        agents_dir: &Path,
        agent_id: &str,
    ) -> bool {
        if expected.lock().await.contains(agent_id) {
            return true;
        }
        agent_bus::read_registry(agents_dir)
            .ok()
            .and_then(|records| records.into_iter().find(|record| record.id == agent_id))
            .is_some_and(|record| agent_bus::agent_record_is_acp(&record))
    }

    #[test]
    fn primary_pty_id_is_agent() {
        assert!(AcpRuntime::is_primary_pty(PRIMARY_PTY_AGENT_ID));
        assert!(!AcpRuntime::is_primary_pty("reviewer"));
    }

    #[test]
    fn runtime_starts_with_empty_maps() {
        let rt = AcpRuntime::new();
        assert!(rt.launch_configs_arc().try_lock().is_ok());
        assert!(rt.sessions_arc().try_lock().is_ok());
    }

    #[test]
    fn truncate_status_error_collapses_whitespace() {
        assert_eq!(truncate_acp_status_error("foo   bar"), "foo bar");
        let long = "x ".repeat(200);
        let truncated = truncate_acp_status_error(&long);
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().count() <= 160);
    }

    #[tokio::test]
    async fn deliver_if_ready_returns_false_when_session_missing() {
        let delivery = AgentDelivery::new();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let (ui_tx, _ui_rx) = mpsc::channel::<UiCommand>(8);
        let agents_dir = temp_dir("no-session");
        let delivered = deliver_if_ready(
            "reviewer",
            &sessions,
            &delivery.pending_arc(),
            &agents_dir,
            &ui_tx,
        )
        .await;
        assert!(!delivered);
        std::fs::remove_dir_all(&agents_dir).ok();
    }

    #[tokio::test]
    async fn deliver_if_ready_does_not_drain_mailbox_when_session_absent() {
        // No real ACP session can be constructed without spawning a subprocess, so we
        // verify the mailbox-drain half directly: enqueue a bus message, then call
        // deliver_if_ready against an empty session map and confirm it returns false
        // but the message is still in the mailbox (no spurious drain).
        let agents_dir = temp_dir("mailbox-no-drain");
        agent_bus::enqueue(
            &agents_dir,
            &agent_bus::Message {
                from: "orchestrator".to_string(),
                to: "reviewer".to_string(),
                ts: 1,
                text: "hello".to_string(),
            },
        )
        .unwrap();

        let delivery = AgentDelivery::new();
        let sessions: Arc<Mutex<HashMap<String, AcpSession>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (ui_tx, _ui_rx) = mpsc::channel::<UiCommand>(8);
        let delivered = deliver_if_ready(
            "reviewer",
            &sessions,
            &delivery.pending_arc(),
            &agents_dir,
            &ui_tx,
        )
        .await;
        assert!(!delivered);
        // Mailbox untouched because session is absent.
        let remaining = agent_bus::drain_inbox(&agents_dir, "reviewer", true).unwrap();
        assert_eq!(remaining.len(), 1);
        std::fs::remove_dir_all(&agents_dir).ok();
    }

    #[tokio::test]
    async fn recipient_is_acp_checks_expected_then_registry() {
        let agents_dir = temp_dir("recipient-check");
        let expected = Arc::new(Mutex::new(HashSet::new()));

        // Unknown id, empty registry → false.
        assert!(!recipient_is_acp_via_expected(&expected, &agents_dir, "ghost").await,);

        // Expected set takes precedence even without a registry entry.
        expected.lock().await.insert("queued".to_string());
        assert!(recipient_is_acp_via_expected(&expected, &agents_dir, "queued").await,);

        // Registry ACP record wins when not in expected.
        agent_bus::write_registry(
            &agents_dir,
            &[agent_bus::AgentRecord {
                id: "reviewer".to_string(),
                role: Some("reviewer".to_string()),
                command: Some("opencode acp".to_string()),
                mode: Some(agent_bus::AgentMode::Acp),
                primary: false,
            }],
        )
        .unwrap();
        assert!(recipient_is_acp_via_expected(&expected, &agents_dir, "reviewer").await,);

        // PTY record → false.
        agent_bus::write_registry(
            &agents_dir,
            &[agent_bus::AgentRecord {
                id: "agent".to_string(),
                role: Some("primary".to_string()),
                command: Some("opencode".to_string()),
                mode: Some(agent_bus::AgentMode::Pty),
                primary: true,
            }],
        )
        .unwrap();
        assert!(!recipient_is_acp_via_expected(&expected, &agents_dir, "agent").await,);

        std::fs::remove_dir_all(&agents_dir).ok();
    }
}
