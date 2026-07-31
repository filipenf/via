//! Mock ACP agent for manual UX testing of via's permission modal queue.
//!
//! **What:** A typed ACP agent on stdio (official `agent-client-protocol` SDK) that
//! fires controllable `session/request_permission` bursts or drip sequences with full
//! fake tool-call lifecycles.
//!
//! **Why:** Exercises the FIFO modal queue (`+N pending`, Tab/Shift+Tab, FIFO drain)
//! without a real ACP backend. Replaces the throwaway shell mock.
//!
//! **Build:** `cargo build --example mock_acp_agent`
//!
//! **Spawn in via:** `via agent spawn --id mock1 --command "$(pwd)/target/debug/examples/mock_acp_agent acp"`
//! The trailing `acp` positional marks the command as ACP (via checks the last token).
//! After handshake, type commands in the ACP pane (or `via agent send --to mock1 -m burst`).
//!
//! **Usage:** Interactive commands `1`/`burst [N] [delay_ms]`, `2`/`drip [N] [delay_ms]`,
//! `help`. Burst vs drip is chosen per prompt. clap flags `--requests`, `--delay-ms`
//! supply defaults when omitted. `session/prompt` stays pending until the scenario finishes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, MessageId, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionId, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, JsonRpcRequest, Responder, SentRequest};
use clap::Parser;
use tokio::sync::watch;
use tokio::time::sleep;

const SESSION_ID: &str = "sess_mock";
const FIRST_TOOL_CALL_NUM: u64 = 1;

const MENU_TEXT: &str = "mock-acp-agent — permission modal queue tester

Commands (type here or via agent send):
  1, burst [N] [delay_ms]  — fire N modals at once (concurrent permission requests)
  2, drip [N] [delay_ms]   — one modal; next after each answer completes
  help                     — show this menu

Defaults: N and delay_ms from --requests / --delay-ms flags.
Burst vs drip is selected per command, not via a startup flag.
Each resolution is echoed in the transcript; fake commands never execute.";

#[derive(Debug, Parser)]
#[command(
    name = "mock_acp_agent",
    about = "Mock ACP agent for via modal queue UX testing"
)]
struct Args {
    #[arg(long, default_value_t = 3)]
    requests: u32,
    #[arg(long, default_value_t = 0)]
    delay_ms: u64,
    /// Hidden trailing marker so via classifies this binary as an ACP agent (`… mock_acp_agent acp`).
    #[arg(hide = true, value_parser = parse_acp_marker)]
    acp: Option<String>,
}

#[derive(Debug, Clone)]
struct Defaults {
    requests: u32,
    delay_ms: u64,
}

#[derive(Debug)]
struct DripState;

#[derive(Debug)]
struct SessionState {
    active_prompts: u64,
    cancelled: bool,
    cancel_signal: watch::Sender<bool>,
    drip: Option<DripState>,
    active_tool_calls: HashSet<String>,
}

#[derive(Debug)]
struct InnerState {
    sessions: HashMap<SessionId, SessionState>,
    next_tool_call_num: u64,
    next_message_num: u64,
}

#[derive(Clone)]
struct MockAgent {
    defaults: Defaults,
    state: Arc<Mutex<InnerState>>,
}

#[derive(Debug, PartialEq, Eq)]
enum PromptCommand {
    Burst { count: u32, delay_ms: u64 },
    Drip { count: u32, delay_ms: u64 },
    Help,
    Error(String),
}

impl MockAgent {
    fn new(args: Args) -> Self {
        Self {
            defaults: Defaults {
                requests: args.requests,
                delay_ms: args.delay_ms,
            },
            state: Arc::new(Mutex::new(InnerState {
                sessions: HashMap::new(),
                next_tool_call_num: FIRST_TOOL_CALL_NUM,
                next_message_num: 1,
            })),
        }
    }

