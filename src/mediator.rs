use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::config::Config;
use crate::editor::{self, EditorState};
use crate::event::{EditorEvent, Event, UiEvent};
use crate::nvim::{self, FileTarget};

const EVENT_BUFFER_SIZE: usize = 128;

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
    editor_state: EditorState,
}

#[derive(Clone)]
pub struct EventSender {
    events: mpsc::Sender<Event>,
}

pub struct MediatorHandle {
    events: EventSender,
    stopped: oneshot::Receiver<()>,
    editor_listener: JoinHandle<()>,
}

impl Mediator {
    pub fn new(config: Config) -> Self {
        let (_events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        Self {
            config,
            events: events_rx,
            editor_state: EditorState::default(),
        }
    }

    pub fn spawn(mut self) -> MediatorHandle {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        self.events = events_rx;
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
                    info!("mediator received shutdown");
                    break;
                }
                Event::Ui(UiEvent::OpenRequested { path, line }) => {
                    let target = FileTarget { path, line };

                    if let Err(error) = nvim::open_file(&self.config.nvim_socket_path, target).await
                    {
                        error!(%error, "failed to open file in Neovim");
                    }
                }
                Event::Editor(event) => self.apply_editor_event(event),
                event => debug!(?event, "mediator event received"),
            }
        }
    }

    fn apply_editor_event(&mut self, event: EditorEvent) {
        debug!(?event, "editor context updated");
        self.editor_state.apply(event);
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

    pub async fn shutdown(self) {
        self.events.send(Event::Shutdown).await;
        let _ = self.stopped.await;
        self.editor_listener.abort();
    }
}
