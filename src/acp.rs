use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::debug;

use crate::event::{AgentEvent, Event};
use crate::mediator::EventSender;

/// Minimal ACP (Agent Client Protocol) client.
///
/// This implements a lightweight JSON-RPC 2.0 client that speaks the
/// Agent Client Protocol over stdio with a subprocess agent.
pub struct AcpClient {
    child: Child,
    next_id: u64,
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
#[serde(rename_all = "snake_case")]
pub struct ContextUpdateParams {
    pub active_buffer: Option<BufferContext>,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BufferContext {
    pub path: String,
    pub line: u32,
    pub column: u32,
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
pub struct NewSessionResult {
    pub session_id: String,
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
        id: u64,
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
        id: u64,
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
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        debug!(command, ?args, "spawning ACP agent subprocess");
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn ACP agent: {command}"))?;

        // Start a background task that logs everything the agent prints to stderr.
        // This is crucial for seeing error messages, prompts, or crashes.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::warn!(stderr = %line, "ACP agent stderr");
                    }
                }
            });
        }

        debug!(command, "ACP agent process started");
        Ok(Self { child, next_id: 1 })
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

    /// Spawn a background task that continuously reads JSON-RPC lines
    /// from the agent's stdout and logs them at info level.
    ///
    /// This is the simplest way to observe streaming responses and
    /// notifications while we build proper rendering.
    pub fn spawn_reader(&mut self, events: EventSender) {
        if let Some(stdout) = self.child.stdout.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        if let Some(event) = log_acp_line(&line) {
                            events.send(Event::Agent(event)).await;
                        }
                    }
                }
                tracing::debug!("ACP agent stdout closed");
            });
        }
    }

    /// Push updated editor context to the agent.
    ///
    /// This is the primary mechanism for keeping the agent aware of the
    /// current buffer, cursor position, etc. It replaces any previous
    /// context for the active session.
    pub async fn update_context(&mut self, params: ContextUpdateParams) -> Result<()> {
        self.send_notification("context/update", serde_json::to_value(params)?)
            .await
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

    pub async fn prompt_context(
        &mut self,
        session_id: &str,
        path: &Path,
        display_path: &Path,
        line_range: Option<(u32, u32)>,
    ) -> Result<()> {
        let mut prompt_text = format!("Use `{}` as context.", display_path.display());
        let resource = selected_file_resource(path, line_range).await?;

        if let Some((start, end)) = line_range {
            prompt_text = format!("Use `{}:{start}-{end}` as context.", display_path.display());
        }

        self.prompt(session_id, &prompt_text, resource).await
    }

    async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = JsonRpcMessage::Request {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        self.write_message(&msg).await?;
        self.read_response(id).await
    }

    async fn send_notification(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let msg = JsonRpcMessage::Notification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };

        self.write_message(&msg).await
    }

    async fn send_request_detached(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = JsonRpcMessage::Request {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        self.write_message(&msg).await
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        debug!(json = %json, "writing ACP message to agent stdin");
        let stdin = self
            .child
            .stdin
            .as_mut()
            .context("agent stdin unavailable")?;
        stdin.write_all(json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        debug!("ACP message flushed to agent");
        Ok(())
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
                } => {
                    if id == expected_id {
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
                }
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

fn log_acp_line(line: &str) -> Option<AgentEvent> {
    // Keep raw logs around while ACP support is still being brought up; these are invaluable
    // when a specific agent sends a shape we do not recognize yet.
    tracing::info!(raw = %line, "ACP message from agent");

    let Ok(message) = serde_json::from_str::<JsonRpcMessage>(line) else {
        tracing::warn!(raw = %line, "failed to parse ACP message");
        return None;
    };

    match message {
        JsonRpcMessage::Notification { method, params, .. } if method == "session/update" => {
            log_session_update(params)
        }
        JsonRpcMessage::Notification { method, .. } => {
            tracing::debug!(method, "ACP notification from agent");
            None
        }
        JsonRpcMessage::Request { id, method, .. } => {
            tracing::info!(id, method, "ACP request from agent");
            None
        }
        JsonRpcMessage::Response {
            id, result, error, ..
        } => {
            tracing::debug!(id, ?result, ?error, "ACP response from agent");
            if result
                .as_ref()
                .and_then(|result| result.get("stopReason"))
                .and_then(serde_json::Value::as_str)
                .is_some()
            {
                Some(AgentEvent::AcpProgress {
                    id: "__turn".to_string(),
                    label: String::new(),
                    active: false,
                })
            } else {
                None
            }
        }
    }
}

fn log_session_update(params: serde_json::Value) -> Option<AgentEvent> {
    let session_id = params
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let Some(update) = params.get("update") else {
        tracing::warn!(session_id, "ACP session/update missing update payload");
        return None;
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");

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
            None
        }
        "agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk" => {
            let text = update
                .get("content")
                .and_then(content_text)
                .unwrap_or_default();
            tracing::info!(session_id, kind, text = %text, "ACP message chunk");
            if text.is_empty() {
                None
            } else {
                Some(AgentEvent::AcpTranscriptChunk {
                    kind: kind.to_string(),
                    text: text.to_string(),
                })
            }
        }
        "tool_call" => {
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
            tracing::info!(session_id, tool_call_id, title, status, "ACP tool call");
            Some(AgentEvent::AcpProgress {
                id: tool_call_id.to_string(),
                label: title.to_string(),
                active: status != "completed" && status != "failed" && status != "cancelled",
            })
        }
        "tool_call_update" => {
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
            tracing::info!(
                session_id,
                tool_call_id,
                title,
                status,
                "ACP tool call updated"
            );
            Some(AgentEvent::AcpProgress {
                id: tool_call_id.to_string(),
                label: title.to_string(),
                active: status != "completed" && status != "failed" && status != "cancelled",
            })
        }
        "plan" => {
            let count = update
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            tracing::info!(session_id, count, "ACP plan updated");
            None
        }
        other => {
            tracing::debug!(session_id, kind = other, "unhandled ACP session update");
            None
        }
    }
}

fn content_text(content: &serde_json::Value) -> Option<&str> {
    match content.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => content.get("text").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

async fn selected_file_resource(
    path: &Path,
    line_range: Option<(u32, u32)>,
) -> Result<Option<PromptResource>> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read context file {}", path.display()))?;
    let text = match line_range {
        Some((start, end)) => {
            let start = start.max(1) as usize;
            let end = end.max(start as u32) as usize;
            text.lines()
                .skip(start - 1)
                .take(end - start + 1)
                .collect::<Vec<_>>()
                .join("\n")
        }
        None => text,
    };

    if text.trim().is_empty() {
        return Ok(None);
    }

    let uri = match line_range {
        Some((start, end)) => format!("file://{}#L{start}-L{end}", path.display()),
        None => format!("file://{}", path.display()),
    };

    Ok(Some(PromptResource {
        uri,
        mime_type: Some("text/plain".to_string()),
        text,
    }))
}
