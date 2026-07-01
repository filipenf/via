use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use serde::{Deserialize, Serialize};

use crate::acp::{self, AcpClient, ContextUpdateParams, PromptResource};
use crate::agent_bus;
use crate::config::Config;
use crate::editor::{self, EditorState};
use crate::event::{
    AcpAgentEvent, AcpModalKind, AgentEvent, EditorEvent, Event, UiCommand, UiEvent,
};
use crate::lsp_bridge;
use crate::nvim::{self, FileTarget};

const EVENT_BUFFER_SIZE: usize = 128;

/// Upper bound on the ACP spawn + initialize + new_session handshake. A misbehaving
/// subprocess that never replies must not pin the connect task forever.
const ACP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const ACP_HANDSHAKE_MAX_ATTEMPTS: u32 = 3;
const ACP_HANDSHAKE_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct PendingAcpPrompt {
    content: String,
    /// False when the ACP pane already rendered the user message locally.
    mirror_on_delivery: bool,
}

#[derive(Clone)]
struct AcpLaunchConfig {
    role: Option<String>,
    command: String,
}

#[derive(Debug, Deserialize)]
struct AgentToolRequest {
    id: Option<String>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AgentToolResponse {
    id: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LspDefinitionArgs {
    uri: String,
    line: u32,
    character: u32,
}

fn serialize_tool_response(response: AgentToolResponse) -> String {
    match serde_json::to_string(&response) {
        Ok(payload) => format!("@tool_result {payload}\n"),
        Err(error) => format!(
            "@tool_result {{\"id\":null,\"ok\":false,\"error\":\"failed to serialize tool response: {}\"}}\n",
            error
        ),
    }
}

fn selection_resource_uri(path: &Path, start_line: u32, end_line: u32) -> String {
    format!("file://{}#L{start_line}-L{end_line}", path.display())
}

/// One connected ACP agent: its JSON-RPC client and active session id.
#[derive(Clone)]
struct AcpSession {
    client: Arc<Mutex<AcpClient>>,
    session_id: String,
    model: Option<String>,
}

/// Spawn an ACP agent process, run the initialize handshake, and create a session.
/// Used both for the primary at startup and for sub-agents spawned at runtime.
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
            kind: crate::event::AcpModalKind::HandshakeRetry,
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

fn spawn_acp_prompt_delivery(
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
async fn deliver_acp_prompts_if_ready(
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
        mirror_to_ui = true;
        contents.extend(messages.into_iter().map(|message| message.text));
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

use crate::config::{ORCHESTRATOR_AGENT_ID, PRIMARY_PTY_AGENT_ID};

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    ui_commands: mpsc::Sender<UiCommand>,
    editor_state: EditorState,
    in_flight_symbol_open: Option<JoinHandle<()>>,
    lsp_handle: Option<lsp_bridge::LspBridgeHandle>,
    agent_output_buffer: String,
    /// Connected ACP agents keyed by bus id ("orchestrator" plus any spawned subs).
    /// Shared so async connect tasks can insert new sessions without `&mut self`.
    acp_sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
    /// Event sender retained after `spawn()` so dynamically connected agents can start readers.
    event_sender: Option<EventSender>,
    /// (agent id, tool call id) → latest title from `session/update` (for permission UI).
    /// Keyed by agent id as well so concurrent ACP agents can't collide on tool_call_id.
    acp_tool_titles: HashMap<(String, String), String>,
    /// Prompts for ACP sub-agents that arrived before the handshake finished.
    pending_acp_prompts: Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>>,
    /// Sub-agent ids spawned (or connecting) as ACP — used to queue bus messages until ready.
    expected_acp_agents: Arc<Mutex<HashSet<String>>>,
    /// Launch command/role for spawned ACP sub-agents (for reconnect after handshake failure).
    acp_launch_configs: Arc<Mutex<HashMap<String, AcpLaunchConfig>>>,
    /// Sub-agents with an in-flight connect task (prevents duplicate handshake attempts).
    connecting_acp_agents: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone)]
pub struct EventSender {
    events: mpsc::Sender<Event>,
}

pub struct MediatorHandle {
    events: EventSender,
    ui_commands: Option<mpsc::Receiver<UiCommand>>,
    stopped: oneshot::Receiver<()>,
    editor_listener: JoinHandle<()>,
    lsp_listener: JoinHandle<()>,
    lsp_clients_forwarder: JoinHandle<()>,
    #[allow(dead_code)]
    lsp_handle: Option<lsp_bridge::LspBridgeHandle>,
}

impl Mediator {
    pub fn new(config: Config) -> Self {
        let (_events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (ui_commands_tx, _ui_commands_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        Self {
            config,
            events: events_rx,
            ui_commands: ui_commands_tx,
            editor_state: EditorState::default(),
            in_flight_symbol_open: None,
            lsp_handle: None,
            agent_output_buffer: String::new(),
            acp_sessions: Arc::new(Mutex::new(HashMap::new())),
            event_sender: None,
            acp_tool_titles: HashMap::new(),
            pending_acp_prompts: Arc::new(Mutex::new(HashMap::new())),
            expected_acp_agents: Arc::new(Mutex::new(HashSet::new())),
            acp_launch_configs: Arc::new(Mutex::new(HashMap::new())),
            connecting_acp_agents: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Look up a connected ACP session by exact id (no fallback).
    async fn acp_session(&self, agent_id: &str) -> Option<AcpSession> {
        self.acp_sessions.lock().await.get(agent_id).cloned()
    }

    /// True when a bus recipient is (or will be) an ACP agent rather than a PTY pane.
    async fn recipient_is_acp(&self, agent_id: &str) -> bool {
        if self.expected_acp_agents.lock().await.contains(agent_id) {
            return true;
        }
        agent_bus::read_registry(&self.config.agents_dir)
            .ok()
            .and_then(|records| records.into_iter().find(|record| record.id == agent_id))
            .is_some_and(|record| agent_bus::agent_record_is_acp(&record))
    }

    async fn queue_acp_prompt(&self, agent_id: &str, content: String, mirror_on_delivery: bool) {
        info!(agent_id, "queuing ACP prompt until session is ready");
        self.pending_acp_prompts
            .lock()
            .await
            .entry(agent_id.to_string())
            .or_default()
            .push(PendingAcpPrompt {
                content,
                mirror_on_delivery,
            });
    }

    async fn deliver_acp_prompts_if_ready(&self, agent_id: &str) -> bool {
        deliver_acp_prompts_if_ready(
            agent_id,
            &self.acp_sessions,
            &self.pending_acp_prompts,
            &self.config.agents_dir,
            &self.ui_commands,
        )
        .await
    }

    fn connect_acp_agent(&self, id: String, role: Option<String>, command: String) {
        let sessions = Arc::clone(&self.acp_sessions);
        let pending = Arc::clone(&self.pending_acp_prompts);
        let expected = Arc::clone(&self.expected_acp_agents);
        let connecting = Arc::clone(&self.connecting_acp_agents);
        let event_sender = self.event_sender.clone();
        let ui_commands = self.ui_commands.clone();
        let agents_dir = self.config.agents_dir.clone();
        let cwd = self.config.working_directory.clone();
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
                    let client = Arc::clone(&session.client);
                    let model = session.model.clone();
                    sessions.lock().await.insert(id.clone(), session);
                    expected.lock().await.remove(&id);
                    connecting.lock().await.remove(&id);
                    let _ = ui_commands.try_send(UiCommand::AcpSessionStatus {
                        agent_id: id.clone(),
                        model,
                        provider_error: None,
                        clear_provider_error: true,
                    });
                    if let Some(events) = event_sender {
                        client.lock().await.spawn_reader(events);
                    }
                    deliver_acp_prompts_if_ready(
                        &id,
                        &sessions,
                        &pending,
                        &agents_dir,
                        &ui_commands,
                    )
                    .await;
                }
                Err(err) => {
                    connecting.lock().await.remove(&id);
                    let queued_count = pending.lock().await.get(&id).map(|v| v.len()).unwrap_or(0);
                    notify_acp_handshake_failed(&ui_commands, &id, &err, queued_count);
                }
            }
        });
    }

    async fn retry_acp_connect_if_needed(&self, agent_id: &str) {
        if self.acp_sessions.lock().await.contains_key(agent_id) {
            return;
        }
        if self.connecting_acp_agents.lock().await.contains(agent_id) {
            return;
        }
        let Some(config) = self.acp_launch_configs.lock().await.get(agent_id).cloned() else {
            return;
        };
        self.expected_acp_agents
            .lock()
            .await
            .insert(agent_id.to_string());
        self.connect_acp_agent(
            agent_id.to_string(),
            config.role.clone(),
            config.command.clone(),
        );
    }

    async fn handle_acp_handshake_action(
        &self,
        agent_id: &str,
        action: crate::event::AcpHandshakeAction,
    ) {
        match action {
            crate::event::AcpHandshakeAction::Retry => {
                self.send_acp_session_status(agent_id, None, None, true);
                self.retry_acp_connect_if_needed(agent_id).await;
            }
            crate::event::AcpHandshakeAction::DiscardQueued => {
                let removed = self
                    .pending_acp_prompts
                    .lock()
                    .await
                    .remove(agent_id)
                    .unwrap_or_default();
                if !removed.is_empty() {
                    self.send_ui_command(UiCommand::AcpTranscriptChunk {
                        agent_id: agent_id.to_string(),
                        kind: "system".to_string(),
                        text: format!("via: discarded {} queued message(s)", removed.len()),
                    });
                }
                self.send_acp_session_status(agent_id, None, None, true);
            }
            crate::event::AcpHandshakeAction::Dismiss => {}
        }
    }

    fn send_acp_session_status(
        &self,
        agent_id: &str,
        model: Option<&str>,
        provider_error: Option<&str>,
        clear_provider_error: bool,
    ) {
        self.send_ui_command(UiCommand::AcpSessionStatus {
            agent_id: agent_id.to_string(),
            model: model.map(str::to_string),
            provider_error: provider_error.map(str::to_string),
            clear_provider_error,
        });
    }

    /// Resolve the ACP session to use for a prompt, preferring an explicit id, then the
    /// primary/orchestrator, then any single connected agent.
    async fn acp_session_for_prompt(&self, agent_id: Option<&str>) -> Option<AcpSession> {
        let map = self.acp_sessions.lock().await;
        if let Some(id) = agent_id {
            if let Some(session) = map.get(id) {
                return Some(session.clone());
            }
        }
        if let Some(session) = map.get(ORCHESTRATOR_AGENT_ID) {
            return Some(session.clone());
        }
        map.values().next().cloned()
    }

    pub fn spawn(mut self) -> MediatorHandle {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (ui_commands_tx, ui_commands_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        self.events = events_rx;
        self.ui_commands = ui_commands_tx;
        let events = EventSender {
            events: events_tx.clone(),
        };
        self.event_sender = Some(events.clone());

        // Start a stdout reader for each ACP agent connected before startup (the primary).
        let sessions = Arc::clone(&self.acp_sessions);
        let ui_commands = self.ui_commands.clone();
        let reader_events = events.clone();
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

        let editor_listener = editor::spawn_listener(
            self.config.editor_socket_path.clone(),
            self.config.working_directory.clone(),
            events.clone(),
        );

        let (lsp_handle, lsp_listener, _lsp_state, mut lsp_clients_updates) =
            lsp_bridge::spawn_listener(
                self.config.lsp_bridge_socket_path.clone(),
                self.config.working_directory.clone(),
            );
        self.lsp_handle = Some(lsp_handle.clone());

        let lsp_clients_forwarder = tokio::spawn(async move {
            while let Some(clients) = lsp_clients_updates.recv().await {
                debug!(
                    count = clients.len(),
                    "lsp clients updated (not forwarding to agent stdin)"
                );
                drop(clients);
            }
        });

        let (stopped_tx, stopped_rx) = oneshot::channel();
        tokio::spawn(async move {
            self.run().await;
            let _ = stopped_tx.send(());
        });

        MediatorHandle {
            events,
            ui_commands: Some(ui_commands_rx),
            stopped: stopped_rx,
            editor_listener,
            lsp_listener,
            lsp_clients_forwarder,
            lsp_handle: Some(lsp_handle),
        }
    }

    async fn run(&mut self) {
        info!(
            nvim_command = %self.config.nvim_command,
            nvim_socket = %self.config.nvim_socket_path.display(),
            editor_socket = %self.config.editor_socket_path.display(),
            agent_configured = self.config.agent_command.is_some(),
            "mediator ready"
        );

        while let Some(event) = self.events.recv().await {
            match event {
                Event::Shutdown => {
                    if let Some(task) = self.in_flight_symbol_open.take() {
                        task.abort();
                    }
                    info!("mediator received shutdown");
                    break;
                }
                Event::Ui(UiEvent::OpenRequested { path, line }) => {
                    let target = FileTarget { path, line };

                    if let Err(error) = nvim::open_file(
                        &self.config.nvim_socket_path,
                        &self.config.working_directory,
                        target,
                    )
                    .await
                    {
                        error!(%error, "failed to open file in Neovim");
                    }
                }
                Event::Ui(UiEvent::SymbolOpenRequested { symbol }) => {
                    if let Some(task) = self.in_flight_symbol_open.take() {
                        task.abort();
                    }

                    let socket_path = self.config.nvim_socket_path.clone();
                    self.in_flight_symbol_open = Some(tokio::spawn(async move {
                        if let Err(error) = nvim::open_symbol(&socket_path, &symbol).await {
                            error!(%error, symbol, "failed to open symbol in Neovim");
                        }
                    }));
                }
                Event::Ui(UiEvent::ReviewRequested) => {
                    if let Err(error) = nvim::open_review(
                        &self.config.nvim_socket_path,
                        &self.config.working_directory,
                    )
                    .await
                    {
                        error!(%error, "failed to open review in Neovim");
                    }
                }
                Event::Ui(UiEvent::AgentPromptSubmitted { text, agent_id }) => {
                    if text.trim().is_empty() {
                        continue;
                    }

                    let target = agent_id.as_deref().unwrap_or(ORCHESTRATOR_AGENT_ID);

                    let prompt_resource = self
                        .editor_state
                        .visual_selection
                        .as_ref()
                        .filter(|selection| !selection.text.trim().is_empty())
                        .map(|selection| PromptResource {
                            uri: selection_resource_uri(
                                &selection.path,
                                selection.start_line,
                                selection.end_line,
                            ),
                            mime_type: Some("text/plain".to_string()),
                            text: selection.text.clone(),
                        });

                    if let Some(session) = self.acp_session(target).await {
                        let client = Arc::clone(&session.client);
                        let session_id = session.session_id;
                        tokio::spawn(async move {
                            let mut guard = client.lock().await;
                            if let Err(error) =
                                guard.prompt(&session_id, &text, prompt_resource).await
                            {
                                debug!(%error, "failed to send ACP prompt");
                            }
                        });
                    } else if self.recipient_is_acp(target).await {
                        self.queue_acp_prompt(target, text, false).await;
                        if !self.deliver_acp_prompts_if_ready(target).await {
                            self.retry_acp_connect_if_needed(target).await;
                        }
                    } else {
                        debug!("agent prompt submitted without an active ACP session");
                    }
                }
                Event::Ui(UiEvent::AcpHandshakeAction { agent_id, action }) => {
                    self.handle_acp_handshake_action(&agent_id, action).await;
                }
                Event::Ui(UiEvent::AcpJsonRpcResult {
                    agent_id,
                    id,
                    result,
                }) => {
                    let Some(session) = self.acp_session(&agent_id).await else {
                        continue;
                    };
                    let client = Arc::clone(&session.client);
                    tokio::spawn(async move {
                        let mut guard = client.lock().await;
                        if let Err(err) = guard.send_jsonrpc_result(id, result).await {
                            debug!(%err, "failed to send ACP JSON-RPC result to agent");
                        }
                    });
                }
                Event::Editor(event) => self.apply_editor_event(event).await,
                Event::Agent(AgentEvent::OutputChunk(chunk)) => {
                    self.handle_agent_output(chunk).await;
                }
                Event::Agent(AgentEvent::Acp { agent_id, event }) => match event {
                    AcpAgentEvent::TranscriptChunk { kind, text } => {
                        if kind == "agent_message_chunk" && !text.is_empty() {
                            self.send_acp_session_status(&agent_id, None, None, true);
                        }
                        self.send_ui_command(UiCommand::AcpTranscriptChunk {
                            agent_id,
                            kind,
                            text,
                        });
                    }
                    AcpAgentEvent::Progress { id, label, active } => {
                        if active && !label.is_empty() {
                            self.acp_tool_titles
                                .insert((agent_id.clone(), id.clone()), label.clone());
                        } else if !active {
                            self.acp_tool_titles.remove(&(agent_id.clone(), id.clone()));
                        }
                        self.send_ui_command(UiCommand::AcpProgress {
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
                            if let Some(t) = self.acp_tool_titles.get(&title_key) {
                                title = t.clone();
                            }
                        }
                        self.send_ui_command(UiCommand::AcpModalPrompt {
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
                        self.send_ui_command(UiCommand::AcpModalPrompt {
                            agent_id,
                            jsonrpc_id,
                            title,
                            message: prompt,
                            options,
                            kind: AcpModalKind::AskQuestion { question_id },
                        });
                    }
                    AcpAgentEvent::SessionStatus { provider_error } => {
                        self.send_acp_session_status(
                            &agent_id,
                            None,
                            provider_error.as_deref(),
                            false,
                        );
                    }
                },
                Event::Agent(event) => debug!(?event, "agent event received"),
                event => debug!(?event, "mediator event received"),
            }
        }
    }

    async fn apply_editor_event(&mut self, event: EditorEvent) {
        let previous_path = self
            .editor_state
            .active_buffer
            .as_ref()
            .map(|buffer| buffer.path.clone());

        debug!(?event, "editor context updated");
        match &event {
            EditorEvent::ActiveBufferChanged { path, line, column } => {
                // Editor context follows the primary ACP agent when one is connected.
                if let Some(session) = self.acp_session_for_prompt(None).await {
                    let path = path.clone();
                    let line = *line;
                    let column = *column;
                    let client = Arc::clone(&session.client);
                    tokio::spawn(async move {
                        let params = ContextUpdateParams {
                            active_buffer: Some(acp::BufferContext {
                                path: path.to_string_lossy().to_string(),
                                line,
                                column,
                            }),
                            workspace_roots: vec![],
                        };
                        let mut guard = client.lock().await;
                        if let Err(err) = guard.update_context(params).await {
                            debug!(%err, "failed to send ACP context update");
                        }
                    });
                } else if previous_path.as_ref() != Some(path) {
                    // Legacy PTY path
                    self.send_ui_command(UiCommand::EditorContextChanged {
                        path: path.clone(),
                        line: *line,
                        column: *column,
                    });
                }
            }
            EditorEvent::VisualSelectionChanged { .. } => {}
            EditorEvent::DiagnosticsChanged { .. } => {}
            EditorEvent::BufferSendRequested {
                path,
                start_line,
                end_line,
            } => {
                let display_path = path
                    .strip_prefix(&self.config.working_directory)
                    .unwrap_or(path);
                if let Some(session) = self.acp_session_for_prompt(None).await {
                    let client = Arc::clone(&session.client);
                    let session_id = session.session_id;
                    let path = path.clone();
                    let display_path = display_path.to_path_buf();
                    let line_range = start_line.zip(*end_line);
                    tokio::spawn(async move {
                        let mut guard = client.lock().await;
                        if let Err(err) = guard
                            .prompt_context(&session_id, &path, &display_path, line_range)
                            .await
                        {
                            debug!(%err, "failed to send ACP context prompt");
                        }
                    });
                } else {
                    let payload = if let (Some(start), Some(end)) = (start_line, end_line) {
                        format!("@{}:{start}-{end}\n", display_path.display())
                    } else {
                        format!("@{}\n", display_path.display())
                    };
                    self.send_ui_command(UiCommand::AgentInput {
                        payload,
                        focus_agent: true,
                        target_agent_id: None,
                    });
                }
            }
            EditorEvent::AgentSend {
                agent_id,
                from,
                content,
                focus: _focus,
            } => {
                let target = agent_id.as_deref().unwrap_or(ORCHESTRATOR_AGENT_ID);
                if self.recipient_is_acp(target).await {
                    info!(
                        agent_id = target,
                        from = ?from,
                        bytes = content.len(),
                        "ACP agent message received"
                    );
                    if from.is_none() {
                        self.queue_acp_prompt(target, content.clone(), true).await;
                    }
                    if !self.deliver_acp_prompts_if_ready(target).await {
                        info!(
                            agent_id = target,
                            "ACP message queued until session handshake completes"
                        );
                        self.retry_acp_connect_if_needed(target).await;
                    }
                } else if from.is_none() {
                    // Lua/editor path: persist to mailbox (CLI send enqueues before notifying).
                    let envelope = agent_bus::Message {
                        from: "unknown".to_string(),
                        to: target.to_string(),
                        ts: crate::util::now_millis(),
                        text: content.clone(),
                    };
                    if let Err(err) = agent_bus::enqueue(&self.config.agents_dir, &envelope) {
                        warn!(%err, agent_id = target, "failed to enqueue bus message");
                    }
                } else {
                    debug!(
                        agent_id = target,
                        "non-ACP recipient: message stored in mailbox only"
                    );
                }
            }
            EditorEvent::SpawnAgent { id, role, command } => {
                if !self.config.orchestration_enabled {
                    warn!(
                        %id,
                        "spawn agent ignored: orchestration unavailable (no ACP mapping for configured agent)"
                    );
                } else {
                    info!(%id, role = ?role, command = ?command, "spawn agent requested");
                    let (role, command) =
                        self.config
                            .apply_spawn_preset(id.as_str(), role.clone(), command.clone());
                    let launch = self.config.resolve_spawn_command(command.as_deref());
                    let resolved = launch.command;

                    if launch.acp {
                        self.expected_acp_agents.lock().await.insert(id.clone());
                        self.acp_launch_configs.lock().await.insert(
                            id.clone(),
                            AcpLaunchConfig {
                                role: role.clone(),
                                command: resolved.clone(),
                            },
                        );
                        self.connect_acp_agent(id.clone(), role.clone(), resolved.clone());
                    }

                    self.send_ui_command(UiCommand::SpawnAgent {
                        id: id.clone(),
                        role: role.clone(),
                        command: Some(resolved),
                    });
                }
            }
            EditorEvent::TerminateAgent { id } => {
                if id == PRIMARY_PTY_AGENT_ID {
                    tracing::warn!(%id, "refusing to terminate primary agent");
                } else {
                    info!(%id, "terminate agent requested");
                    self.acp_sessions.lock().await.remove(id);
                    self.expected_acp_agents.lock().await.remove(id);
                    self.pending_acp_prompts.lock().await.remove(id);
                    self.acp_launch_configs.lock().await.remove(id);
                    self.connecting_acp_agents.lock().await.remove(id);
                    self.acp_tool_titles
                        .retain(|(agent_id, _), _| agent_id != id);
                    self.send_ui_command(UiCommand::TerminateAgent { id: id.clone() });
                }
            }
        }

        self.editor_state.apply(event);
    }

    fn send_ui_command(&self, command: UiCommand) {
        if self.ui_commands.try_send(command).is_err() {
            warn!("ui command channel full or closed; dropped pane update");
        }
    }

    async fn handle_agent_output(&mut self, chunk: String) {
        self.agent_output_buffer.push_str(&chunk);

        while let Some(newline_index) = self.agent_output_buffer.find('\n') {
            let line = self.agent_output_buffer[..newline_index].trim().to_string();
            self.agent_output_buffer.drain(..=newline_index);

            if line.is_empty() {
                continue;
            }

            let payload = match line.strip_prefix("@tool ") {
                Some(payload) => payload,
                None => continue,
            };

            if let Some(response) = self.handle_tool_command(payload).await {
                self.send_ui_command(UiCommand::AgentInput {
                    payload: response,
                    focus_agent: false,
                    target_agent_id: None,
                });
            }
        }
    }

    async fn handle_tool_command(&self, payload: &str) -> Option<String> {
        let request = match serde_json::from_str::<AgentToolRequest>(payload) {
            Ok(request) => request,
            Err(error) => {
                return Some(serialize_tool_response(AgentToolResponse {
                    id: None,
                    ok: false,
                    result: None,
                    error: Some(format!("invalid tool request: {error}")),
                }));
            }
        };

        let id = request.id.clone();
        let response = match request.method.as_str() {
            "lsp_clients" => {
                let clients = if let Some(handle) = &self.lsp_handle {
                    handle.clients().await
                } else {
                    vec![]
                };
                AgentToolResponse {
                    id,
                    ok: true,
                    result: Some(
                        serde_json::to_value(clients).unwrap_or(serde_json::Value::Array(vec![])),
                    ),
                    error: None,
                }
            }
            "lsp_definition" => {
                let handle = match &self.lsp_handle {
                    Some(handle) => handle,
                    None => {
                        return Some(serialize_tool_response(AgentToolResponse {
                            id,
                            ok: false,
                            result: None,
                            error: Some("lsp bridge unavailable".to_string()),
                        }));
                    }
                };

                let args = match serde_json::from_value::<LspDefinitionArgs>(request.params) {
                    Ok(args) => args,
                    Err(error) => {
                        return Some(serialize_tool_response(AgentToolResponse {
                            id,
                            ok: false,
                            result: None,
                            error: Some(format!("invalid lsp_definition params: {error}")),
                        }));
                    }
                };

                match handle
                    .definition(&args.uri, args.line, args.character)
                    .await
                {
                    Ok(result) => AgentToolResponse {
                        id,
                        ok: true,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => AgentToolResponse {
                        id,
                        ok: false,
                        result: None,
                        error: Some(error.to_string()),
                    },
                }
            }
            _ => AgentToolResponse {
                id,
                ok: false,
                result: None,
                error: Some(format!("unsupported tool method: {}", request.method)),
            },
        };

        Some(serialize_tool_response(response))
    }
}

impl EventSender {
    pub fn try_send(&self, event: Event) {
        if self.events.try_send(event).is_err() {
            debug!("mediator is not accepting events");
        }
    }

    pub async fn send(&self, event: Event) {
        if self.events.send(event).await.is_err() {
            debug!("mediator is no longer accepting events");
        }
    }
}

impl MediatorHandle {
    pub fn events(&self) -> EventSender {
        self.events.clone()
    }

    pub fn take_ui_commands(&mut self) -> mpsc::Receiver<UiCommand> {
        self.ui_commands
            .take()
            .expect("UI commands receiver was already taken")
    }

    #[allow(dead_code)]
    pub fn lsp_handle(&self) -> Option<lsp_bridge::LspBridgeHandle> {
        self.lsp_handle.clone()
    }

    /// Convenience: returns the list of LSP clients currently attached in Neovim (if the bridge is active).
    /// Useful for agent/ACP code to discover language server capabilities without a full request.
    #[allow(dead_code)]
    pub async fn lsp_clients(&self) -> Vec<lsp_bridge::LspClientInfo> {
        if let Some(handle) = self.lsp_handle() {
            handle.clients().await
        } else {
            vec![]
        }
    }

    pub async fn shutdown(self) {
        self.events.send(Event::Shutdown).await;
        let _ = self.stopped.await;
        self.editor_listener.abort();
        self.lsp_listener.abort();
        self.lsp_clients_forwarder.abort();
        // lsp_handle dropped here
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_config() -> Config {
        Config {
            nvim_command: "nvim".to_string(),
            agent_command: Some("echo agent".to_string()),
            acp_agent: None,
            orchestration_enabled: false,
            agent_pane_cols: None,
            review_backend: crate::config::ReviewBackend::Nvim,
            scroll_sensitivity: crate::config::DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            agents_dir: PathBuf::from("/tmp/agents"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
            plugin_dir: None,
            agent_presets: crate::config::default_agent_presets(),
        }
    }

    fn parse_tool_result(payload: &str) -> serde_json::Value {
        let body = payload
            .strip_prefix("@tool_result ")
            .expect("tool result prefix");
        serde_json::from_str(body.trim()).expect("valid tool result json")
    }

    #[tokio::test]
    async fn lsp_clients_tool_returns_empty_array_without_bridge() {
        let mediator = Mediator::new(test_config());
        let response = mediator
            .handle_tool_command(r#"{"id":"1","method":"lsp_clients"}"#)
            .await
            .expect("response");

        let value = parse_tool_result(&response);
        assert_eq!(value["id"], "1");
        assert_eq!(value["ok"], true);
        assert_eq!(value["result"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn unsupported_tool_method_returns_error() {
        let mediator = Mediator::new(test_config());
        let response = mediator
            .handle_tool_command(r#"{"id":"2","method":"lsp_hover"}"#)
            .await
            .expect("response");

        let value = parse_tool_result(&response);
        assert_eq!(value["id"], "2");
        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error string")
                .contains("unsupported tool method")
        );
    }

    #[test]
    fn serialize_tool_response_roundtrips_success_and_error() {
        let ok = serialize_tool_response(AgentToolResponse {
            id: Some("t1".into()),
            ok: true,
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        });
        assert!(ok.starts_with("@tool_result "));
        let parsed: serde_json::Value =
            serde_json::from_str(ok.trim_start_matches("@tool_result ").trim()).unwrap();
        assert_eq!(parsed["id"], "t1");
        assert_eq!(parsed["ok"], true);

        let err = serialize_tool_response(AgentToolResponse {
            id: None,
            ok: false,
            result: None,
            error: Some("boom".into()),
        });
        let parsed_err: serde_json::Value =
            serde_json::from_str(err.trim_start_matches("@tool_result ").trim()).unwrap();
        assert_eq!(parsed_err["ok"], false);
        assert!(parsed_err["error"].as_str().unwrap().contains("boom"));
    }
}
