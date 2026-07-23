use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::debug;

use crate::event::{AcpAgentEvent, AcpPermissionOption, AgentEvent, Event};
use crate::mediator::EventSender;

/// Minimal ACP (Agent Client Protocol) client.
///
/// This implements a lightweight JSON-RPC 2.0 client that speaks the
/// Agent Client Protocol over stdio with a subprocess agent.
pub struct AcpClient {
    child: Child,
    next_id: u64,
    /// Bus id of the agent this client drives (e.g. "orchestrator", "reviewer").
    /// Used to stamp emitted events so the UI can route them to the right pane.
    agent_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: Option<ClientInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub agent_capabilities: serde_json::Value,
    pub agent_info: Option<AgentInfo>,
    #[serde(default)]
    pub auth_methods: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigOption {
    id: String,
    #[serde(default)]
    current_value: Option<String>,
    #[serde(default)]
    options: Vec<ConfigOptionChoice>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigOptionChoice {
    value: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default)]
    config_options: Vec<ConfigOption>,
}

fn selected_config_value(options: &[ConfigOption], id: &str) -> Option<String> {
    options
        .iter()
        .find(|option| option.id == id)
        .and_then(|option| option.current_value.clone())
}

impl NewSessionResult {
    /// Selected model from `session/new` config options (e.g. `lemonade/Gemma-…`).
    pub fn selected_model(&self) -> Option<String> {
        selected_config_value(&self.config_options, "model")
    }

    /// Map a user-facing model slug (e.g. `composer-2.5` from `agent models`) to the
    /// ACP `session/set_config_option` value when the agent exposes a select list.
    pub fn resolve_model_value(&self, desired: &str) -> Option<String> {
        resolve_model_config_value(&self.config_options, desired)
    }
}

fn model_option_base(value: &str) -> &str {
    value.split('[').next().unwrap_or(value)
}