    async fn run(self) -> agent_client_protocol::Result<()> {
        Agent
            .builder()
            .name("mock-acp-agent")
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |initialize: InitializeRequest, responder, _connection| {
                        agent.handle_initialize(initialize, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: NewSessionRequest, responder, connection| {
                        agent.handle_new_session(request, responder, connection)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: PromptRequest, responder, connection| {
                        let session_id = request.session_id.clone();
                        if let Err(error) = agent.begin_prompt(&session_id) {
                            return responder.respond_with_error(error);
                        }
                        let connection_for_task = connection.clone();
                        let spawn_result = connection.spawn({
                            let agent = agent.clone();
                            async move {
                                agent
                                    .process_prompt(request, responder, connection_for_task)
                                    .await
                            }
                        });
                        if spawn_result.is_err() {
                            agent.finish_prompt(&session_id, StopReason::EndTurn);
                        }
                        spawn_result
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let agent = self;
                    async move |notification: CancelNotification, connection| {
                        agent.handle_cancel(notification, connection)
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(agent_client_protocol::Stdio::new())
            .await
    }

    fn handle_initialize(
        &self,
        initialize: InitializeRequest,
        responder: Responder<InitializeResponse>,
    ) -> agent_client_protocol::Result<()> {
        responder.respond(
            InitializeResponse::new(initialize.protocol_version)
                .agent_capabilities(AgentCapabilities::new())
                .agent_info(Implementation::new("mock-acp-agent", "0.2")),
        )
    }

    fn handle_new_session(
        &self,
        _request: NewSessionRequest,
        responder: Responder<NewSessionResponse>,
        connection: ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<()> {
        let session_id = SessionId::from(SESSION_ID);
        let (cancel_signal, _) = watch::channel(false);
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.sessions.insert(
                session_id.clone(),
                SessionState {
                    active_prompts: 0,
                    cancelled: false,
                    cancel_signal,
                    drip: None,
                    active_tool_calls: HashSet::new(),
                },
            );
        }
        responder.respond(NewSessionResponse::new(session_id.clone()))?;
        self.send_agent_message(&connection, &session_id, MENU_TEXT)
    }

    fn handle_cancel(
        &self,
        notification: CancelNotification,
        connection: ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<()> {
        self.mark_cancelled(&notification.session_id);
        self.cancel_active_tool_calls(&connection, &notification.session_id);
        Ok(())
    }

    fn begin_prompt(&self, session_id: &SessionId) -> Result<(), agent_client_protocol::Error> {
        let mut state = self.state.lock().expect("mock state lock");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| invalid_session(session_id))?;
        session.active_prompts += 1;
        Ok(())
    }

    fn finish_prompt(&self, session_id: &SessionId, stop_reason: StopReason) -> StopReason {
        let mut state = self.state.lock().expect("mock state lock");
        let Some(session) = state.sessions.get_mut(session_id) else {
            return StopReason::Cancelled;
        };
        let cancelled = session.cancelled;
        session.active_prompts = session.active_prompts.saturating_sub(1);
        if cancelled && session.active_prompts == 0 {
            session.cancelled = false;
            session
                .cancel_signal
                .send_modify(|cancelled| *cancelled = false);
        }
        if cancelled {
            StopReason::Cancelled
        } else {
            stop_reason
        }
    }

    fn mark_cancelled(&self, session_id: &SessionId) {
        let signal = {
            let mut state = self.state.lock().expect("mock state lock");
            let Some(session) = state.sessions.get_mut(session_id) else {
                return;
            };
            if session.active_prompts == 0 {
                return;
            }
            session.cancelled = true;
            session.cancel_signal.clone()
        };
        signal.send_modify(|cancelled| *cancelled = true);
    }

    fn is_cancelled(&self, session_id: &SessionId) -> bool {
        self.state
            .lock()
            .expect("mock state lock")
            .sessions
            .get(session_id)
            .is_some_and(|session| session.cancelled)
    }

    async fn wait_for_cancelled(&self, session_id: &SessionId) {
        let Some(mut signal) = self
            .state
            .lock()
            .expect("mock state lock")
            .sessions
            .get(session_id)
            .map(|session| session.cancel_signal.subscribe())
        else {
            return;
        };
        loop {
            if *signal.borrow() {
                return;
            }
            if signal.changed().await.is_err() {
                return;
            }
        }
    }

    fn alloc_tool_call_id(&self) -> String {
        let mut state = self.state.lock().expect("mock state lock");
        let id = format!("call_{}", state.next_tool_call_num);
        state.next_tool_call_num += 1;
        id
    }

    fn alloc_message_id(&self) -> String {
        let mut state = self.state.lock().expect("mock state lock");
        let id = format!("msg_{}", state.next_message_num);
        state.next_message_num += 1;
        id
    }

    fn track_tool_call(&self, session_id: &SessionId, tool_call_id: &str) {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.active_tool_calls.insert(tool_call_id.to_string());
        }
    }

    fn untrack_tool_call(&self, session_id: &SessionId, tool_call_id: &str) {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.active_tool_calls.remove(tool_call_id);
        }
    }

    fn cancel_active_tool_calls(&self, connection: &ConnectionTo<Client>, session_id: &SessionId) {
        let tool_call_ids: Vec<String> = {
            let mut state = self.state.lock().expect("mock state lock");
            let Some(session) = state.sessions.get_mut(session_id) else {
                return;
            };
            session.active_tool_calls.drain().collect()
        };
        for tool_call_id in tool_call_ids {
            let update = ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![ToolCallContent::from("cancelled by session/cancel")]),
            );
            let _ = send_session_update(
                connection,
                session_id,
                SessionUpdate::ToolCallUpdate(update),
            );
        }
    }

    async fn request_until_cancelled<Req>(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        request: Req,
    ) -> Result<Req::Response, agent_client_protocol::Error>
    where
        Req: JsonRpcRequest,
        Req::Response: Send,
    {
        let sent: SentRequest<Req::Response> = connection.send_request(request);
        tokio::select! {
            result = sent.block_task() => result,
            () = self.wait_for_cancelled(session_id) => {
                Err(agent_client_protocol::Error::new(-32800, "Request was cancelled"))
            }
        }
    }

    async fn process_prompt(
        &self,
        request: PromptRequest,
        responder: Responder<PromptResponse>,
        connection: ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<()> {
        let session_id = request.session_id.clone();
        let text = extract_text_from_prompt(&request.prompt);

        let stop_reason = match parse_prompt_command(&text, &self.defaults) {
            PromptCommand::Help => {
                self.send_agent_message(&connection, &session_id, MENU_TEXT)?;
                StopReason::EndTurn
            }
            PromptCommand::Error(message) => {
                self.send_agent_message(&connection, &session_id, &message)?;
                StopReason::EndTurn
            }
            PromptCommand::Burst { count, delay_ms } => {
                if let Err(error) = self
                    .run_burst(&session_id, &connection, count, delay_ms)
                    .await
                {
                    return responder.respond_with_error(error);
                }
                StopReason::EndTurn
            }
            PromptCommand::Drip { count, delay_ms } => {
                if self.drip_active(&session_id) {
                    self.send_agent_message(
                        &connection,
                        &session_id,
                        "mock: drip already active — finish or resolve the current sequence before starting another",
                    )?;
                    StopReason::EndTurn
                } else if let Err(error) = self
                    .run_drip(&session_id, &connection, count, delay_ms)
                    .await
                {
                    return responder.respond_with_error(error);
                } else {
                    StopReason::EndTurn
                }
            }
        };

        let stop_reason = self.finish_prompt(&session_id, stop_reason);
        responder.respond(PromptResponse::new(stop_reason))
    }

    fn drip_active(&self, session_id: &SessionId) -> bool {
        self.state
            .lock()
            .expect("mock state lock")
            .sessions
            .get(session_id)
            .is_some_and(|session| session.drip.is_some())
    }

    fn start_drip(&self, session_id: &SessionId) {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.drip = Some(DripState);
        }
    }

    fn finish_drip(&self, session_id: &SessionId) {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.drip = None;
        }
    }

