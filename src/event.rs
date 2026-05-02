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
    Resize { columns: u16, rows: u16 },
}

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    OutputChunk(String),
    FileReference { path: PathBuf, line: Option<u32> },
}