/// Resolve a preset slug against `session/new` model options.
fn resolve_model_config_value(options: &[ConfigOption], desired: &str) -> Option<String> {
    let model_option = options.iter().find(|option| option.id == "model")?;
    let choices = &model_option.options;
    if choices.is_empty() {
        return Some(desired.to_string());
    }

    if choices.iter().any(|choice| choice.value == desired) {
        return Some(desired.to_string());
    }

    if let Some(choice) = choices
        .iter()
        .find(|choice| choice.name.as_deref() == Some(desired))
    {
        return Some(choice.value.clone());
    }

    if let Some(choice) = choices
        .iter()
        .find(|choice| model_option_base(&choice.value) == desired)
    {
        return Some(choice.value.clone());
    }

    let desired_lower = desired.to_ascii_lowercase();
    if let Some(choice) = choices.iter().find(|choice| {
        choice
            .name
            .as_ref()
            .is_some_and(|name| name.to_ascii_lowercase() == desired_lower)
    }) {
        return Some(choice.value.clone());
    }

    if let Some(base) = desired.strip_suffix("-fast") {
        if let Some(choice) = choices.iter().find(|choice| {
            model_option_base(&choice.value) == base && choice.value.contains("fast=true")
        }) {
            return Some(choice.value.clone());
        }
        if let Some(choice) = choices
            .iter()
            .find(|choice| model_option_base(&choice.value) == base)
        {
            return Some(choice.value.clone());
        }
    }

    None
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResult {
    #[serde(default)]
    config_options: Vec<ConfigOption>,
}

impl SetConfigOptionResult {
    /// Selected model from the updated config options.
    pub fn selected_model(&self) -> Option<String> {
        selected_config_value(&self.config_options, "model")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Resource { resource: PromptResource },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResource {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum JsonRpcMessage {
    Request {
        jsonrpc: String,
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        jsonrpc: String,
        method: String,
        params: serde_json::Value,
    },
    Response {
        jsonrpc: String,
        id: serde_json::Value,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<JsonRpcError>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

impl AcpClient {
    /// Spawn an ACP-capable agent as a subprocess.
    ///
    /// The command should be something like "opencode acp", "cursor-agent acp", etc.
    pub async fn spawn(
        agent_id: &str,
        command: &str,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Self> {
        debug!(agent_id, command, ?args, "spawning ACP agent subprocess");
        let mut child_cmd = Command::new(command);
        child_cmd
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            child_cmd.env(key, value);
        }
        let child = child_cmd
            .spawn()
            .with_context(|| format!("failed to spawn ACP agent: {command}"))?;

        debug!(command, "ACP agent process started");
        Ok(Self {
            child,
            next_id: 1,
            agent_id: agent_id.to_string(),
        })
    }

    /// Perform the ACP initialize handshake.
    pub async fn initialize(&mut self) -> Result<InitializeResult> {
        debug!("sending initialize request to ACP agent");
        let params = InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: Some(ClientInfo {
                name: "via".to_string(),
                title: "via".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
        };

        let response = self
            .send_request("initialize", serde_json::to_value(params)?)
            .await?;

        debug!("received initialize response from ACP agent");
        serde_json::from_value(response).context("failed to parse initialize result")
    }

    /// Create a new agent session.
    pub async fn new_session(&mut self, working_directory: &Path) -> Result<NewSessionResult> {
        let params = NewSessionParams {
            cwd: working_directory.to_string_lossy().to_string(),
            mcp_servers: Vec::new(),
        };

        let response = self
            .send_request("session/new", serde_json::to_value(params)?)
            .await?;

        serde_json::from_value(response).context("failed to parse new_session result")
    }

    /// Set a session configuration option (e.g. `config_id = "model"`).
    pub async fn set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<SetConfigOptionResult> {
        let params = SetConfigOptionParams {
            session_id: session_id.to_string(),
            config_id: config_id.to_string(),
            value: value.to_string(),
        };

        let response = self
            .send_request("session/set_config_option", serde_json::to_value(params)?)
            .await?;

        serde_json::from_value(response).context("failed to parse set_config_option result")
    }

    /// Spawn a background task that continuously reads JSON-RPC lines
    /// from the agent's stdout, logs them, and forwards recognized updates to the bus.
    pub fn spawn_reader(&mut self, events: EventSender) {
        let agent_id = self.agent_id.clone();
        if let Some(stdout) = self.child.stdout.take() {
            let events = events.clone();
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        for event in process_acp_line(&line, &agent_id) {
                            events.send(Event::Agent(event)).await;
                        }
                    }
                }
                tracing::debug!(agent_id, "ACP agent stdout closed");
            });
        }
        if let Some(stderr) = self.child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    tracing::warn!(agent_id, stderr = %line, "ACP agent stderr");
                    if looks_like_provider_error(line) {
                        events
                            .send(Event::Agent(AgentEvent::acp(
                                &agent_id,
                                AcpAgentEvent::SessionStatus {
                                    provider_error: Some(truncate_provider_error(line)),
                                },
                            )))
                            .await;
                    }
                }
                tracing::debug!(agent_id, "ACP agent stderr closed");
            });
        }
    }

    /// Send a prompt to an existing session.
    pub async fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        resource: Option<PromptResource>,
    ) -> Result<()> {
        let mut prompt = vec![ContentBlock::Text {
            text: text.to_string(),
        }];
        if let Some(resource) = resource {
            prompt.push(ContentBlock::Resource { resource });
        }

        let params = PromptParams {
            session_id: session_id.to_string(),
            prompt,
        };

        self.send_request_detached("session/prompt", serde_json::to_value(params)?)
            .await
    }

    async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write_line_json(&line).await?;
        self.read_response(id).await
    }

    async fn send_request_detached(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;

        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write_line_json(&line).await
    }

    async fn write_line_json(&mut self, value: &serde_json::Value) -> Result<()> {
        let mut json = serde_json::to_string(value)?;
        json.push('\n');
        debug!(json = %json.trim_end(), "writing ACP message to agent stdin");
        let stdin = self
            .child
            .stdin
            .as_mut()
            .context("agent stdin unavailable")?;
        stdin.write_all(json.as_bytes()).await?;
        stdin.flush().await?;
        debug!("ACP message flushed to agent");
        Ok(())
    }

    /// Reply to a JSON-RPC **request** from the agent (e.g. `session/request_permission`, ask-question methods).
    pub async fn send_jsonrpc_result(
        &mut self,
        id: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<()> {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_line_json(&line).await
    }

    async fn read_response(&mut self, expected_id: u64) -> Result<serde_json::Value> {
        debug!(expected_id, "waiting for ACP response");
        let stdout = self
            .child
            .stdout
            .as_mut()
            .context("agent stdout unavailable")?;
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let msg: JsonRpcMessage = serde_json::from_str(&line)
                .with_context(|| format!("invalid JSON-RPC from agent: {line}"))?;

            match msg {
                JsonRpcMessage::Response {
                    jsonrpc: _,
                    id,
                    result,
                    error,
                } if jsonrpc_id_matches(&id, expected_id) => {
                    if let Some(err) = error {
                        anyhow::bail!(
                            "ACP error {}: {}{}",
                            err.code,
                            err.message,
                            err.data
                                .map(|data| format!(" data={data}"))
                                .unwrap_or_default()
                        );
                    }
                    return Ok(result.unwrap_or(serde_json::Value::Null));
                }
                // Ignore responses for other IDs (future improvement: queue them)
                JsonRpcMessage::Notification {
                    jsonrpc: _,
                    method,
                    params,
                } => {
                    // Handle streaming notifications here in a real implementation
                    tracing::debug!(method, ?params, "received ACP notification");
                }
                _ => {}
            }
        }

        anyhow::bail!("agent closed connection before response")
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Best effort: kill the agent process when we go away
        let _ = self.child.start_kill();
    }
}