    async fn run_burst(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        count: u32,
        delay_ms: u64,
    ) -> agent_client_protocol::Result<()> {
        let mut tasks = Vec::with_capacity(count as usize);
        for index in 1..=count {
            if index > 1 && delay_ms > 0 {
                sleep(Duration::from_millis(delay_ms)).await;
            }
            if self.is_cancelled(session_id) {
                break;
            }
            let agent = self.clone();
            let session_id = session_id.clone();
            let connection = connection.clone();
            tasks.push(async move {
                agent
                    .run_fake_command(&session_id, &connection, index, count)
                    .await
            });
        }
        let mut set = tokio::task::JoinSet::new();
        for task in tasks {
            set.spawn(task);
        }
        while let Some(result) = set.join_next().await {
            result.map_err(agent_client_protocol::Error::into_internal_error)??;
        }
        Ok(())
    }

    async fn run_drip(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        count: u32,
        delay_ms: u64,
    ) -> agent_client_protocol::Result<()> {
        self.start_drip(session_id);
        for index in 1..=count {
            if self.is_cancelled(session_id) {
                break;
            }
            if index > 1 && delay_ms > 0 {
                sleep(Duration::from_millis(delay_ms)).await;
            }
            self.run_fake_command(session_id, connection, index, count)
                .await?;
        }
        self.finish_drip(session_id);
        Ok(())
    }

