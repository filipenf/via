//! Host-side control plane for a PTY-hosted `via --acp-tui` child.
//!
//! Binds a Unix listener **before** spawn; accepts the child connection non-blocking
//! and exchanges newline-delimited JSON ([`HostToTui`] / [`TuiToHost`]).
//!
//! Outbound writes go through [`AcpTuiBridge::write_buf`] and are drained with
//! explicit partial-write handling: non-blocking `write_all`/`writeln!` can return
//! `WouldBlock` after writing only some bytes, which would corrupt NDJSON framing.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use super::VIA_ACP_UI_SOCKET_ENV;
#[cfg(test)]
use super::protocol::TranscriptKind;
use super::protocol::{HostToTui, TuiToHost, encode_host_line, parse_tui_line};

/// Override path to the executable that understands `--acp-tui` (defaults to `current_exe()`).
pub const VIA_ACP_TUI_BIN_ENV: &str = "VIA_ACP_TUI_BIN";

/// Resolve the binary used to host the ACP TUI.
pub fn resolve_acp_tui_bin() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(VIA_ACP_TUI_BIN_ENV) {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe().context("resolve current_exe for via --acp-tui")
}

/// Socket path under the instance runtime dir for one agent pane.
pub fn socket_path_for_agent(runtime_dir: &Path, agent_id: &str) -> PathBuf {
    runtime_dir.join(format!("acp-ui-{}.sock", sanitize_agent_id(agent_id)))
}

fn sanitize_agent_id(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// One ACP TUI control-plane session (listener → connected stream).
pub struct AcpTuiBridge {
    agent_id: String,
    socket_path: PathBuf,
    listener: Option<UnixListener>,
    stream: Option<UnixStream>,
    read_buf: Vec<u8>,
    /// Encoded host→TUI bytes not yet fully written (may hold a partial line).
    write_buf: Vec<u8>,
    /// Messages waiting until the stream is connected (or until `write_buf` drains).
    pending: VecDeque<HostToTui>,
}

impl AcpTuiBridge {
    /// Bind a non-blocking listener at `socket_path` (removes a stale socket first).
    pub fn bind(agent_id: impl Into<String>, socket_path: PathBuf) -> Result<Self> {
        let agent_id = agent_id.into();
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("remove stale ACP UI socket {}", socket_path.display()))?;
        }
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create ACP UI socket dir {}", parent.display()))?;
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind ACP UI socket {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("set ACP UI listener non-blocking")?;
        Ok(Self {
            agent_id,
            socket_path,
            listener: Some(listener),
            stream: None,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            pending: VecDeque::new(),
        })
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Queue or write a host→TUI message.
    pub fn send(&mut self, msg: HostToTui) -> Result<()> {
        self.try_accept()?;
        if self.stream.is_some() {
            self.enqueue_host_msg(&msg)?;
            self.flush_outbound()?;
        } else {
            self.pending.push_back(msg);
        }
        Ok(())
    }

    /// Accept (if needed), flush outbound bytes / pending, read any TUI→host lines.
    pub fn poll(&mut self) -> Result<Vec<TuiToHost>> {
        self.try_accept()?;
        self.flush_outbound()?;
        self.read_messages()
    }

    /// Send shutdown and best-effort wait for [`TuiToHost::ShutdownAck`].
    pub fn shutdown(&mut self) {
        if let Err(err) = self.send(HostToTui::Shutdown) {
            debug!(agent_id = %self.agent_id, %err, "ACP TUI shutdown send failed");
        }
        for _ in 0..10 {
            match self.poll() {
                Ok(msgs) => {
                    if msgs.iter().any(|msg| matches!(msg, TuiToHost::ShutdownAck)) {
                        break;
                    }
                }
                Err(err) => {
                    debug!(agent_id = %self.agent_id, %err, "ACP TUI shutdown poll failed");
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn try_accept(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let Some(listener) = self.listener.as_ref() else {
            return Ok(());
        };
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(true)
                    .context("set ACP UI stream non-blocking")?;
                debug!(
                    agent_id = %self.agent_id,
                    path = %self.socket_path.display(),
                    "ACP TUI control socket connected"
                );
                self.stream = Some(stream);
                self.listener = None;
                self.flush_outbound()?;
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => {
                return Err(err).context("accept ACP UI socket");
            }
        }
        Ok(())
    }

    fn enqueue_host_msg(&mut self, msg: &HostToTui) -> Result<()> {
        let line = encode_host_line(msg)?;
        self.write_buf.extend_from_slice(line.as_bytes());
        self.write_buf.push(b'\n');
        Ok(())
    }

    /// Drain `write_buf`, then encode any `pending` messages that fit.
    ///
    /// Partial writes stay in `write_buf` (never re-queued as structured messages) so a
    /// `WouldBlock` mid-line cannot leave a truncated JSON frame followed by a full
    /// re-send of the same message.
    fn flush_outbound(&mut self) -> Result<()> {
        self.flush_write_buf()?;
        while self.stream.is_some() && self.write_buf.is_empty() {
            let Some(msg) = self.pending.pop_front() else {
                break;
            };
            self.enqueue_host_msg(&msg)?;
            self.flush_write_buf()?;
            if !self.write_buf.is_empty() {
                break;
            }
        }
        Ok(())
    }

    /// Write as much of `write_buf` as the non-blocking socket accepts.
    ///
    /// Returns `Ok(())` even if bytes remain (caller / next [`Self::poll`] retries). Hard I/O
    /// errors propagate. On `WouldBlock`, spins briefly so small control messages usually
    /// complete without waiting for the next UI frame.
    fn flush_write_buf(&mut self) -> Result<()> {
        const MAX_SPINS: u32 = 64;
        for spin in 0..MAX_SPINS {
            if self.write_buf.is_empty() {
                return Ok(());
            }
            let Some(stream) = self.stream.as_mut() else {
                return Ok(());
            };
            match stream.write(&self.write_buf) {
                Ok(0) => {
                    bail!(
                        "ACP UI write returned 0 with {} bytes pending",
                        self.write_buf.len()
                    );
                }
                Ok(n) => {
                    self.write_buf.drain(..n);
                    continue;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if spin + 1 == MAX_SPINS {
                        // Leave remainder for a later poll(); do not treat as fatal.
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_micros(50));
                    continue;
                }
                Err(err) => return Err(err).context("write ACP UI host message"),
            }
        }
        Ok(())
    }

    fn read_messages(&mut self) -> Result<Vec<TuiToHost>> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(Vec::new());
        };
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    warn!(
                        agent_id = %self.agent_id,
                        "ACP TUI control socket closed by peer"
                    );
                    self.stream = None;
                    break;
                }
                Ok(n) => self.read_buf.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(err).context("read ACP UI socket"),
            }
        }

        let mut messages = Vec::new();
        while let Some(newline) = self.read_buf.iter().position(|&b| b == b'\n') {
            let line = self.read_buf.drain(..=newline).collect::<Vec<u8>>();
            let line = String::from_utf8_lossy(&line);
            match parse_tui_line(line.trim()) {
                Ok(Some(msg)) => messages.push(msg),
                Ok(None) => {}
                Err(err) => {
                    warn!(
                        agent_id = %self.agent_id,
                        %err,
                        "invalid TUI→host IPC line"
                    );
                }
            }
        }
        Ok(messages)
    }
}

