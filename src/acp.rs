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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default)]
    config_options: Vec<ConfigOption>,
}

impl NewSessionResult {
    /// Selected model from `session/new` config options (e.g. `lemonade/Gemma-…`).
    pub fn selected_model(&self) -> Option<String> {
        self.config_options
            .iter()
            .find(|option| option.id == "model")
            .and_then(|option| option.current_value.clone())
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

fn parse_permission_request(
    jsonrpc_id: serde_json::Value,
    params: serde_json::Value,
) -> Option<AcpAgentEvent> {
    let session_id = params
        .get("sessionId")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let tool_call = params.get("toolCall")?;
    let tool_call_id = tool_call
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let title = tool_call
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Permission required")
        .to_string();
    let options_arr = params.get("options")?.as_array()?;
    let mut options = Vec::new();
    for item in options_arr {
        let option_id = item.get("optionId").and_then(serde_json::Value::as_str)?;
        let name = item
            .get("name")
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
    Some(AcpAgentEvent::PermissionRequest {
        jsonrpc_id,
        session_id,
        tool_call_id,
        title,
        options,
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
            process_session_update(params).into_iter().collect()
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

fn process_session_update(params: serde_json::Value) -> Option<AcpAgentEvent> {
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
                Some(AcpAgentEvent::TranscriptChunk {
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
            Some(AcpAgentEvent::Progress {
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
            Some(AcpAgentEvent::Progress {
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
