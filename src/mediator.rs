use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::config::Config;
use crate::editor::{self, EditorState};
use crate::event::{EditorEvent, Event, UiCommand, UiEvent};
use crate::nvim::{self, FileTarget};

const EVENT_BUFFER_SIZE: usize = 128;

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    ui_commands: mpsc::Sender<UiCommand>,
    editor_state: EditorState,
    in_flight_symbol_open: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct EventSender {
    events: mpsc::Sender<Event>,
}

pub struct MediatorHandle {
    events: EventSender,
    ui_commands: Option<mpsc::Receiver<UiCommand>>,
    stopped: oneshot::Receiver<()>,
    editor_listener: JoinHandle<()>,
}

impl Mediator {
    pub fn new(config: Config) -> Self {
        let (_events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (ui_commands_tx, _ui_commands_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        Self {
            config,
            events: events_rx,
            ui_commands: ui_commands_tx,
            editor_state: EditorState::default(),
            in_flight_symbol_open: None,
        }
    }

    pub fn spawn(mut self) -> MediatorHandle {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (ui_commands_tx, ui_commands_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        self.events = events_rx;
        self.ui_commands = ui_commands_tx;
        let events = EventSender {
            events: events_tx.clone(),
        };
        let editor_listener = editor::spawn_listener(
            self.config.editor_socket_path.clone(),
            self.config.working_directory.clone(),
            events.clone(),
        );

        let (stopped_tx, stopped_rx) = oneshot::channel();
        tokio::spawn(async move {
            self.run().await;
            let _ = stopped_tx.send(());
        });

        MediatorHandle {
            events,
            ui_commands: Some(ui_commands_rx),
            stopped: stopped_rx,
            editor_listener,
        }
    }

    async fn run(&mut self) {
        info!(
            nvim_command = %self.config.nvim_command,
            nvim_socket = %self.config.nvim_socket_path.display(),
            editor_socket = %self.config.editor_socket_path.display(),
            agent_configured = self.config.agent_command.is_some(),
            "mediator ready"
        );

        while let Some(event) = self.events.recv().await {
            match event {
                Event::Shutdown => {
                    if let Some(task) = self.in_flight_symbol_open.take() {
                        task.abort();
                    }
                    info!("mediator received shutdown");
                    break;
                }
                Event::Ui(UiEvent::OpenRequested { path, line }) => {
                    let target = FileTarget { path, line };

                    if let Err(error) = nvim::open_file(
                        &self.config.nvim_socket_path,
                        &self.config.working_directory,
                        target,
                    )
                    .await
                    {
                        error!(%error, "failed to open file in Neovim");
                    }
                }
                Event::Ui(UiEvent::SymbolOpenRequested { symbol }) => {
                    if let Some(task) = self.in_flight_symbol_open.take() {
                        task.abort();
                    }

                    let socket_path = self.config.nvim_socket_path.clone();
                    self.in_flight_symbol_open = Some(tokio::spawn(async move {
                        if let Err(error) = nvim::open_symbol(&socket_path, &symbol).await {
                            error!(%error, symbol, "failed to open symbol in Neovim");
                        }
                    }));
                }
                Event::Editor(event) => self.apply_editor_event(event),
                event => debug!(?event, "mediator event received"),
            }
        }
    }

    fn apply_editor_event(&mut self, event: EditorEvent) {
        let previous_path = self
            .editor_state
            .active_buffer
            .as_ref()
            .map(|buffer| buffer.path.clone());
        let previous_selection = self.editor_state.visual_selection.clone();

        debug!(?event, "editor context updated");
        match &event {
            EditorEvent::ActiveBufferChanged { path, line, column } => {
                if previous_path.as_ref() != Some(path) {
                    self.send_ui_command(UiCommand::EditorContextChanged {
                        path: path.clone(),
                        line: *line,
                        column: *column,
                    });
                }
            }
            EditorEvent::VisualSelectionChanged {
                path,
                start_line,
                end_line,
            } => {
                let changed = previous_selection
                    .as_ref()
                    .map(|selection| {
                        selection.path != *path
                            || selection.start_line != *start_line
                            || selection.end_line != *end_line
                    })
                    .unwrap_or(true);

                if changed {
                    self.send_ui_command(UiCommand::VisualSelectionChanged {
                        path: path.clone(),
                        start_line: *start_line,
                        end_line: *end_line,
                    });
                }
            }
            EditorEvent::DiagnosticsChanged { .. } => {}
        }

        self.editor_state.apply(event);
    }

    fn send_ui_command(&self, command: UiCommand) {
        if self.ui_commands.try_send(command).is_err() {
            debug!("ui is not accepting commands");
        }
    }
}

impl EventSender {
    pub fn try_send(&self, event: Event) {
        if self.events.try_send(event).is_err() {
            debug!("mediator is not accepting events");
        }
    }

    pub async fn send(&self, event: Event) {
        if self.events.send(event).await.is_err() {
            debug!("mediator is no longer accepting events");
        }
    }
}

impl MediatorHandle {
    pub fn events(&self) -> EventSender {
        self.events.clone()
    }

    pub fn take_ui_commands(&mut self) -> mpsc::Receiver<UiCommand> {
        self.ui_commands
            .take()
            .expect("UI commands receiver was already taken")
    }

    pub async fn shutdown(self) {
        self.events.send(Event::Shutdown).await;
        let _ = self.stopped.await;
        self.editor_listener.abort();
    }
}
