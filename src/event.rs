#![allow(dead_code)]

use std::path::PathBuf;

use crate::lsp_bridge;

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
    LspClientsChanged {
        clients: Vec<lsp_bridge::LspClientInfo>,
    },
    AgentInput {
        payload: String,
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
    },
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    OutputChunk(String),
    FileReference { path: PathBuf, line: Option<u32> },
}
