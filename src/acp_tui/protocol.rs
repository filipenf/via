//! Host ↔ ACP TUI control-plane messages (newline-delimited JSON).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Logical transcript row kind on the control-plane wire.
///
/// ACP / `UiCommand` chunk names are mapped at the host edge via
/// [`TranscriptKind::from_acp`]; the TUI only sees this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    User,
    Agent,
    Thought,
    Tool,
    System,
}

impl TranscriptKind {
    /// Map mediator / ACP chunk type names onto the wire enum.
    ///
    /// Unknown values become [`TranscriptKind::System`] so a newer host cannot
    /// break an older TUI with an unrecognized kind string.
    pub fn from_acp(raw: &str) -> Self {
        match raw {
            "user" | "user_message_chunk" => Self::User,
            "agent" | "agent_message_chunk" => Self::Agent,
            "thought" | "agent_thought_chunk" => Self::Thought,
            "tool" | "tool_call" | "tool_call_update" => Self::Tool,
            _ => Self::System,
        }
    }
}

/// Messages the host (or a stub pipe) sends into the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToTui {
    Transcript {
        kind: TranscriptKind,
        text: String,
    },
    Progress {
        id: String,
        label: String,
        active: bool,
    },
    SessionStatus {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        provider_error: Option<String>,
        #[serde(default)]
        clear_provider_error: bool,
    },
    Shutdown,
}

/// Messages the TUI emits upstream (socket, or stderr when standalone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiToHost {
    Ready { agent_id: String },
    Submit { text: String },
    ShutdownAck,
}

/// Parse one trimmed JSON line. Empty / comment lines yield `Ok(None)`.
pub fn parse_host_line(line: &str) -> Result<Option<HostToTui>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let msg: HostToTui =
        serde_json::from_str(line).with_context(|| format!("invalid host IPC line: {line}"))?;
    Ok(Some(msg))
}

/// Serialize a host→TUI message to a JSON line (no trailing newline).
pub fn encode_host_line(msg: &HostToTui) -> Result<String> {
    serde_json::to_string(msg).context("encode host IPC line")
}

/// Parse one trimmed TUI→host JSON line. Empty / comment lines yield `Ok(None)`.
pub fn parse_tui_line(line: &str) -> Result<Option<TuiToHost>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let msg: TuiToHost =
        serde_json::from_str(line).with_context(|| format!("invalid TUI IPC line: {line}"))?;
    Ok(Some(msg))
}

/// Serialize a TUI→host message to a JSON line (no trailing newline).
pub fn encode_tui_line(msg: &TuiToHost) -> Result<String> {
    serde_json::to_string(msg).context("encode TUI IPC line")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_transcript_and_skip_blank() {
        assert!(parse_host_line("").unwrap().is_none());
        assert!(parse_host_line("  # comment").unwrap().is_none());
        let msg = parse_host_line(r#"{"type":"transcript","kind":"agent","text":"hello"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            msg,
            HostToTui::Transcript {
                kind: TranscriptKind::Agent,
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn from_acp_maps_chunk_aliases() {
        assert_eq!(
            TranscriptKind::from_acp("agent_message_chunk"),
            TranscriptKind::Agent
        );
        assert_eq!(
            TranscriptKind::from_acp("user_message_chunk"),
            TranscriptKind::User
        );
        assert_eq!(TranscriptKind::from_acp("mystery"), TranscriptKind::System);
    }

    #[test]
    fn parse_progress_and_shutdown() {
        let msg = parse_host_line(r#"{"type":"progress","id":"t1","label":"edit","active":true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            msg,
            HostToTui::Progress {
                id: "t1".into(),
                label: "edit".into(),
                active: true,
            }
        );
        assert_eq!(
            parse_host_line(r#"{"type":"shutdown"}"#).unwrap().unwrap(),
            HostToTui::Shutdown
        );
    }

    #[test]
    fn parse_session_status() {
        let msg = parse_host_line(
            r#"{"type":"session_status","model":"gpt","provider_error":null,"clear_provider_error":true}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            msg,
            HostToTui::SessionStatus {
                model: Some("gpt".into()),
                provider_error: None,
                clear_provider_error: true,
            }
        );
    }

    #[test]
    fn reject_garbage() {
        let err = parse_host_line("not-json").unwrap_err();
        assert!(err.to_string().contains("invalid host IPC line"));
    }

    #[test]
    fn encode_ready_and_submit_roundtrip_shape() {
        let ready = encode_tui_line(&TuiToHost::Ready {
            agent_id: "coder".into(),
        })
        .unwrap();
        assert!(ready.contains(r#""type":"ready""#));
        assert!(ready.contains(r#""agent_id":"coder""#));

        let submit = encode_tui_line(&TuiToHost::Submit { text: "hi".into() }).unwrap();
        let parsed: TuiToHost = serde_json::from_str(&submit).unwrap();
        assert_eq!(parsed, TuiToHost::Submit { text: "hi".into() });
    }
}
