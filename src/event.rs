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
    },
    /// JSON-RPC response written to the ACP agent stdin (`id` + `result` only).
    AcpJsonRpcResult {
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
    CursorAskQuestion { question_id: String },
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
    },
    AcpTranscriptChunk {
        kind: String,
        text: String,
    },
    AcpProgress {
        id: String,
        label: String,
        active: bool,
    },
    /// Centered modal: ACP `session/request_permission` or Cursor `cursor/ask_question`.
    AcpModalPrompt {
        jsonrpc_id: serde_json::Value,
        title: String,
        message: String,
        options: Vec<AcpPermissionOption>,
        kind: AcpModalKind,
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
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    OutputChunk(String),
    FileReference {
        path: PathBuf,
        line: Option<u32>,
    },
    AcpTranscriptChunk {
        kind: String,
        text: String,
    },
    AcpProgress {
        id: String,
        label: String,
        active: bool,
    },
    /// `session/request_permission` from the agent.
    AcpPermissionRequest {
        jsonrpc_id: serde_json::Value,
        session_id: String,
        tool_call_id: String,
        title: String,
        options: Vec<AcpPermissionOption>,
    },
    /// Cursor `cursor/ask_question` blocking request.
    AcpCursorAskQuestion {
        jsonrpc_id: serde_json::Value,
        title: String,
        question_id: String,
        prompt: String,
        options: Vec<AcpPermissionOption>,
    },
}
