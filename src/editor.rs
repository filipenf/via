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
    pub visual_selection: Option<VisualSelection>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSelection {
    pub path: PathBuf,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
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
            EditorEvent::VisualSelectionChanged {
                path,
                start_line,
                end_line,
                text,
            } => {
                self.visual_selection = Some(VisualSelection {
                    path,
                    start_line,
                    end_line,
                    text,
                });
            }
            EditorEvent::BufferSendRequested { .. } => {
                // one-shot request, no state to update
            }
            EditorEvent::AgentSend { .. } => {
                // one-shot request, no state to update
            }
            EditorEvent::SpawnAgent { .. } => {
                // one-shot request, handled by mediator/ui
            }
            EditorEvent::TerminateAgent { .. } => {
                // one-shot request, handled by mediator/ui
            }
            EditorEvent::ReviewGateOpened { .. } => {
                // one-shot request, handled by mediator (opens review UI)
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
    VisualSelectionChanged {
        path: String,
        start_line: u32,
        end_line: u32,
        #[serde(default)]
        text: String,
    },
    BufferSendRequested {
        path: String,
        #[serde(default)]
        start_line: Option<u32>,
        #[serde(default)]
        end_line: Option<u32>,
    },
    AgentSend {
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        from: Option<String>,
        content: String,
        #[serde(default = "default_true")]
        focus: bool,
    },
    SpawnAgent {
        id: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
    TerminateAgent {
        id: String,
    },
    ReviewRequested {
        task_id: String,
        #[serde(default)]
        title: String,
    },
}

fn default_true() -> bool {
    true
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
        WireEditorEvent::VisualSelectionChanged {
            path,
            start_line,
            end_line,
            text,
        } => EditorEvent::VisualSelectionChanged {
            path: resolve_path(&path, working_directory),
            start_line,
            end_line,
            text,
        },
        WireEditorEvent::BufferSendRequested {
            path,
            start_line,
            end_line,
        } => EditorEvent::BufferSendRequested {
            path: resolve_path(&path, working_directory),
            start_line,
            end_line,
        },
        WireEditorEvent::AgentSend {
            agent_id,
            from,
            content,
            focus,
        } => EditorEvent::AgentSend {
            agent_id,
            from,
            content,
            focus,
        },
        WireEditorEvent::SpawnAgent { id, role, command } => {
            EditorEvent::SpawnAgent { id, role, command }
        }
        WireEditorEvent::TerminateAgent { id } => EditorEvent::TerminateAgent { id },
        WireEditorEvent::ReviewRequested { task_id, title } => {
            EditorEvent::ReviewGateOpened { task_id, title }
        }
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
    fn parses_visual_selection_changed_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"visual_selection_changed","path":"src/main.rs","start_line":3,"end_line":8}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::VisualSelectionChanged {
                path: PathBuf::from("/repo/src/main.rs"),
                start_line: 3,
                end_line: 8,
                text: String::new(),
            }
        );
    }

    #[test]
    fn parses_spawn_agent_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"spawn_agent","id":"reviewer","role":"reviewer","command":"opencode acp"}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::SpawnAgent {
                id: "reviewer".to_string(),
                role: Some("reviewer".to_string()),
                command: Some("opencode acp".to_string()),
            }
        );
    }

    #[test]
    fn parses_terminate_agent_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"terminate_agent","id":"reviewer"}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::TerminateAgent {
                id: "reviewer".to_string(),
            }
        );
    }

    #[test]
    fn parses_review_requested_event() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"review_requested","task_id":"t1","title":"Do thing"}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::ReviewGateOpened {
                task_id: "t1".to_string(),
                title: "Do thing".to_string(),
            }
        );
    }

    #[test]
    fn parses_review_requested_event_without_title() {
        assert_eq!(
            parse_editor_event(
                r#"{"type":"review_requested","task_id":"t1"}"#,
                Path::new("/repo")
            )
            .unwrap(),
            EditorEvent::ReviewGateOpened {
                task_id: "t1".to_string(),
                title: String::new(),
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
        state.apply(EditorEvent::VisualSelectionChanged {
            path: PathBuf::from("/repo/src/main.rs"),
            start_line: 3,
            end_line: 8,
            text: "fn main() {}".to_string(),
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
        assert_eq!(
            state.visual_selection,
            Some(VisualSelection {
                path: PathBuf::from("/repo/src/main.rs"),
                start_line: 3,
                end_line: 8,
                text: "fn main() {}".to_string(),
            })
        );
    }
}
