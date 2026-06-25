#![allow(dead_code)]

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Event {
    Ui(UiEvent),
    Editor(EditorEvent),
    Agent(AgentEvent),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    OpenRequested {
        path: PathBuf,
        line: Option<u32>,
    },
    SymbolOpenRequested {
        symbol: String,
    },
    ReviewRequested,
    AgentPromptSubmitted {
        text: String,
        /// Which ACP agent the prompt was typed into (None = primary/orchestrator).
        agent_id: Option<String>,
    },
    /// JSON-RPC response written to the ACP agent stdin (`id` + `result` only).
    AcpJsonRpcResult {
        /// ACP agent whose session this result belongs to.
        agent_id: String,
        id: serde_json::Value,
        result: serde_json::Value,
    },
    Resize {
        columns: u16,
        rows: u16,
    },
}

/// ACP permission or Cursor ask-question option (`optionId` + display `name` / `label`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPermissionOption {
    pub option_id: String,
    pub name: String,
}

/// How to build the JSON-RPC `result` when the user answers the modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpModalKind {
    SessionPermission,
    AskQuestion { question_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    EditorContextChanged {
        path: PathBuf,
        line: u32,
        column: u32,
    },
    VisualSelectionChanged {
        path: PathBuf,
        start_line: u32,
        end_line: u32,
    },
    AgentInput {
        payload: String,
        focus_agent: bool,
        /// If Some(id), route to the AgentTerminal pane whose id matches.
        /// If None or not found, fall back to the first agent pane.
        target_agent_id: Option<String>,
    },
    AcpTranscriptChunk {
        agent_id: String,
        kind: String,
        text: String,
    },
    AcpProgress {
        agent_id: String,
        id: String,
        label: String,
        active: bool,
    },
    /// Model selection and provider connectivity for the ACP pane header.
    AcpSessionStatus {
        agent_id: String,
        model: Option<String>,
        provider_error: Option<String>,
        clear_provider_error: bool,
    },
    /// Centered modal: ACP `session/request_permission` or agent ask-question requests.
    AcpModalPrompt {
        agent_id: String,
        jsonrpc_id: serde_json::Value,
        title: String,
        message: String,
        options: Vec<AcpPermissionOption>,
        kind: AcpModalKind,
    },
    /// Request from orchestrator or Lua to spawn a new agent pane.
    SpawnAgent {
        id: String,
        role: Option<String>,
        command: Option<String>,
    },
    /// Close a sub-agent pane and tear down its session (PTY or ACP).
    TerminateAgent {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    ActiveBufferChanged {
        path: PathBuf,
        line: u32,
        column: u32,
    },
    DiagnosticsChanged {
        path: PathBuf,
        error_count: usize,
        warning_count: usize,
    },
    VisualSelectionChanged {
        path: PathBuf,
        start_line: u32,
        end_line: u32,
        text: String,
    },
    BufferSendRequested {
        path: PathBuf,
        start_line: Option<u32>,
        end_line: Option<u32>,
    },
    AgentSend {
        agent_id: Option<String>,
        /// Sender id, used to build a mailbox ping for PTY recipients.
        from: Option<String>,
        content: String,
        focus: bool,
    },
    SpawnAgent {
        id: String,
        role: Option<String>,
        command: Option<String>,
    },
    TerminateAgent {
        id: String,
    },
}

/// ACP protocol payload from an agent subprocess (routing id lives on `AgentEvent::Acp`).
#[derive(Debug, Clone)]
pub enum AcpAgentEvent {
    TranscriptChunk {
        kind: String,
        text: String,
    },
    Progress {
        id: String,
        label: String,
        active: bool,
    },
    /// `session/request_permission` from the agent.
    PermissionRequest {
        jsonrpc_id: serde_json::Value,
        session_id: String,
        tool_call_id: String,
        title: String,
        options: Vec<AcpPermissionOption>,
    },
    /// Agent ask-question blocking request (e.g. `cursor/ask_question`, `_zed/askQuestion`).
    AskQuestion {
        jsonrpc_id: serde_json::Value,
        title: String,
        question_id: String,
        prompt: String,
        options: Vec<AcpPermissionOption>,
    },
    /// Provider connectivity or JSON-RPC failure surfaced for the ACP pane header.
    SessionStatus {
        provider_error: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    OutputChunk(String),
    FileReference {
        path: PathBuf,
        line: Option<u32>,
    },
    /// ACP reader output — always tagged for pane routing when several agents run.
    Acp {
        agent_id: String,
        event: AcpAgentEvent,
    },
}

impl AgentEvent {
    pub fn acp(agent_id: impl Into<String>, event: AcpAgentEvent) -> Self {
        Self::Acp {
            agent_id: agent_id.into(),
            event,
        }
    }
}
