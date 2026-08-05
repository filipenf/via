//! Host ↔ remote-helper control-plane messages (length-prefixed CBOR).
//!
//! Framing: `[u32 BE length][cbor payload]`. `Input` / `Output` / `Replay` carry
//! native CBOR byte strings (no base64). Pane I/O uses Spawn/Attach/Input/Output.
//! ACP agent stdio reuses the same session I/O with [`RemoteRequest::SpawnStdio`]
//! (piped, not PTY). Nvim RPC / ACP-TUI Unix socket mux remains a later frame type.
//!
//! Reconnect: Detach keeps processes; Attach returns [`RemoteEvent::Replay`] for
//! primary-screen PTYs (not nvim / stdio). [`SessionInfo`] carries roster metadata
//! for best-effort local layout restore.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Reject frames larger than this (DoS guard on the length prefix).
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

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
        #[serde(with = "serde_bytes")]
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
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Replay {
        session_id: String,
        #[serde(with = "serde_bytes")]
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

/// Encode `msg` as CBOR and write `[u32 BE len][cbor]`.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> Result<()> {
    let mut payload = Vec::new();
    ciborium::into_writer(msg, &mut payload).context("encode remote CBOR frame")?;
    let len = u32::try_from(payload.len()).context("remote frame too large")?;
    if len > MAX_FRAME_LEN {
        bail!("remote frame length {len} exceeds max {MAX_FRAME_LEN}");
    }
    writer
        .write_all(&len.to_be_bytes())
        .context("write remote frame length")?;
    writer
        .write_all(&payload)
        .context("write remote frame payload")?;
    Ok(())
}

/// Read one length-prefixed CBOR frame. Returns `Ok(None)` on clean EOF before
/// any length bytes. Partial frames after a length prefix are errors.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_eof(reader, &mut len_buf)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        bail!("remote frame length {len} exceeds max {MAX_FRAME_LEN}");
    }
    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .context("read remote frame payload")?;
    let msg = ciborium::from_reader(payload.as_slice()).context("decode remote CBOR frame")?;
    Ok(Some(msg))
}

/// Try to peel one complete frame from the front of `buf`. Returns `Ok(None)` if
/// more bytes are needed. On success, drains the frame bytes from `buf`.
pub fn try_read_frame_from_buf<T: DeserializeOwned>(buf: &mut Vec<u8>) -> Result<Option<T>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_LEN {
        bail!("remote frame length {len} exceeds max {MAX_FRAME_LEN}");
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let payload = buf[4..total].to_vec();
    buf.drain(..total);
    let msg = ciborium::from_reader(payload.as_slice()).context("decode remote CBOR frame")?;
    Ok(Some(msg))
}

fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => bail!("unexpected EOF while reading remote frame header"),
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("read remote frame header"),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        let mut buf = Vec::new();
        write_frame(&mut buf, &spawn).unwrap();
        let parsed: RemoteRequest = read_frame(&mut Cursor::new(&buf)).unwrap().unwrap();
        assert_eq!(parsed, spawn);

        let input = RemoteRequest::Input {
            session_id: "agent".into(),
            bytes: b"hello\n".to_vec(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &input).unwrap();
        // Native byte string — payload must contain raw bytes, not base64.
        assert!(
            buf.windows(6).any(|w| w == b"hello\n"),
            "expected raw bytes in frame, got {buf:?}"
        );
        assert!(
            !buf.windows(8).any(|w| w == b"aGVsbG8K"),
            "must not base64-encode Input bytes"
        );
        let parsed: RemoteRequest = read_frame(&mut Cursor::new(&buf)).unwrap().unwrap();
        assert_eq!(parsed, input);
    }

    #[test]
    fn spawn_defaults_replay_true_when_omitted() {
        // Manually build a CBOR map without `replay_scrollback`.
        let value = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("type".into()),
                ciborium::Value::Text("spawn".into()),
            ),
            (
                ciborium::Value::Text("session_id".into()),
                ciborium::Value::Text("x".into()),
            ),
            (
                ciborium::Value::Text("argv".into()),
                ciborium::Value::Array(vec![ciborium::Value::Text("true".into())]),
            ),
            (
                ciborium::Value::Text("cols".into()),
                ciborium::Value::Integer(80.into()),
            ),
            (
                ciborium::Value::Text("rows".into()),
                ciborium::Value::Integer(24.into()),
            ),
        ]);
        let mut payload = Vec::new();
        ciborium::into_writer(&value, &mut payload).unwrap();
        let req: RemoteRequest = ciborium::from_reader(payload.as_slice()).unwrap();
        match req {
            RemoteRequest::Spawn {
                replay_scrollback, ..
            } => assert!(replay_scrollback),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn event_roundtrip_output_replay_exit_with_csi() {
        let csi = b"\x1b[31mred\x00\xff".to_vec();
        for event in [
            RemoteEvent::Ready {
                session_id: "nvim".into(),
            },
            RemoteEvent::Output {
                session_id: "nvim".into(),
                bytes: csi.clone(),
            },
            RemoteEvent::Replay {
                session_id: "nvim".into(),
                bytes: csi.clone(),
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
            let mut buf = Vec::new();
            write_frame(&mut buf, &event).unwrap();
            let parsed: RemoteEvent = read_frame(&mut Cursor::new(&buf)).unwrap().unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn read_frame_eof_before_header() {
        let mut empty: &[u8] = &[];
        let got: Option<RemoteRequest> = read_frame(&mut empty).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn try_read_frame_from_buf_needs_more() {
        let mut buf = vec![0, 0, 0, 10]; // length 10, no payload yet
        let got: Option<RemoteRequest> = try_read_frame_from_buf(&mut buf).unwrap();
        assert!(got.is_none());
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn rejects_unknown_type() {
        let value = ciborium::Value::Map(vec![(
            ciborium::Value::Text("type".into()),
            ciborium::Value::Text("nope".into()),
        )]);
        let mut payload = Vec::new();
        ciborium::into_writer(&value, &mut payload).unwrap();
        assert!(ciborium::from_reader::<RemoteRequest, _>(payload.as_slice()).is_err());
        assert!(ciborium::from_reader::<RemoteEvent, _>(payload.as_slice()).is_err());
    }

    #[test]
    fn binary_payload_roundtrip_all_bytes() {
        let bin: Vec<u8> = (0..=255).collect();
        let event = RemoteEvent::Output {
            session_id: "bin".into(),
            bytes: bin.clone(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &event).unwrap();
        let parsed: RemoteEvent = read_frame(&mut Cursor::new(&buf)).unwrap().unwrap();
        match parsed {
            RemoteEvent::Output { bytes, .. } => assert_eq!(bytes, bin),
            other => panic!("unexpected {other:?}"),
        }
    }
}
