use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::event::{EditorEvent, Event};
use crate::mediator::EventSender;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorState {
    pub active_buffer: Option<ActiveBuffer>,
    pub diagnostics: HashMap<PathBuf, DiagnosticSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBuffer {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub error_count: usize,
    pub warning_count: usize,
}

impl EditorState {
    pub fn apply(&mut self, event: EditorEvent) {
        match event {
            EditorEvent::ActiveBufferChanged { path, line, column } => {
                self.active_buffer = Some(ActiveBuffer { path, line, column });
            }
            EditorEvent::DiagnosticsChanged {
                path,
                error_count,
                warning_count,
            } => {
                self.diagnostics.insert(
                    path,
                    DiagnosticSummary {
                        error_count,
                        warning_count,
                    },
                );
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEditorEvent {
    ActiveBufferChanged {
        path: String,
        line: u32,
        column: u32,
    },
    DiagnosticsChanged {
        path: String,
        error_count: usize,
        warning_count: usize,
    },
}

pub fn spawn_listener(
    socket_path: PathBuf,
    working_directory: PathBuf,
    events: EventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run_listener(socket_path, working_directory, events).await {
            error!(%error, "editor context listener stopped");
        }
    })
}

async fn run_listener(
    socket_path: PathBuf,
    working_directory: PathBuf,
    events: EventSender,
) -> Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).with_context(|| {
            format!(
                "failed to remove stale editor socket {}",
                socket_path.display()
            )
        })?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind editor socket {}", socket_path.display()))?;
    let _socket_file = SocketFile {
        path: socket_path.clone(),
    };

    info!(socket = %socket_path.display(), "editor context listener ready");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept editor context connection")?;
        let working_directory = working_directory.clone();
        let events = events.clone();

        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, &working_directory, events).await {
                debug!(%error, "editor context connection closed");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    working_directory: &Path,
    events: EventSender,
) -> Result<()> {
    let mut lines = BufReader::new(stream).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read editor context message")?
    {
        if line.trim().is_empty() {
            continue;
        }

        match parse_editor_event(&line, working_directory) {
            Ok(event) => events.send(Event::Editor(event)).await,
            Err(error) => warn!(%error, message = %line, "invalid editor context message"),
        }
    }

    Ok(())
}

pub fn parse_editor_event(input: &str, working_directory: &Path) -> Result<EditorEvent> {
    let wire: WireEditorEvent =
        serde_json::from_str(input).context("failed to parse editor context JSON")?;

    Ok(match wire {
        WireEditorEvent::ActiveBufferChanged { path, line, column } => {
            EditorEvent::ActiveBufferChanged {
                path: resolve_path(&path, working_directory),
                line,
                column,
            }
        }
        WireEditorEvent::DiagnosticsChanged {
            path,
            error_count,
            warning_count,
        } => EditorEvent::DiagnosticsChanged {
            path: resolve_path(&path, working_directory),
            error_count,
            warning_count,
        },
    })
}

fn resolve_path(path: &str, working_directory: &Path) -> PathBuf {
    let path = PathBuf::from(path);

    if path.is_absolute() {
        path
    } else {
        working_directory.join(path)
    }
}

struct SocketFile {
    path: PathBuf,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_buffer_changed_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"active_buffer_changed","path":"src/main.rs","line":42,"column":7}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::ActiveBufferChanged {
                path: PathBuf::from("/repo/src/main.rs"),
                line: 42,
                column: 7,
            }
        );
    }

    #[test]
    fn parses_diagnostics_changed_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"diagnostics_changed","path":"/repo/src/main.rs","error_count":1,"warning_count":3}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::DiagnosticsChanged {
                path: PathBuf::from("/repo/src/main.rs"),
                error_count: 1,
                warning_count: 3,
            }
        );
    }

    #[test]
    fn applies_editor_events_to_state() {
        let mut state = EditorState::default();

        state.apply(EditorEvent::ActiveBufferChanged {
            path: PathBuf::from("/repo/src/main.rs"),
            line: 10,
            column: 2,
        });
        state.apply(EditorEvent::DiagnosticsChanged {
            path: PathBuf::from("/repo/src/main.rs"),
            error_count: 1,
            warning_count: 2,
        });

        assert_eq!(
            state.active_buffer,
            Some(ActiveBuffer {
                path: PathBuf::from("/repo/src/main.rs"),
                line: 10,
                column: 2,
            })
        );
        assert_eq!(
            state.diagnostics.get(Path::new("/repo/src/main.rs")),
            Some(&DiagnosticSummary {
                error_count: 1,
                warning_count: 2,
            })
        );
    }
}