    async fn run_fake_command(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        index: u32,
        total: u32,
    ) -> agent_client_protocol::Result<()> {
        let tool_call_id = self.alloc_tool_call_id();
        let command = format!("demo-command-{index} --not-auto-approved");
        let title = format!("Mock permission {index} of {total}");

        let initial = ToolCall::new(tool_call_id.clone(), title.clone())
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({ "command": command, "cwd": "/tmp" }));
        send_session_update(connection, session_id, SessionUpdate::ToolCall(initial))?;
        self.track_tool_call(session_id, &tool_call_id);

        let permission = RequestPermissionRequest::new(
            session_id.clone(),
            ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .title(title)
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending),
            ),
            permission_options(),
        );

        let response = match self
            .request_until_cancelled(session_id, connection, permission)
            .await
        {
            Ok(response) => response,
            Err(_) => {
                self.emit_cancelled_tool_call(connection, session_id, &tool_call_id)?;
                self.untrack_tool_call(session_id, &tool_call_id);
                return Ok(());
            }
        };

        self.handle_permission_outcome(connection, session_id, &tool_call_id, &response)
            .await?;
        self.untrack_tool_call(session_id, &tool_call_id);
        Ok(())
    }

    async fn handle_permission_outcome(
        &self,
        connection: &ConnectionTo<Client>,
        session_id: &SessionId,
        tool_call_id: &str,
        response: &RequestPermissionResponse,
    ) -> agent_client_protocol::Result<()> {
        let label = outcome_label(&response.outcome);
        self.send_agent_message(
            connection,
            session_id,
            &format!("mock: request {tool_call_id} resolved -> {label}"),
        )?;

        match &response.outcome {
            RequestPermissionOutcome::Selected(selected)
                if is_allow_option(option_id_str(&selected.option_id)) =>
            {
                let in_progress = ToolCallUpdate::new(
                    tool_call_id.to_string(),
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::InProgress)
                        .content(vec![ToolCallContent::from(format!(
                            "mock running `{tool_call_id}` (not executed)"
                        ))]),
                );
                send_session_update(
                    connection,
                    session_id,
                    SessionUpdate::ToolCallUpdate(in_progress),
                )?;

                let completed = ToolCallUpdate::new(
                    tool_call_id.to_string(),
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Completed)
                        .content(vec![ToolCallContent::from(format!(
                            "mock output for `{tool_call_id}`"
                        ))])
                        .raw_output(
                            serde_json::json!({ "mock": true, "toolCallId": tool_call_id }),
                        ),
                );
                send_session_update(
                    connection,
                    session_id,
                    SessionUpdate::ToolCallUpdate(completed),
                )?;
            }
            RequestPermissionOutcome::Selected(selected)
                if is_reject_option(option_id_str(&selected.option_id)) =>
            {
                self.emit_failed_tool_call(
                    connection,
                    session_id,
                    tool_call_id,
                    &format!("rejected ({})", option_id_str(&selected.option_id)),
                )?;
            }
            RequestPermissionOutcome::Selected(selected) => {
                self.emit_failed_tool_call(
                    connection,
                    session_id,
                    tool_call_id,
                    &format!("unknown option {}", option_id_str(&selected.option_id)),
                )?;
            }
            RequestPermissionOutcome::Cancelled => {
                self.emit_cancelled_tool_call(connection, session_id, tool_call_id)?;
            }
            _ => {
                self.emit_failed_tool_call(
                    connection,
                    session_id,
                    tool_call_id,
                    "unknown outcome",
                )?;
            }
        }
        Ok(())
    }

    fn emit_cancelled_tool_call(
        &self,
        connection: &ConnectionTo<Client>,
        session_id: &SessionId,
        tool_call_id: &str,
    ) -> agent_client_protocol::Result<()> {
        self.emit_failed_tool_call(connection, session_id, tool_call_id, "cancelled")
    }

    fn emit_failed_tool_call(
        &self,
        connection: &ConnectionTo<Client>,
        session_id: &SessionId,
        tool_call_id: &str,
        reason: &str,
    ) -> agent_client_protocol::Result<()> {
        let update = ToolCallUpdate::new(
            tool_call_id.to_string(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .content(vec![ToolCallContent::from(reason.to_string())]),
        );
        send_session_update(
            connection,
            session_id,
            SessionUpdate::ToolCallUpdate(update),
        )
    }

    fn send_agent_message(
        &self,
        connection: &ConnectionTo<Client>,
        session_id: &SessionId,
        text: &str,
    ) -> agent_client_protocol::Result<()> {
        let message_id = MessageId::new(self.alloc_message_id());
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text.to_string().into()).message_id(message_id),
            ),
        )
    }
}

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow-always",
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ]
}

