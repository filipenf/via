use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

use crate::config::Config;
use crate::event::Event;

const EVENT_BUFFER_SIZE: usize = 128;

pub struct Mediator {
    config: Config,
    events: mpsc::Receiver<Event>,
}

#[derive(Clone)]
pub struct EventSender {
    events: mpsc::Sender<Event>,
}

pub struct MediatorHandle {
    events: EventSender,
    stopped: oneshot::Receiver<()>,
}

impl Mediator {
    pub fn new(config: Config) -> Self {
        let (_events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        Self {
            config,
            events: events_rx,
        }
    }

    pub fn spawn(mut self) -> MediatorHandle {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        self.events = events_rx;

        let (stopped_tx, stopped_rx) = oneshot::channel();
        tokio::spawn(async move {
            self.run().await;
            let _ = stopped_tx.send(());
        });

        MediatorHandle {
            events: EventSender { events: events_tx },
            stopped: stopped_rx,
        }
    }

    async fn run(&mut self) {
        info!(
            nvim_command = %self.config.nvim_command,
            nvim_socket = %self.config.nvim_socket_path.display(),
            agent_configured = self.config.agent_command.is_some(),
            "mediator ready"
        );

        while let Some(event) = self.events.recv().await {
            if matches!(event, Event::Shutdown) {
                info!("mediator received shutdown");
                break;
            }

            debug!(?event, "mediator event received");
        }
    }
}

impl EventSender {
    pub async fn send(&self, event: Event) {
        if self.events.send(event).await.is_err() {
            debug!("mediator is no longer accepting events");
        }
    }
}

impl MediatorHandle {
    #[allow(dead_code)]
    pub fn events(&self) -> EventSender {
        self.events.clone()
    }

    pub async fn shutdown(self) {
        self.events.send(Event::Shutdown).await;
        let _ = self.stopped.await;
    }
}
