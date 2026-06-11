use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use serde::{Deserialize, Serialize};

use crate::acp::{self, AcpClient, ContextUpdateParams, PromptResource};
use crate::config::Config;
use crate::editor::{self, EditorState};
use crate::event::{AcpModalKind, AgentEvent, EditorEvent, Event, UiCommand, UiEvent};
use crate::lsp_bridge;
use crate::nvim::{self, FileTarget};

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

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    ui_commands: mpsc::Sender<UiCommand>,
    editor_state: EditorState,
    in_flight_symbol_open: Option<JoinHandle<()>>,
    lsp_handle: Option<lsp_bridge::LspBridgeHandle>,
    agent_output_buffer: String,
    /// ACP client when using the Agent Client Protocol instead of raw PTY.
    acp_client: Option<Arc<Mutex<AcpClient>>>,
    /// Current ACP session ID (if ACP is active).
    acp_session_id: Option<String>,
    /// Tool call id → latest title from `session/update` (for permission UI).
    acp_tool_titles: HashMap<String, String>,
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
            acp_client: None,
            acp_session_id: None,
            acp_tool_titles: HashMap::new(),
        }
    }

    /// Connect to an ACP-capable agent.
    ///
    /// Spawns the agent process, performs the initialize handshake,
    /// and creates a new session. After this call, editor context updates
    /// will be sent via ACP `context/update` instead of raw PTY injection.
    pub async fn connect_acp(&mut self, command: &str, args: &[&str]) -> Result<String> {
        let mut client = AcpClient::spawn(command, args).await?;
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
            agent = agent_name,
            version = agent_version,
            protocol_version = init.protocol_version,
            "ACP agent initialized"
        );

        let session = client.new_session(&self.config.working_directory).await?;
        tracing::info!(session_id = %session.session_id, "ACP session created");

        self.acp_client = Some(Arc::new(Mutex::new(client)));
        self.acp_session_id = Some(session.session_id.clone());
        Ok(session.session_id)
    }

    pub fn spawn(mut self) -> MediatorHandle {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (ui_commands_tx, ui_commands_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        self.events = events_rx;
        self.ui_commands = ui_commands_tx;
        let events = EventSender {
            events: events_tx.clone(),
        };

        if let Some(client) = &self.acp_client {
            let client = Arc::clone(client);
            let events = events.clone();
            tokio::spawn(async move {
                let mut guard = client.lock().await;
                guard.spawn_reader(events);
            });
        }

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
                Event::Ui(UiEvent::AgentPromptSubmitted { text }) => {
                    if text.trim().is_empty() {
                        continue;
                    }

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

                    let (Some(client), Some(session_id)) =
                        (&self.acp_client, self.acp_session_id.clone())
                    else {
                        debug!("agent prompt submitted without an active ACP session");
                        continue;
                    };
                    let client = Arc::clone(client);
                    tokio::spawn(async move {
                        let mut guard = client.lock().await;
                        if let Err(error) = guard.prompt(&session_id, &text, prompt_resource).await
                        {
                            debug!(%error, "failed to send ACP prompt");
                        }
                    });
                }
                Event::Ui(UiEvent::AcpJsonRpcResult { id, result }) => {
                    let Some(client) = self.acp_client.clone() else {
                        continue;
                    };
                    tokio::spawn(async move {
                        let mut guard = client.lock().await;
                        if let Err(err) = guard.send_jsonrpc_result(id, result).await {
                            debug!(%err, "failed to send ACP JSON-RPC result to agent");
                        }
                    });
                }
                Event::Editor(event) => self.apply_editor_event(event),
                Event::Agent(AgentEvent::OutputChunk(chunk)) => {
                    self.handle_agent_output(chunk).await;
                }
                Event::Agent(AgentEvent::AcpTranscriptChunk { kind, text }) => {
                    self.send_ui_command(UiCommand::AcpTranscriptChunk { kind, text });
                }
                Event::Agent(AgentEvent::AcpProgress { id, label, active }) => {
                    if active && !label.is_empty() {
                        self.acp_tool_titles.insert(id.clone(), label.clone());
                    }
                    if !active {
                        self.acp_tool_titles.remove(&id);
                    }
                    self.send_ui_command(UiCommand::AcpProgress { id, label, active });
                }
                Event::Agent(AgentEvent::AcpPermissionRequest {
                    jsonrpc_id,
                    session_id,
                    tool_call_id,
                    mut title,
                    options,
                }) => {
                    if title == "Permission required" || title.trim().is_empty() {
                        if let Some(t) = self.acp_tool_titles.get(&tool_call_id) {
                            title = t.clone();
                        }
                    }
                    self.send_ui_command(UiCommand::AcpModalPrompt {
                        jsonrpc_id,
                        title,
                        message: format!("Session `{session_id}`\nTool call `{tool_call_id}`"),
                        options,
                        kind: AcpModalKind::SessionPermission,
                    });
                }
                Event::Agent(AgentEvent::AcpCursorAskQuestion {
                    jsonrpc_id,
                    title,
                    question_id,
                    prompt,
                    options,
                }) => {
                    self.send_ui_command(UiCommand::AcpModalPrompt {
                        jsonrpc_id,
                        title,
                        message: prompt,
                        options,
                        kind: AcpModalKind::CursorAskQuestion { question_id },
                    });
                }
                Event::Agent(event) => debug!(?event, "agent event received"),
                event => debug!(?event, "mediator event received"),
            }
        }
    }

    fn apply_editor_event(&mut self, event: EditorEvent) {
        let previous_path = self
            .editor_state
            .active_buffer
            .as_ref()
            .map(|buffer| buffer.path.clone());

        debug!(?event, "editor context updated");
        match &event {
            EditorEvent::ActiveBufferChanged { path, line, column } => {
                if let (Some(client), Some(_session_id)) = (&self.acp_client, &self.acp_session_id)
                {
                    // ACP path: send structured context update
                    let path = path.clone();
                    let line = *line;
                    let column = *column;
                    let client = Arc::clone(client);
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
                if let (Some(client), Some(session_id)) = (&self.acp_client, &self.acp_session_id) {
                    let client = Arc::clone(client);
                    let path = path.clone();
                    let display_path = display_path.to_path_buf();
                    let session_id = session_id.clone();
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
                    });
                }
            }
        }

        self.editor_state.apply(event);
    }

    fn send_ui_command(&self, command: UiCommand) {
        if self.ui_commands.try_send(command).is_err() {
            debug!("ui is not accepting commands");
        }
    }

    async fn handle_agent_output(&mut self, chunk: String) {
        self.agent_output_buffer.push_str(&chunk);

        loop {
            let Some(newline_index) = self.agent_output_buffer.find('\n') else {
                break;
            };

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
            agent_pane_cols: None,
            review_backend: crate::config::ReviewBackend::Nvim,
            scroll_sensitivity: crate::config::DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/tmp/context_bridge.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/lsp.sock"),
            working_directory: PathBuf::from("/tmp"),
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
}