fn jsonrpc_id_matches(received: &serde_json::Value, expected: u64) -> bool {
    received.as_u64() == Some(expected)
        || received.as_i64() == Some(expected as i64)
        || received.as_str().and_then(|s| s.parse::<u64>().ok()) == Some(expected)
}

fn parse_permission_options(options: &serde_json::Value) -> Option<Vec<AcpPermissionOption>> {
    let options_arr = options.as_array()?;
    let mut parsed = Vec::new();
    for item in options_arr {
        let option_id = item.get("optionId").and_then(serde_json::Value::as_str)?;
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(option_id)
            .to_string();
        parsed.push(AcpPermissionOption {
            option_id: option_id.to_string(),
            name,
        });
    }
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn tool_call_update_fields(
    tool_call: &serde_json::Value,
    fallback_title: Option<&str>,
) -> (String, String, Option<String>) {
    let tool_call_id = tool_call
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let title = tool_call
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.is_empty())
        .or(fallback_title)
        .unwrap_or("Permission required")
        .to_string();
    let kind = tool_call
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    (tool_call_id, title, kind)
}

fn parse_permission_request(
    jsonrpc_id: serde_json::Value,
    params: serde_json::Value,
) -> Option<AcpAgentEvent> {
    let session_id = params
        .get("sessionId")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let options = parse_permission_options(params.get("options")?)?;
    let top_title = params
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.is_empty());

    let (tool_call_id, title, tool_kind, command, command_cwd) =
        if let Some(subject) = params.get("subject") {
            match subject.get("type").and_then(serde_json::Value::as_str) {
                Some("command") => {
                    let command = subject
                        .get("command")
                        .and_then(serde_json::Value::as_str)?
                        .to_string();
                    let command_cwd = subject
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let tool_call_id = subject
                        .get("toolCallId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let title = top_title.unwrap_or("Permission required").to_string();
                    (tool_call_id, title, None, Some(command), command_cwd)
                }
                Some("tool_call") => {
                    let tool_call = subject.get("toolCall")?;
                    let (tool_call_id, title, tool_kind) =
                        tool_call_update_fields(tool_call, top_title);
                    (tool_call_id, title, tool_kind, None, None)
                }
                Some(other) => {
                    tracing::debug!(subject_type = other, "unknown ACP permission subject type");
                    let tool_call_id = subject
                        .get("toolCallId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let title = top_title.unwrap_or("Permission required").to_string();
                    (tool_call_id, title, None, None, None)
                }
                None if subject.get("toolCall").is_some() => {
                    let tool_call = subject.get("toolCall")?;
                    let (tool_call_id, title, tool_kind) =
                        tool_call_update_fields(tool_call, top_title);
                    (tool_call_id, title, tool_kind, None, None)
                }
                None => return None,
            }
        } else {
            let tool_call = params.get("toolCall")?;
            let (tool_call_id, title, tool_kind) = tool_call_update_fields(tool_call, top_title);
            (tool_call_id, title, tool_kind, None, None)
        };

    Some(AcpAgentEvent::PermissionRequest {
        jsonrpc_id,
        session_id,
        tool_call_id,
        title,
        options,
        command,
        command_cwd,
        tool_kind,
    })
}

fn parse_ask_question(
    jsonrpc_id: serde_json::Value,
    params: serde_json::Value,
) -> Option<AcpAgentEvent> {
    let title = params
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Question")
        .to_string();
    let questions = params.get("questions")?.as_array()?;
    let q = questions.first()?;
    let question_id = q.get("id").and_then(serde_json::Value::as_str)?.to_string();
    let prompt = q
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let opts_arr = q.get("options")?.as_array()?;
    let mut options = Vec::new();
    for item in opts_arr {
        let option_id = item.get("id").and_then(serde_json::Value::as_str)?;
        let name = item
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(option_id)
            .to_string();
        options.push(AcpPermissionOption {
            option_id: option_id.to_string(),
            name,
        });
    }
    if options.is_empty() {
        return None;
    }
    Some(AcpAgentEvent::AskQuestion {
        jsonrpc_id,
        title,
        question_id,
        prompt,
        options,
    })
}

/// Log one JSON-RPC line from agent stdout and translate recognized messages into bus events.
fn process_acp_line(line: &str, agent_id: &str) -> Vec<AgentEvent> {
    // Keep raw logs around while ACP support is still being brought up; these are invaluable
    // when a specific agent sends a shape we do not recognize yet.
    tracing::info!(raw = %line, "ACP message from agent");

    let Ok(message) = serde_json::from_str::<JsonRpcMessage>(line) else {
        tracing::warn!(raw = %line, "failed to parse ACP message");
        return Vec::new();
    };

    let events: Vec<AcpAgentEvent> = match message {
        JsonRpcMessage::Notification { method, params, .. } if method == "session/update" => {
            process_session_update(params)
        }
        JsonRpcMessage::Notification { method, .. } => {
            tracing::debug!(method, "ACP notification from agent");
            Vec::new()
        }
        JsonRpcMessage::Request {
            id, method, params, ..
        } => match method.as_str() {
            "session/request_permission" => {
                parse_permission_request(id, params).into_iter().collect()
            }
            "cursor/ask_question" | "_zed/askQuestion" => {
                parse_ask_question(id, params).into_iter().collect()
            }
            _ => {
                tracing::info!(?id, %method, "ACP request from agent (no handler)");
                Vec::new()
            }
        },
        JsonRpcMessage::Response {
            id, result, error, ..
        } => {
            if let Some(err) = &error {
                tracing::warn!(
                    ?id,
                    code = err.code,
                    message = %err.message,
                    "ACP JSON-RPC error from agent"
                );
                vec![
                    AcpAgentEvent::SessionStatus {
                        provider_error: Some(truncate_provider_error(&err.message)),
                    },
                    AcpAgentEvent::Progress {
                        id: "__turn".to_string(),
                        label: String::new(),
                        active: false,
                    },
                ]
            } else {
                tracing::debug!(?id, ?result, "ACP response from agent");
                if result
                    .as_ref()
                    .and_then(|result| result.get("stopReason"))
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                {
                    vec![AcpAgentEvent::Progress {
                        id: "__turn".to_string(),
                        label: String::new(),
                        active: false,
                    }]
                } else {
                    Vec::new()
                }
            }
        }
    };

    events
        .into_iter()
        .map(|event| AgentEvent::acp(agent_id, event))
        .collect()
}

fn process_session_update(params: serde_json::Value) -> Vec<AcpAgentEvent> {
    let session_id = params
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let Some(update) = params.get("update") else {
        tracing::warn!(session_id, "ACP session/update missing update payload");
        return Vec::new();
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");

    // Each arm may emit zero, one, or two events (e.g. tool_call → Progress + ToolCallMeta).
    match kind {
        "available_commands_update" => {
            let commands = update
                .get("availableCommands")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            tracing::info!(
                session_id,
                count = commands.len(),
                commands = %commands.join(", "),
                "ACP available commands updated"
            );
            Vec::new()
        }
        "agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk" => {
            let text = update
                .get("content")
                .and_then(content_text)
                .unwrap_or_default();
            tracing::info!(session_id, kind, text = %text, "ACP message chunk");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![AcpAgentEvent::TranscriptChunk {
                    kind: kind.to_string(),
                    text: text.to_string(),
                }]
            }
        }
        "tool_call" | "tool_call_update" => {
            let title = update
                .get("title")
                .and_then(serde_json::Value::as_str)
                .filter(|title| !title.is_empty())
                .unwrap_or("Working");
            let status = update
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let tool_call_id = update
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let tool_kind = update
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            tracing::info!(
                session_id,
                tool_call_id,
                title,
                status,
                ?tool_kind,
                "ACP tool call updated"
            );
            let active = status != "completed" && status != "failed" && status != "cancelled";
            vec![
                AcpAgentEvent::Progress {
                    id: tool_call_id.to_string(),
                    label: title.to_string(),
                    active,
                },
                AcpAgentEvent::ToolCallMeta {
                    tool_call_id: tool_call_id.to_string(),
                    title: (title != "Working").then(|| title.to_string()),
                    kind: tool_kind,
                },
            ]
        }
        "plan" => {
            let count = update
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            tracing::info!(session_id, count, "ACP plan updated");
            Vec::new()
        }
        other => {
            tracing::debug!(session_id, kind = other, "unhandled ACP session update");
            Vec::new()
        }
    }
}

fn looks_like_provider_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("cannot connect")
        || lower.contains("failed")
        || lower.contains("unable to connect")
}