impl Drop for AcpTuiBridge {
    fn drop(&mut self) {
        self.listener.take();
        self.stream.take();
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// Env + argv fragment for spawning the TUI child (after [`AcpTuiBridge::bind`]).
pub fn spawn_env_and_args<'a>(
    agent_id: &'a str,
    role: &'a str,
    socket: &'a str,
) -> (Vec<(&'a str, &'a str)>, Vec<&'a str>) {
    let env = vec![
        (crate::agent_bus::VIA_AGENT_ID_ENV, agent_id),
        (crate::agent_bus::VIA_AGENT_ROLE_ENV, role),
        (VIA_ACP_UI_SOCKET_ENV, socket),
    ];
    let args = vec![
        "--acp-tui",
        "--agent-id",
        agent_id,
        "--role",
        role,
        "--socket",
        socket,
    ];
    (env, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use super::super::protocol::parse_host_line;

    #[test]
    fn bind_before_connect_and_roundtrip() {
        let dir = std::env::temp_dir().join(format!("via-acp-ui-host-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = socket_path_for_agent(&dir, "coder");
        let mut bridge = AcpTuiBridge::bind("coder", path.clone()).unwrap();

        let client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();
        let mut client = client;

        let deadline = Instant::now() + Duration::from_millis(500);
        while bridge.stream.is_none() && Instant::now() < deadline {
            bridge.try_accept().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bridge.stream.is_some());

        bridge
            .send(HostToTui::Transcript {
                kind: TranscriptKind::Agent,
                text: "hi".into(),
            })
            .unwrap();

        let mut buf = vec![0u8; 256];
        let mut got = String::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    got.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if got.contains('\n') {
                        break;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    // Resume any leftover write_buf from a WouldBlock mid-send.
                    bridge.flush_outbound().unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("{err}"),
            }
        }
        assert!(got.contains(r#""type":"transcript""#));
        assert!(got.contains("hi"));

        writeln!(client, r#"{{"type":"submit","text":"ping"}}"#).unwrap();
        client.flush().unwrap();

        let deadline = Instant::now() + Duration::from_millis(500);
        let mut msgs = Vec::new();
        while Instant::now() < deadline {
            msgs.extend(bridge.poll().unwrap());
            if !msgs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            msgs.iter()
                .any(|m| matches!(m, TuiToHost::Submit { text } if text == "ping"))
        );

        drop(bridge);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_write_buf_drains_remainder_after_simulated_partial() {
        let dir = std::env::temp_dir().join(format!("via-acp-ui-partial-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = socket_path_for_agent(&dir, "coder");
        let mut bridge = AcpTuiBridge::bind("coder", path.clone()).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();

        let deadline = Instant::now() + Duration::from_millis(500);
        while bridge.stream.is_none() && Instant::now() < deadline {
            bridge.try_accept().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }

        let line = encode_host_line(&HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "partial-write-safe".into(),
        })
        .unwrap();
        bridge.write_buf.extend_from_slice(line.as_bytes());
        bridge.write_buf.push(b'\n');
        // Simulate bytes already accepted by the kernel (partial write).
        let mid = 8.min(bridge.write_buf.len() - 1);
        let already = bridge.write_buf.drain(..mid).collect::<Vec<u8>>();
        assert!(!already.is_empty());
        assert!(!bridge.write_buf.is_empty());

        bridge.flush_write_buf().unwrap();
        assert!(
            bridge.write_buf.is_empty(),
            "remainder should drain once the peer can accept data"
        );

        // Reconstruct the on-wire frame: prefix that "already left" + flushed remainder.
        let mut got = String::from_utf8_lossy(&already).into_owned();
        let mut buf = vec![0u8; 512];
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    got.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if got.contains('\n') {
                        break;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    bridge.flush_write_buf().unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("{err}"),
            }
        }
        let msg = parse_host_line(got.trim()).unwrap().unwrap();
        assert_eq!(
            msg,
            HostToTui::Transcript {
                kind: TranscriptKind::Agent,
                text: "partial-write-safe".into(),
            }
        );

        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_replaces_path_unsafe() {
        assert_eq!(sanitize_agent_id("a/b c"), "a_b_c");
    }
}
