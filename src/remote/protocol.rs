//! Host ↔ remote-helper control-plane messages (newline-delimited JSON).
//!
//! PTY payloads are base64 in JSON so framing stays line-oriented. Pane I/O uses
//! Spawn/Attach/Input/Output. ACP agent stdio reuses the same session I/O with
//! [`RemoteRequest::SpawnStdio`] (piped, not PTY). Nvim RPC / ACP-TUI Unix socket
//! mux remains a later frame type.
//!
//! Reconnect: Detach keeps processes; Attach returns [`RemoteEvent::Replay`] for
//! primary-screen PTYs (not nvim / stdio). [`SessionInfo`] carries roster metadata
//! for best-effort local layout restore.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Host → helper requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteRequest {
    ListSessions,
    Spawn {
        session_id: String,
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        cwd: Option<String>,
        cols: u16,
        rows: u16,
        /// When false, Attach skips scrollback Replay (nvim / alt-screen).
        #[serde(default = "default_true")]
        replay_scrollback: bool,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    Attach {
        session_id: String,
    },
    Detach {
        session_id: String,
    },
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    Input {
        session_id: String,
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
    /// Spawn a non-PTY process with piped stdio (ACP agent). Same Input/Output framing.
    SpawnStdio {
        session_id: String,
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Kill a session process (ACP Drop). Detach alone leaves processes alive.
    Kill {
        session_id: String,
    },
    Shutdown,
}

fn default_true() -> bool {
    true
}

/// Helper → host events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteEvent {
    Ready {
        session_id: String,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    Output {
        session_id: String,
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
    Replay {
        session_id: String,
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
    Exit {
        session_id: String,
        #[serde(default)]
        code: Option<i32>,
    },
    Error {
        message: String,
        #[serde(default)]
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Pty,
    Stdio,
}

/// Roster entry for ListSessions / layout restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub alive: bool,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub kind: SessionKind,
    /// When false, client should not expect Replay on Attach (nvim).
    #[serde(default = "default_true")]
    pub replay_scrollback: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Parse one trimmed JSON line. Empty / comment lines yield `Ok(None)`.
pub fn parse_request_line(line: &str) -> Result<Option<RemoteRequest>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let msg: RemoteRequest =
        serde_json::from_str(line).with_context(|| format!("invalid remote request: {line}"))?;
    Ok(Some(msg))
}

/// Serialize a request to a JSON line (no trailing newline).
pub fn encode_request_line(msg: &RemoteRequest) -> Result<String> {
    serde_json::to_string(msg).context("encode remote request")
}

/// Parse one trimmed JSON line. Empty / comment lines yield `Ok(None)`.
pub fn parse_event_line(line: &str) -> Result<Option<RemoteEvent>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let msg: RemoteEvent =
        serde_json::from_str(line).with_context(|| format!("invalid remote event: {line}"))?;
    Ok(Some(msg))
}

/// Serialize an event to a JSON line (no trailing newline).
pub fn encode_event_line(msg: &RemoteEvent) -> Result<String> {
    serde_json::to_string(msg).context("encode remote event")
}

mod b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        decode(&s).map_err(serde::de::Error::custom)
    }

    pub(super) fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    pub(super) fn decode(input: &str) -> Result<Vec<u8>, String> {
        let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if clean.len() % 4 != 0 {
            return Err("invalid base64 length".into());
        }
        let mut out = Vec::with_capacity(clean.len() / 4 * 3);
        for chunk in clean.chunks(4) {
            let mut n = 0u32;
            let mut pads = 0;
            for (i, b) in chunk.iter().enumerate() {
                let v = match *b {
                    b'A'..=b'Z' => b - b'A',
                    b'a'..=b'z' => b - b'a' + 26,
                    b'0'..=b'9' => b - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    b'=' => {
                        pads += 1;
                        0
                    }
                    other => return Err(format!("invalid base64 byte {other}")),
                };
                if *b != b'=' && pads > 0 {
                    return Err("pad in the middle of base64".into());
                }
                if i < 4 - pads {
                    n |= (v as u32) << (18 - 6 * i);
                }
            }
            out.push((n >> 16) as u8);
            if pads < 2 {
                out.push((n >> 8) as u8);
            }
            if pads < 1 {
                out.push(n as u8);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_spawn_and_input() {
        let spawn = RemoteRequest::Spawn {
            session_id: "agent".into(),
            argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
            env: vec![("FOO".into(), "bar".into())],
            cwd: Some("/tmp".into()),
            cols: 80,
            rows: 24,
            replay_scrollback: true,
            role: Some("agent".into()),
            label: Some("agent".into()),
        };
        let line = encode_request_line(&spawn).unwrap();
        assert_eq!(parse_request_line(&line).unwrap().unwrap(), spawn);

        let input = RemoteRequest::Input {
            session_id: "agent".into(),
            bytes: b"hello\n".to_vec(),
        };
        let line = encode_request_line(&input).unwrap();
        let parsed = parse_request_line(&line).unwrap().unwrap();
        assert_eq!(parsed, input);
        assert!(line.contains("aGVsbG8K"), "expected base64 payload: {line}");
    }

    #[test]
    fn spawn_defaults_replay_true_when_omitted() {
        let line = r#"{"type":"spawn","session_id":"x","argv":["true"],"cols":80,"rows":24}"#;
        match parse_request_line(line).unwrap().unwrap() {
            RemoteRequest::Spawn {
                replay_scrollback, ..
            } => assert!(replay_scrollback),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn event_roundtrip_output_replay_exit() {
        for event in [
            RemoteEvent::Ready {
                session_id: "nvim".into(),
            },
            RemoteEvent::Output {
                session_id: "nvim".into(),
                bytes: b"\x1b[31mred".to_vec(),
            },
            RemoteEvent::Replay {
                session_id: "nvim".into(),
                bytes: b"scrollback".to_vec(),
            },
            RemoteEvent::Exit {
                session_id: "nvim".into(),
                code: Some(0),
            },
            RemoteEvent::SessionList {
                sessions: vec![SessionInfo {
                    session_id: "nvim".into(),
                    alive: true,
                    cols: 120,
                    rows: 40,
                    kind: SessionKind::Pty,
                    replay_scrollback: false,
                    role: Some("editor".into()),
                    label: Some("nvim".into()),
                }],
            },
            RemoteEvent::Error {
                message: "nope".into(),
                session_id: Some("x".into()),
            },
        ] {
            let line = encode_event_line(&event).unwrap();
            assert_eq!(parse_event_line(&line).unwrap().unwrap(), event);
        }
    }

    #[test]
    fn skips_blank_and_comment_lines() {
        assert!(parse_request_line("").unwrap().is_none());
        assert!(parse_request_line("  # wait").unwrap().is_none());
        assert!(parse_event_line("\n").unwrap().is_none());
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse_request_line(r#"{"type":"nope"}"#).is_err());
        assert!(parse_event_line(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn b64_roundtrip_empty_and_binary() {
        assert_eq!(b64::decode(&b64::encode(b"")).unwrap(), b"");
        assert_eq!(b64::decode(&b64::encode(b"a")).unwrap(), b"a");
        assert_eq!(b64::decode(&b64::encode(b"ab")).unwrap(), b"ab");
        assert_eq!(b64::decode(&b64::encode(b"abc")).unwrap(), b"abc");
        let bin: Vec<u8> = (0..=255).collect();
        assert_eq!(b64::decode(&b64::encode(&bin)).unwrap(), bin);
    }
}