fn truncate_provider_error(message: &str) -> String {
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

fn content_text(content: &serde_json::Value) -> Option<&str> {
    match content.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => content.get("text").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_acp_line_tags_events_with_agent_id() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#;
        let events = process_acp_line(line, "reviewer");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                agent_id,
                event: AcpAgentEvent::TranscriptChunk { kind, text }
            } if agent_id == "reviewer" && kind == "agent_message_chunk" && text == "hi"
        ));
    }

    #[test]
    fn agent_event_acp_constructor() {
        let event = AgentEvent::acp(
            "coder",
            AcpAgentEvent::Progress {
                id: "t1".to_string(),
                label: "Working".to_string(),
                active: true,
            },
        );
        assert!(matches!(
            event,
            AgentEvent::Acp {
                agent_id,
                event: AcpAgentEvent::Progress { id, .. }
            } if agent_id == "coder" && id == "t1"
        ));
    }

    #[test]
    fn process_acp_line_parses_ask_question_methods() {
        let params = r#"{"title":"Pick one","questions":[{"id":"q1","prompt":"Which?","options":[{"id":"a","label":"A"},{"id":"b","label":"B"}]}]}"#;
        for method in ["cursor/ask_question", "_zed/askQuestion"] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{method}","params":{params}}}"#);
            let events = process_acp_line(&line, "orchestrator");
            assert_eq!(events.len(), 1);
            assert!(matches!(
                &events[0],
                AgentEvent::Acp {
                    agent_id,
                    event: AcpAgentEvent::AskQuestion {
                        title,
                        question_id,
                        prompt,
                        ..
                    }
                } if agent_id == "orchestrator"
                    && title == "Pick one"
                    && question_id == "q1"
                    && prompt == "Which?"
            ));
        }
    }

    #[test]
    fn new_session_result_extracts_selected_model() {
        let json = r#"{
            "sessionId": "ses_test",
            "configOptions": [
                {"id": "model", "name": "Model", "currentValue": "lemonade/Gemma-4-26B-A4B-it-GGUF"}
            ]
        }"#;
        let result: NewSessionResult = serde_json::from_str(json).expect("parse session/new");
        assert_eq!(
            result.selected_model().as_deref(),
            Some("lemonade/Gemma-4-26B-A4B-it-GGUF")
        );
    }

    #[test]
    fn resolve_model_value_maps_cli_slug_to_agent_acp_id() {
        let json = r#"{
            "sessionId": "ses_test",
            "configOptions": [{
                "id": "model",
                "currentValue": "composer-2.5[fast=true]",
                "options": [
                    {"value": "default[]", "name": "Auto"},
                    {"value": "composer-2.5[fast=true]", "name": "composer-2.5"},
                    {"value": "gpt-5.3-codex[reasoning=medium,fast=false]", "name": "gpt-5.3-codex"}
                ]
            }]
        }"#;
        let result: NewSessionResult = serde_json::from_str(json).expect("parse session/new");
        assert_eq!(
            result.resolve_model_value("composer-2.5").as_deref(),
            Some("composer-2.5[fast=true]")
        );
        assert_eq!(
            result.resolve_model_value("gpt-5.3-codex").as_deref(),
            Some("gpt-5.3-codex[reasoning=medium,fast=false]")
        );
        assert_eq!(result.resolve_model_value("unknown-model"), None);
    }

    #[test]
    fn resolve_model_value_passes_through_when_no_options() {
        let json = r#"{
            "sessionId": "ses_test",
            "configOptions": [
                {"id": "model", "currentValue": "opencode/deepseek-v4-flash-free"}
            ]
        }"#;
        let result: NewSessionResult = serde_json::from_str(json).expect("parse session/new");
        assert_eq!(
            result
                .resolve_model_value("opencode/deepseek-v4-flash-free")
                .as_deref(),
            Some("opencode/deepseek-v4-flash-free")
        );
    }

    #[test]
    fn set_config_option_params_serialize_to_acp_shape() {
        let params = SetConfigOptionParams {
            session_id: "sess_test".to_string(),
            config_id: "model".to_string(),
            value: "claude-sonnet".to_string(),
        };
        let json = serde_json::to_value(params).expect("serialize");
        assert_eq!(json["sessionId"], "sess_test");
        assert_eq!(json["configId"], "model");
        assert_eq!(json["value"], "claude-sonnet");
    }

    #[test]
    fn set_config_option_result_extracts_selected_model() {
        let json = r#"{
            "configOptions": [
                {"id": "model", "currentValue": "claude-sonnet"}
            ]
        }"#;
        let result: SetConfigOptionResult =
            serde_json::from_str(json).expect("parse set_config_option");
        assert_eq!(result.selected_model().as_deref(), Some("claude-sonnet"));
    }

    #[tokio::test]
    async fn set_config_option_after_new_session() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("via-acp-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("mock_acp.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess_test","configOptions":[{"id":"model","currentValue":"old-model"}]}}\n' "$id"
      ;;
    *'"method":"session/set_config_option"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[{"id":"model","currentValue":"new-model"}]}}\n' "$id"
      ;;
  esac
done
"#,
        )
        .expect("write mock script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod mock script");

        let mut client = AcpClient::spawn("test", script.to_str().unwrap(), &[], &[])
            .await
            .expect("spawn mock agent");
        client.initialize().await.expect("initialize");
        let session = client
            .new_session(Path::new("/tmp"))
            .await
            .expect("new_session");
        assert_eq!(session.session_id, "sess_test");
        assert_eq!(session.selected_model().as_deref(), Some("old-model"));

        let updated = client
            .set_config_option(&session.session_id, "model", "new-model")
            .await
            .expect("set_config_option");
        assert_eq!(updated.selected_model().as_deref(), Some("new-model"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_acp_line_parses_v1_permission_with_tool_kind() {
        let line = r#"{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"call_1","title":"Read README","kind":"read"},"options":[{"optionId":"allow-once","name":"Allow once"}]}}"#;
        let events = process_acp_line(line, "coder");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                agent_id,
                event: AcpAgentEvent::PermissionRequest {
                    session_id,
                    tool_call_id,
                    title,
                    tool_kind,
                    command,
                    command_cwd,
                    ..
                }
            } if agent_id == "coder"
                && session_id == "s1"
                && tool_call_id == "call_1"
                && title == "Read README"
                && tool_kind.as_deref() == Some("read")
                && command.is_none()
                && command_cwd.is_none()
        ));
    }

    #[test]
    fn process_acp_line_parses_v2_command_permission_subject() {
        let line = r#"{"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"sessionId":"s1","title":"Run tests?","subject":{"type":"command","command":"via task list","cwd":"/home/user/project","toolCallId":"call_2"},"options":[{"optionId":"allow-once","name":"Allow once"}]}}"#;
        let events = process_acp_line(line, "coder");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                event: AcpAgentEvent::PermissionRequest {
                    title,
                    tool_call_id,
                    command,
                    command_cwd,
                    tool_kind,
                    ..
                },
                ..
            } if title == "Run tests?"
                && tool_call_id == "call_2"
                && command.as_deref() == Some("via task list")
                && command_cwd.as_deref() == Some("/home/user/project")
                && tool_kind.is_none()
        ));
    }

    #[test]
    fn process_acp_line_parses_v2_tool_call_permission_subject() {
        let line = r#"{"jsonrpc":"2.0","id":11,"method":"session/request_permission","params":{"sessionId":"s1","title":"Search workspace","subject":{"type":"tool_call","toolCall":{"toolCallId":"call_3","kind":"search"}},"options":[{"optionId":"allow-once","name":"Allow once"}]}}"#;
        let events = process_acp_line(line, "coder");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                event: AcpAgentEvent::PermissionRequest {
                    title,
                    tool_call_id,
                    tool_kind,
                    command,
                    ..
                },
                ..
            } if title == "Search workspace"
                && tool_call_id == "call_3"
                && tool_kind.as_deref() == Some("search")
                && command.is_none()
        ));
    }

    #[test]
    fn process_session_update_emits_tool_call_meta_with_kind() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call_4",
                "title": "Run shell",
                "status": "in_progress",
                "kind": "execute"
            }
        });
        let events = process_session_update(params);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            AcpAgentEvent::Progress {
                id,
                label,
                active: true,
            } if id == "call_4" && label == "Run shell"
        ));
        assert!(matches!(
            &events[1],
            AcpAgentEvent::ToolCallMeta {
                tool_call_id,
                title,
                kind,
            } if tool_call_id == "call_4"
                && title.as_deref() == Some("Run shell")
                && kind.as_deref() == Some("execute")
        ));
    }

    #[test]
    fn process_acp_line_parses_subject_with_tool_call_but_no_type() {
        let line = r#"{"jsonrpc":"2.0","id":12,"method":"session/request_permission","params":{"sessionId":"s1","title":"Legacy subject","subject":{"toolCall":{"toolCallId":"call_5","kind":"read"}},"options":[{"optionId":"allow-once","name":"Allow once"}]}}"#;
        let events = process_acp_line(line, "coder");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                event: AcpAgentEvent::PermissionRequest {
                    title,
                    tool_call_id,
                    tool_kind,
                    command,
                    ..
                },
                ..
            } if title == "Legacy subject"
                && tool_call_id == "call_5"
                && tool_kind.as_deref() == Some("read")
                && command.is_none()
        ));
    }

    #[test]
    fn process_acp_line_parses_unknown_permission_subject_type() {
        let line = r#"{"jsonrpc":"2.0","id":13,"method":"session/request_permission","params":{"sessionId":"s1","title":"Future subject","subject":{"type":"future_thing","toolCallId":"call_6"},"options":[{"optionId":"allow-once","name":"Allow once"}]}}"#;
        let events = process_acp_line(line, "coder");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                event: AcpAgentEvent::PermissionRequest {
                    title,
                    tool_call_id,
                    tool_kind,
                    command,
                    command_cwd,
                    ..
                },
                ..
            } if title == "Future subject"
                && tool_call_id == "call_6"
                && tool_kind.is_none()
                && command.is_none()
                && command_cwd.is_none()
        ));
    }

    #[test]
    fn process_acp_line_surfaces_jsonrpc_error_as_session_status() {
        let line =
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"Cannot connect to API"}}"#;
        let events = process_acp_line(line, "reviewer");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            AgentEvent::Acp {
                agent_id,
                event: AcpAgentEvent::SessionStatus { provider_error }
            } if agent_id == "reviewer"
                && provider_error.as_deref() == Some("Cannot connect to API")
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::Acp {
                agent_id,
                event: AcpAgentEvent::Progress { id, active, .. }
            } if agent_id == "reviewer" && id == "__turn" && !active
        ));
    }
}
