//! Agent delivery: routing a bus message to the right surface.
//!
//! Owns the pending-prompt queue for ACP sub-agents whose handshake hasn't
//! completed yet. The "drain mailbox + pending → deliver" primitive lives in
//! [`crate::acp_runtime`] (it needs the session map + `spawn_acp_prompt_delivery`);
//! the mediator wires the two together by passing this module's pending arc into
//! [`AcpRuntime::deliver_if_ready`].

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;

/// A prompt waiting for an ACP session to finish its handshake.
#[derive(Clone)]
pub struct PendingAcpPrompt {
    pub content: String,
    /// False when the ACP pane already rendered the user message locally.
    pub mirror_on_delivery: bool,
}

/// Pending-prompt queue for ACP delivery. The actual drain+deliver primitive
/// lives in [`crate::acp_runtime`]; this struct only owns the queue.
pub struct AgentDelivery {
    pending: Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>>,
}

impl AgentDelivery {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Arc handle to the pending map — passed into [`AcpRuntime::connect`]
    /// tasks and [`AcpRuntime::deliver_if_ready`] so they can drain queued
    /// prompts once the handshake completes.
    pub fn pending_arc(&self) -> Arc<Mutex<HashMap<String, Vec<PendingAcpPrompt>>>> {
        Arc::clone(&self.pending)
    }

    /// Append a prompt for an ACP recipient whose session isn't ready yet.
    pub async fn queue_prompt(&self, agent_id: &str, content: String, mirror_on_delivery: bool) {
        info!(agent_id, "queuing ACP prompt until session is ready");
        self.pending
            .lock()
            .await
            .entry(agent_id.to_string())
            .or_default()
            .push(PendingAcpPrompt {
                content,
                mirror_on_delivery,
            });
    }

    /// Drop queued prompts for `id` (user picked "discard" in the retry modal,
    /// or the agent was terminated).
    pub async fn discard_queued(&self, agent_id: &str) -> Vec<PendingAcpPrompt> {
        self.pending
            .lock()
            .await
            .remove(agent_id)
            .unwrap_or_default()
    }
}

impl Default for AgentDelivery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_and_discard_prompts() {
        let delivery = AgentDelivery::new();
        delivery
            .queue_prompt("reviewer", "hi".to_string(), false)
            .await;
        delivery
            .queue_prompt("reviewer", "again".to_string(), true)
            .await;
        let discarded = delivery.discard_queued("reviewer").await;
        assert_eq!(discarded.len(), 2);
        assert!(delivery.discard_queued("reviewer").await.is_empty());
    }
}
