use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::editor::{self, EditorState};
use crate::event::{AgentEvent, EditorEvent, Event, UiCommand, UiEvent};
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

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    ui_commands: mpsc::Sender<UiCommand>,
    editor_state: EditorState,
    in_flight_symbol_open: Option<JoinHandle<()>>,
    lsp_handle: Option<lsp_bridge::LspBridgeHandle>,
    agent_output_buffer: String,
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
                debug!(count = clients.len(), "lsp clients updated (not forwarding to agent stdin)");
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
                Event::Editor(event) => self.apply_editor_event(event),
                Event::Agent(AgentEvent::OutputChunk(chunk)) => {
                    // Log agent output so we can observe communication during development
                    if !chunk.trim().is_empty() {
                        info!(target: "via::agent", "{}", chunk.trim_end());
                    }
                    self.handle_agent_output(chunk).await;
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
        let previous_selection = self.editor_state.visual_selection.clone();

        debug!(?event, "editor context updated");
        match &event {
            EditorEvent::ActiveBufferChanged { path, line, column } => {
                if previous_path.as_ref() != Some(path) {
                    self.send_ui_command(UiCommand::EditorContextChanged {
                        path: path.clone(),
                        line: *line,
                        column: *column,
                    });
                }
            }
            EditorEvent::VisualSelectionChanged {
                path,
                start_line,
                end_line,
            } => {
                let changed = previous_selection
                    .as_ref()
                    .map(|selection| {
                        selection.path != *path
                            || selection.start_line != *start_line
                            || selection.end_line != *end_line
                    })
                    .unwrap_or(true);

                if changed {
                    self.send_ui_command(UiCommand::VisualSelectionChanged {
                        path: path.clone(),
                        start_line: *start_line,
                        end_line: *end_line,
                    });
                }
            }
            EditorEvent::DiagnosticsChanged { .. } => {}
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
                self.send_ui_command(UiCommand::AgentInput { payload: response });
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
                    result: Some(serde_json::to_value(clients).unwrap_or(serde_json::Value::Array(vec![]))),
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

                match handle.definition(&args.uri, args.line, args.character).await {
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
        assert!(value["error"]
            .as_str()
            .expect("error string")
            .contains("unsupported tool method"));
    }
}
