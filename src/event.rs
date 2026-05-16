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
    OpenRequested { path: PathBuf, line: Option<u32> },
    SymbolOpenRequested { symbol: String },
    AgentPromptSubmitted { text: String },
    Resize { columns: u16, rows: u16 },
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
}