fn send_session_update(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> agent_client_protocol::Result<()> {
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

fn extract_text_from_prompt(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_prompt_command(text: &str, defaults: &Defaults) -> PromptCommand {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return PromptCommand::Error(format!(
            "mock: empty command — type help or see menu (defaults: burst {} requests, {}ms delay)",
            defaults.requests, defaults.delay_ms
        ));
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == "help" || lower == "?" {
        return PromptCommand::Help;
    }

    let mut parts = trimmed.split_whitespace();
    let head = parts.next().unwrap_or("");
    let head_lower = head.to_ascii_lowercase();

    let (kind, mut rest) = match head_lower.as_str() {
        "1" | "burst" => ("burst", parts),
        "2" | "drip" => ("drip", parts),
        other => {
            return PromptCommand::Error(format!(
                "mock: unknown command '{other}' — type help for burst/drip usage"
            ));
        }
    };

    let count = match rest.next() {
        None => defaults.requests,
        Some(raw) => match raw.parse::<u32>() {
            Ok(0) => {
                return PromptCommand::Error("mock: count must be at least 1".into());
            }
            Ok(n) => n,
            Err(_) => {
                return PromptCommand::Error(format!(
                    "mock: invalid count '{raw}' — expected a positive integer"
                ));
            }
        },
    };

    let delay_ms = match rest.next() {
        None => defaults.delay_ms,
        Some(raw) => match raw.parse::<u64>() {
            Ok(ms) => ms,
            Err(_) => {
                return PromptCommand::Error(format!(
                    "mock: invalid delay_ms '{raw}' — expected a non-negative integer"
                ));
            }
        },
    };

    if rest.next().is_some() {
        return PromptCommand::Error(format!(
            "mock: too many arguments for {kind} — usage: {kind} [N] [delay_ms]"
        ));
    }

    match kind {
        "burst" => PromptCommand::Burst { count, delay_ms },
        "drip" => PromptCommand::Drip { count, delay_ms },
        _ => unreachable!(),
    }
}

fn outcome_label(outcome: &RequestPermissionOutcome) -> String {
    match outcome {
        RequestPermissionOutcome::Cancelled => "cancelled".to_string(),
        RequestPermissionOutcome::Selected(selected) => {
            option_id_str(&selected.option_id).to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn option_id_str(option_id: &PermissionOptionId) -> &str {
    option_id.0.as_ref()
}

fn is_allow_option(option_id: &str) -> bool {
    option_id.contains("allow")
}

fn is_reject_option(option_id: &str) -> bool {
    option_id.contains("reject")
}

fn invalid_session(session_id: &SessionId) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(format!("unknown session `{session_id}`"))
}

fn parse_acp_marker(value: &str) -> Result<String, String> {
    if value == "acp" {
        Ok(value.to_string())
    } else {
        Err("expected the trailing ACP marker `acp`".to_string())
    }
}

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    let args = Args::parse();
    eprintln!(
        "mock_acp_agent: defaults requests={} delay_ms={}",
        args.requests, args.delay_ms
    );
    MockAgent::new(args).run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SelectedPermissionOutcome;

    /// Mirrors [`via::config::is_acp_command`] — last token must be `acp`.
    fn is_acp_command(command: &str) -> bool {
        command.split_whitespace().last() == Some("acp")
    }

    fn spawn_command(binary: &str) -> String {
        format!("{binary} acp")
    }

    fn defaults() -> Defaults {
        Defaults {
            requests: 3,
            delay_ms: 0,
        }
    }

    #[test]
    fn parse_prompt_command_variants() {
        let defaults = defaults();
        assert_eq!(
            parse_prompt_command("burst", &defaults),
            PromptCommand::Burst {
                count: 3,
                delay_ms: 0
            }
        );
        assert_eq!(
            parse_prompt_command("burst 5 100", &defaults),
            PromptCommand::Burst {
                count: 5,
                delay_ms: 100
            }
        );
        assert_eq!(
            parse_prompt_command("drip 2", &defaults),
            PromptCommand::Drip {
                count: 2,
                delay_ms: 0
            }
        );
        assert_eq!(parse_prompt_command("help", &defaults), PromptCommand::Help);
        assert!(matches!(
            parse_prompt_command("go", &defaults),
            PromptCommand::Error(_)
        ));
    }

    #[test]
    fn monotonic_tool_call_ids_across_allocations() {
        let agent = MockAgent::new(Args {
            requests: 3,
            delay_ms: 0,
            acp: None,
        });
        assert_eq!(agent.alloc_tool_call_id(), "call_1");
        assert_eq!(agent.alloc_tool_call_id(), "call_2");
        assert_eq!(agent.alloc_tool_call_id(), "call_3");
    }

    #[test]
    fn spawn_command_classifies_as_acp_for_via() {
        let cmd = spawn_command("/tmp/target/debug/examples/mock_acp_agent");
        assert!(is_acp_command(&cmd));
        assert!(!is_acp_command("/tmp/target/debug/examples/mock_acp_agent"));
        assert!(!is_acp_command("opencode"));
        assert!(is_acp_command("opencode acp"));
    }

    #[test]
    fn acp_marker_rejects_other_values() {
        assert_eq!(parse_acp_marker("acp"), Ok("acp".to_string()));
        assert!(parse_acp_marker("pty").is_err());
    }

    #[test]
    fn outcome_label_maps_selected_and_cancelled() {
        assert_eq!(
            outcome_label(&RequestPermissionOutcome::Cancelled),
            "cancelled"
        );
        assert_eq!(
            outcome_label(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("allow-once")
            )),
            "allow-once"
        );
    }

    #[test]
    fn permission_options_include_allow_and_reject() {
        let options = permission_options();
        assert_eq!(options.len(), 3);
        assert_eq!(option_id_str(&options[0].option_id), "allow-once");
        assert_eq!(option_id_str(&options[2].option_id), "reject");
    }

    #[test]
    fn burst_plan_uses_defaults_when_args_omitted() {
        let cmd = parse_prompt_command("1", &defaults());
        assert_eq!(
            cmd,
            PromptCommand::Burst {
                count: 3,
                delay_ms: 0
            }
        );
    }

    #[test]
    fn reject_second_drip_is_detected_via_session_state() {
        let agent = MockAgent::new(Args {
            requests: 3,
            delay_ms: 0,
            acp: None,
        });
        let session_id = SessionId::from(SESSION_ID);
        agent.state.lock().unwrap().sessions.insert(
            session_id.clone(),
            SessionState {
                active_prompts: 0,
                cancelled: false,
                cancel_signal: watch::channel(false).0,
                drip: Some(DripState),
                active_tool_calls: HashSet::new(),
            },
        );
        assert!(agent.drip_active(&session_id));
    }

    #[tokio::test]
    async fn wait_for_cancelled_observes_cancel_before_waiter_starts() {
        let agent = MockAgent::new(Args {
            requests: 3,
            delay_ms: 0,
            acp: None,
        });
        let session_id = SessionId::from(SESSION_ID);
        agent.state.lock().unwrap().sessions.insert(
            session_id.clone(),
            SessionState {
                active_prompts: 1,
                cancelled: false,
                cancel_signal: watch::channel(false).0,
                drip: None,
                active_tool_calls: HashSet::new(),
            },
        );

        agent.mark_cancelled(&session_id);
        tokio::time::timeout(
            Duration::from_secs(1),
            agent.wait_for_cancelled(&session_id),
        )
        .await
        .expect("cancellation signal should be retained for late waiters");
    }

    #[test]
    fn finish_prompt_returns_cancelled_while_session_marked_cancelled() {
        let agent = MockAgent::new(Args {
            requests: 3,
            delay_ms: 0,
            acp: None,
        });
        let session_id = SessionId::from(SESSION_ID);
        agent.state.lock().unwrap().sessions.insert(
            session_id.clone(),
            SessionState {
                active_prompts: 1,
                cancelled: true,
                cancel_signal: watch::channel(true).0,
                drip: None,
                active_tool_calls: HashSet::new(),
            },
        );
        assert_eq!(
            agent.finish_prompt(&session_id, StopReason::EndTurn),
            StopReason::Cancelled
        );
    }
}
