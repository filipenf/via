use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::acp::PromptResource;
use crate::acp_runtime::{AcpConnectCtx, AcpRuntime};
use crate::agent_bus;
use crate::agent_delivery::AgentDelivery;
use crate::config::Config;
use crate::config::ReviewBackend;
use crate::editor::{self, EditorState};
use crate::event::{AgentEvent, EditorEvent, Event, UiCommand, UiEvent};
use crate::lsp_bridge;
use crate::nvim;

use crate::config::{ORCHESTRATOR_AGENT_ID, PRIMARY_PTY_AGENT_ID};

const EVENT_BUFFER_SIZE: usize = 128;

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

/// The via mediator: a thin router between editor, UI, ACP runtime, and store.
///
/// ACP session lifecycle + delivery live in [`crate::acp_runtime`] and
/// [`crate::agent_delivery`]; this struct owns those, the editor state, the LSP
/// bridge handle, and the `@tool` JSON protocol over PTY stdout. Workflow
/// policy stays in skills/config — no new match-arm logic here.
pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    ui_commands: mpsc::Sender<UiCommand>,
    editor_state: EditorState,
    in_flight_symbol_open: Option<JoinHandle<()>>,
    lsp_handle: Option<lsp_bridge::LspBridgeHandle>,
    agent_output_buffer: String,
    acp_runtime: AcpRuntime,
    agent_delivery: AgentDelivery,
    /// Event sender retained after `spawn()` so dynamically connected agents can start readers.
    event_sender: Option<EventSender>,
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
            acp_runtime: AcpRuntime::new(),
            agent_delivery: AgentDelivery::new(),
            event_sender: None,
        }
    }

    /// Build the context passed to ACP connect/retry tasks: pending queue +
    /// agents dir + cwd + reader/event channels.
    fn connect_ctx(&self) -> AcpConnectCtx {
        AcpConnectCtx {
            pending: self.agent_delivery.pending_arc(),
            agents_dir: self.config.agents_dir.clone(),
            cwd: self.config.working_directory.clone(),
            event_sender: self.event_sender.clone(),
            ui_commands: self.ui_commands.clone(),
        }
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

        self.acp_runtime
            .spawn_initial_readers(events.clone(), self.ui_commands.clone());

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
                    let (target, candidates) = self
                        .editor_state
                        .file_index
                        .resolve_open_from_index(path, line);
                    if let Err(error) = nvim::open_file(
                        &self.config.nvim_socket_path,
                        &self.config.working_directory,
                        target,
                        &candidates,
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
                    self.handle_prompt_submitted(text, agent_id).await;
                }
                Event::Ui(UiEvent::AcpHandshakeAction { agent_id, action }) => {
                    let ctx = self.connect_ctx();
                    self.acp_runtime
                        .handle_handshake_action(&agent_id, action, &ctx)
                        .await;
                }
                Event::Ui(UiEvent::AcpJsonRpcResult {
                    agent_id,
                    id,
                    result,
                }) => {
                    if let Some(session) = self.acp_runtime.session(&agent_id).await {
                        let client = Arc::clone(&session.client);
                        tokio::spawn(async move {
                            let mut guard = client.lock().await;
                            if let Err(err) = guard.send_jsonrpc_result(id, result).await {
                                debug!(%err, "failed to send ACP JSON-RPC result to agent");
                            }
                        });
                    }
                }
                Event::Editor(event) => self.apply_editor_event(event).await,
                Event::Agent(AgentEvent::OutputChunk(chunk)) => {
                    self.handle_agent_output(chunk).await;
                }
                Event::Agent(AgentEvent::Acp { agent_id, event }) => {
                    self.acp_runtime
                        .handle_agent_event(&self.ui_commands, agent_id, event)
                        .await;
                }
                Event::Agent(event) => debug!(?event, "agent event received"),
                event => debug!(?event, "mediator event received"),
            }
        }
    }

    /// `AgentPromptSubmitted`: send a prompt to an ACP session, or queue it for
    /// delivery once the handshake completes (retrying the connect if needed).
    async fn handle_prompt_submitted(&mut self, text: String, agent_id: Option<String>) {
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

        if let Some(session) = self.acp_runtime.session(target).await {
            let client = Arc::clone(&session.client);
            let session_id = session.session_id;
            tokio::spawn(async move {
                let mut guard = client.lock().await;
                if let Err(error) = guard.prompt(&session_id, &text, prompt_resource).await {
                    debug!(%error, "failed to send ACP prompt");
                }
            });
        } else if self
            .acp_runtime
            .recipient_is_acp(&self.config.agents_dir, target)
            .await
        {
            self.agent_delivery.queue_prompt(target, text, false).await;
            if !self
                .acp_runtime
                .deliver_if_ready(
                    &self.agent_delivery.pending_arc(),
                    target,
                    &self.config.agents_dir,
                    &self.ui_commands,
                )
                .await
            {
                let ctx = self.connect_ctx();
                self.acp_runtime.retry_connect_if_needed(target, &ctx).await;
            }
        } else {
            debug!("agent prompt submitted without an active ACP session");
        }
    }

    async fn apply_editor_event(&mut self, event: EditorEvent) {
        debug!(?event, "editor context updated");
        match &event {
            EditorEvent::VisualSelectionChanged { .. } => {}
            EditorEvent::DiagnosticsChanged { .. } => {}
            EditorEvent::BufferSendRequested {
                path,
                start_line,
                end_line,
            } => {
                // Explicit buffer/selection send always targets the primary PTY
                // agent pane — never a spawned ACP helper (reviewer/coder/…).
                let display_path = path
                    .strip_prefix(&self.config.working_directory)
                    .unwrap_or(path);
                let payload = if let (Some(start), Some(end)) = (start_line, end_line) {
                    format!("@{}:{start}-{end}\n", display_path.display())
                } else {
                    format!("@{}\n", display_path.display())
                };
                self.send_ui_command(UiCommand::AgentInput {
                    payload,
                    focus_agent: true,
                    target_agent_id: Some(PRIMARY_PTY_AGENT_ID.to_string()),
                });
            }
            EditorEvent::AgentSend {
                agent_id,
                from,
                content,
                focus: _focus,
            } => {
                let target = agent_id.as_deref().unwrap_or(ORCHESTRATOR_AGENT_ID);
                if self
                    .acp_runtime
                    .recipient_is_acp(&self.config.agents_dir, target)
                    .await
                {
                    info!(
                        agent_id = target,
                        from = ?from,
                        bytes = content.len(),
                        "ACP agent message received"
                    );
                    if from.is_none() {
                        self.agent_delivery
                            .queue_prompt(target, content.clone(), true)
                            .await;
                    }
                    if !self
                        .acp_runtime
                        .deliver_if_ready(
                            &self.agent_delivery.pending_arc(),
                            target,
                            &self.config.agents_dir,
                            &self.ui_commands,
                        )
                        .await
                    {
                        info!(
                            agent_id = target,
                            "ACP message queued until session handshake completes"
                        );
                        let ctx = self.connect_ctx();
                        self.acp_runtime.retry_connect_if_needed(target, &ctx).await;
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
                } else if crate::config::is_reserved_agent_id(id) {
                    warn!(
                        %id,
                        "spawn agent ignored: reserved id (primary PTY pane or human role), not a spawnable pane"
                    );
                } else {
                    info!(%id, role = ?role, command = ?command, "spawn agent requested");
                    let (role, command) =
                        self.config
                            .apply_spawn_preset(id.as_str(), role.clone(), command.clone());
                    let launch = self.config.resolve_spawn_command(command.as_deref());
                    let resolved = launch.command;

                    if launch.acp {
                        let ctx = self.connect_ctx();
                        self.acp_runtime
                            .register_spawn(id.clone(), role.clone(), resolved.clone(), ctx)
                            .await;
                    }

                    self.send_ui_command(UiCommand::SpawnAgent {
                        id: id.clone(),
                        role: role.clone(),
                        command: Some(resolved),
                    });
                }
            }
            EditorEvent::TerminateAgent { id } => {
                if AcpRuntime::is_primary_pty(id) {
                    tracing::warn!(%id, "refusing to terminate primary agent");
                } else {
                    info!(%id, "terminate agent requested");
                    // Order matters: terminate first (holds the sessions lock while
                    // clearing `expected`) so a connect task finishing its handshake
                    // concurrently observes the removal and discards its session.
                    // Only then clear pending — otherwise a connect task can drain
                    // the mailbox + pending and spawn delivery for a dead agent.
                    self.acp_runtime.terminate(&self.ui_commands, id).await;
                    self.agent_delivery.discard_queued(id).await;
                }
            }
            EditorEvent::ReviewGateOpened { task_id, title } => {
                info!(%task_id, %title, "review gate opened");
                match self.config.review_backend {
                    ReviewBackend::Nvim => {
                        if let Err(error) = nvim::open_review(
                            &self.config.nvim_socket_path,
                            &self.config.working_directory,
                        )
                        .await
                        {
                            error!(%error, %task_id, "failed to open review in Neovim");
                        }
                    }
                    ReviewBackend::Hunk => {
                        // Hunk review is a UI-only pane toggle; from the
                        // mediator we can't flip it directly. Surface the
                        // request to the UI so the human sees the gate.
                        self.send_ui_command(UiCommand::ReviewGateOpened {
                            task_id: task_id.clone(),
                            title: title.clone(),
                        });
                    }
                }
            }
            EditorEvent::TaskCreated { id } => {
                debug!(%id, "task created signal received");
            }
            EditorEvent::TaskUpdated { id, fields } => {
                debug!(%id, ?fields, "task updated signal received");
            }
            EditorEvent::TaskDeleted { id } => {
                debug!(%id, "task deleted signal received");
            }
            EditorEvent::FileIndexChanged {
                buffers,
                vcs_working_tree,
                vcs_branch,
            } => {
                debug!(
                    buffers = buffers.len(),
                    vcs_wt = vcs_working_tree.len(),
                    vcs_branch = vcs_branch.len(),
                    "file index snapshot received"
                );
                self.send_ui_command(UiCommand::FileIndexChanged {
                    buffers: buffers.clone(),
                    vcs_working_tree: vcs_working_tree.clone(),
                    vcs_branch: vcs_branch.clone(),
                });
            }
            EditorEvent::SymbolIndexChanged { symbols } => {
                debug!(symbols = symbols.len(), "symbol index snapshot received");
                self.send_ui_command(UiCommand::SymbolIndexChanged {
                    symbols: symbols.clone(),
                });
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
    use crate::test_support::temp_dir;
    use std::path::PathBuf;
    use std::time::Duration;

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

    fn test_config_with_agents_dir(agents_dir: PathBuf) -> Config {
        Config {
            agents_dir,
            ..test_config()
        }
    }

    fn mediator_with_ui_receiver(config: Config) -> (Mediator, mpsc::Receiver<UiCommand>) {
        let mut mediator = Mediator::new(config);
        let (ui_tx, ui_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        mediator.ui_commands = ui_tx;
        (mediator, ui_rx)
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

    #[tokio::test]
    async fn spawn_agent_without_command_uses_preset_role_and_resolved_acp_command() {
        let agents_dir = temp_dir("spawn-default-acp");
        let config = Config {
            agent_command: Some("unknown-agent".to_string()),
            acp_agent: Some("false acp".to_string()),
            orchestration_enabled: true,
            agents_dir: agents_dir.clone(),
            ..test_config()
        };
        let (mut mediator, mut ui_rx) = mediator_with_ui_receiver(config);

        mediator
            .apply_editor_event(EditorEvent::SpawnAgent {
                id: "reviewer".to_string(),
                role: None,
                command: None,
            })
            .await;

        let command = tokio::time::timeout(Duration::from_millis(100), ui_rx.recv())
            .await
            .expect("spawn command")
            .expect("ui command");
        assert_eq!(
            command,
            UiCommand::SpawnAgent {
                id: "reviewer".to_string(),
                role: Some("reviewer".to_string()),
                command: Some("false acp".to_string()),
            }
        );

        assert_eq!(
            mediator
                .acp_runtime
                .launch_configs_arc()
                .lock()
                .await
                .get("reviewer")
                .map(|launch| (launch.role.clone(), launch.command.clone())),
            Some((Some("reviewer".to_string()), "false acp".to_string()))
        );
        std::fs::remove_dir_all(&agents_dir).ok();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_human_reserved_id() {
        let agents_dir = temp_dir("spawn-human-rejected");
        let config = Config {
            agent_command: Some("opencode".to_string()),
            orchestration_enabled: true,
            agents_dir: agents_dir.clone(),
            ..test_config()
        };
        let (mut mediator, mut ui_rx) = mediator_with_ui_receiver(config);

        mediator
            .apply_editor_event(EditorEvent::SpawnAgent {
                id: "human".to_string(),
                role: None,
                command: None,
            })
            .await;

        // No UI command should be emitted — the mediator rejects the reserved id.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), ui_rx.recv())
                .await
                .is_err(),
            "spawn of 'human' should not emit UiCommand::SpawnAgent"
        );
        // No ACP launch config should be recorded.
        assert!(
            mediator
                .acp_runtime
                .launch_configs_arc()
                .lock()
                .await
                .get("human")
                .is_none(),
        );
        std::fs::remove_dir_all(&agents_dir).ok();
    }

    #[tokio::test]
    async fn agent_send_to_pty_recipient_is_mailbox_only() {
        let agents_dir = temp_dir("pty-mailbox-only");
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
        let (mut mediator, mut ui_rx) =
            mediator_with_ui_receiver(test_config_with_agents_dir(agents_dir.clone()));

        mediator
            .apply_editor_event(EditorEvent::AgentSend {
                agent_id: Some("agent".to_string()),
                from: None,
                content: "check your inbox".to_string(),
                focus: true,
            })
            .await;

        let messages = agent_bus::drain_inbox(&agents_dir, "agent", true).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "check your inbox");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), ui_rx.recv())
                .await
                .is_err(),
            "PTY bus send should not emit AgentInput ping"
        );
        std::fs::remove_dir_all(&agents_dir).ok();
    }

    #[tokio::test]
    async fn buffer_send_targets_primary_agent_not_spawned_helpers() {
        let (mut mediator, mut ui_rx) = mediator_with_ui_receiver(test_config());

        mediator
            .apply_editor_event(EditorEvent::BufferSendRequested {
                path: PathBuf::from("/tmp/src/main.rs"),
                start_line: Some(3),
                end_line: Some(8),
            })
            .await;

        let command = tokio::time::timeout(Duration::from_millis(100), ui_rx.recv())
            .await
            .expect("buffer send command")
            .expect("ui command");
        assert_eq!(
            command,
            UiCommand::AgentInput {
                payload: "@src/main.rs:3-8\n".to_string(),
                focus_agent: true,
                target_agent_id: Some(PRIMARY_PTY_AGENT_ID.to_string()),
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), ui_rx.recv())
                .await
                .is_err(),
            "buffer send should emit exactly one AgentInput"
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
