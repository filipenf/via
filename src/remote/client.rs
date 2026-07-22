//! Local GUI client for the remote helper control protocol (length-prefixed CBOR).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use tracing::{debug, warn};

use crate::pty::{OutputNotifier, TerminalSize};

use super::protocol::{RemoteEvent, RemoteRequest, read_frame, write_frame};

struct SessionChannels {
    output_tx: Sender<Vec<u8>>,
    exited: Arc<AtomicBool>,
}

enum ControlWriter {
    Unix(UnixStream),
    Pipe(ChildStdin),
    Dyn(Box<dyn Write + Send>),
}

impl Write for ControlWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buf),
            Self::Pipe(s) => s.write(buf),
            Self::Dyn(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            Self::Pipe(s) => s.flush(),
            Self::Dyn(s) => s.flush(),
        }
    }
}

impl ControlWriter {
    fn shutdown_write(&mut self) {
        match self {
            Self::Unix(s) => {
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
            Self::Pipe(_) | Self::Dyn(_) => {
                // Dropping the pipe / replacing Dyn closes the write end.
            }
        }
    }
}

/// Shared length-prefixed CBOR control connection to a remote helper (via SSH proxy
/// or local socket).
///
/// Writer and session map use separate locks so a blocking write cannot deadlock the
/// reader thread (which only needs the session map to dispatch Output).
pub struct RemoteClient {
    writer: Mutex<ControlWriter>,
    sessions: Mutex<HashMap<String, SessionChannels>>,
    child: Mutex<Option<Child>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    wake: Arc<AtomicBool>,
    notifier: Mutex<Option<Box<dyn OutputNotifier>>>,
    pending_list: Mutex<Option<crossbeam_channel::Sender<Vec<crate::remote::SessionInfo>>>>,
}

/// One remote-backed PTY pane handle (I/O goes through [`RemoteClient`]).
pub struct RemotePane {
    session_id: String,
    client: Arc<RemoteClient>,
    output: Option<Receiver<Vec<u8>>>,
    exited: Arc<AtomicBool>,
    on_drop: PaneDropAction,
}

#[derive(Debug, Clone, Copy)]
enum PaneDropAction {
    Detach,
    Kill,
}

/// Options for remote PTY spawn / reattach.
#[derive(Debug, Clone, Default)]
pub struct PtySpawnOpts {
    /// When false, Attach skips scrollback Replay (nvim / alt-screen).
    pub replay_scrollback: bool,
    pub role: Option<String>,
    pub label: Option<String>,
}

impl PtySpawnOpts {
    pub fn primary_screen(role: impl Into<String>) -> Self {
        let role = role.into();
        Self {
            replay_scrollback: true,
            label: Some(role.clone()),
            role: Some(role),
        }
    }

    pub fn nvim() -> Self {
        Self {
            replay_scrollback: false,
            role: Some("editor".into()),
            label: Some("nvim".into()),
        }
    }
}

impl RemoteClient {
    /// Wrap an already-connected stdio pair (e.g. `ssh … via --remote-proxy`).
    pub fn from_stdio(
        stdin: ChildStdin,
        stdout: ChildStdout,
        child: Child,
        wake: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Self::from_parts(
            ControlWriter::Pipe(stdin),
            Box::new(stdout),
            Some(child),
            wake,
        )
    }

    /// Wrap a Unix control socket (local helper / tests). Clones for the reader;
    /// Drop shuts down the write half so the reader can observe EOF.
    pub fn from_unix_stream(stream: UnixStream, wake: Arc<AtomicBool>) -> Result<Arc<Self>> {
        let reader = stream
            .try_clone()
            .context("clone unix control socket for reader")?;
        Ok(Self::from_parts(
            ControlWriter::Unix(stream),
            Box::new(reader),
            None,
            wake,
        ))
    }

    /// Wrap raw read/write ends (tests with custom duplex).
    pub fn from_rw(
        writer: Box<dyn Write + Send>,
        reader: Box<dyn std::io::Read + Send>,
        child: Option<Child>,
        wake: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Self::from_parts(ControlWriter::Dyn(writer), reader, child, wake)
    }

    fn from_parts(
        writer: ControlWriter,
        reader: Box<dyn std::io::Read + Send>,
        child: Option<Child>,
        wake: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            writer: Mutex::new(writer),
            sessions: Mutex::new(HashMap::new()),
            child: Mutex::new(child),
            reader: Mutex::new(None),
            wake: Arc::clone(&wake),
            notifier: Mutex::new(None),
            pending_list: Mutex::new(None),
        });
        let reader_client = Arc::clone(&client);
        let handle = thread::Builder::new()
            .name("via-remote-client-reader".into())
            .spawn(move || {
                reader_client.read_loop(reader);
            })
            .expect("spawn remote client reader");
        *client.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        client
    }

    /// Install the UI redraw notifier (call once the winit proxy exists).
    pub fn set_output_notifier<N>(&self, notifier: N)
    where
        N: OutputNotifier,
    {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(notifier));
    }

    fn notify_ui(&self) {
        self.wake.store(true, Ordering::Release);
        if let Some(notifier) = self
            .notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            notifier.notify_output();
        }
    }

    fn read_loop(&self, mut reader: Box<dyn Read + Send>) {
        loop {
            match read_frame::<_, RemoteEvent>(&mut reader) {
                Ok(None) => break,
                Ok(Some(event)) => self.dispatch_event(event),
                Err(err) => {
                    warn!(error = %err, "remote client reader stopped");
                    break;
                }
            }
        }
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for session in sessions.values() {
            session.exited.store(true, Ordering::Release);
        }
        drop(sessions);
        self.notify_ui();
    }

    fn dispatch_event(&self, event: RemoteEvent) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            RemoteEvent::Output { session_id, bytes }
            | RemoteEvent::Replay { session_id, bytes } => {
                if let Some(session) = sessions.get(&session_id) {
                    let _ = session.output_tx.send(bytes);
                    drop(sessions);
                    self.notify_ui();
                }
            }
            RemoteEvent::Exit { session_id, .. } => {
                if let Some(session) = sessions.get(&session_id) {
                    session.exited.store(true, Ordering::Release);
                    drop(sessions);
                    self.notify_ui();
                }
            }
            RemoteEvent::Ready { .. } => {}
            RemoteEvent::SessionList { sessions } => {
                if let Some(tx) = self
                    .pending_list
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    let _ = tx.send(sessions);
                }
            }
            RemoteEvent::Error {
                message,
                session_id,
            } => {
                warn!(?session_id, %message, "remote helper error");
            }
        }
    }

    fn send(&self, request: &RemoteRequest) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        write_frame(&mut *writer, request).context("write remote request")?;
        writer.flush().context("flush remote request")?;
        Ok(())
    }

    fn register_session(&self, session_id: &str) -> (Receiver<Vec<u8>>, Arc<AtomicBool>) {
        let (output_tx, output_rx) = unbounded();
        let exited = Arc::new(AtomicBool::new(false));
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.insert(
                session_id.to_string(),
                SessionChannels {
                    output_tx,
                    exited: Arc::clone(&exited),
                },
            );
        }
        (output_rx, exited)
    }

    /// Register local channels, Spawn then Attach (reattach-friendly), return pane handle.
    pub fn spawn_or_attach(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        size: TerminalSize,
        opts: PtySpawnOpts,
    ) -> Result<RemotePane> {
        let session_id = session_id.into();
        if argv.is_empty() {
            bail!("remote spawn argv must be non-empty");
        }

        let (output_rx, exited) = self.register_session(&session_id);

        self.send(&RemoteRequest::Spawn {
            session_id: session_id.clone(),
            argv,
            env,
            cwd,
            cols: size.cols.max(1),
            rows: size.rows.max(1),
            replay_scrollback: opts.replay_scrollback,
            role: opts.role,
            label: opts.label,
        })?;
        // Attach even after a fresh Spawn so Replay/Ready flow is consistent; if the
        // session already existed, Spawn is idempotent and Attach recovers it.
        self.send(&RemoteRequest::Attach {
            session_id: session_id.clone(),
        })?;

        Ok(RemotePane {
            session_id,
            client: Arc::clone(self),
            output: Some(output_rx),
            exited,
            on_drop: PaneDropAction::Detach,
        })
    }

    /// Spawn a non-PTY process (ACP agent) with piped stdio over the control channel.
    pub fn spawn_stdio(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        role: Option<String>,
        label: Option<String>,
    ) -> Result<RemotePane> {
        let session_id = session_id.into();
        if argv.is_empty() {
            bail!("remote spawn_stdio argv must be non-empty");
        }
        let (output_rx, exited) = self.register_session(&session_id);
        self.send(&RemoteRequest::SpawnStdio {
            session_id: session_id.clone(),
            argv,
            env,
            cwd,
            role,
            label,
        })?;
        self.send(&RemoteRequest::Attach {
            session_id: session_id.clone(),
        })?;
        Ok(RemotePane {
            session_id,
            client: Arc::clone(self),
            output: Some(output_rx),
            exited,
            on_drop: PaneDropAction::Kill,
        })
    }

    /// Blocking ListSessions (used on GUI reconnect to inspect the remote roster).
    pub fn list_sessions(&self) -> Result<Vec<crate::remote::SessionInfo>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        {
            let mut pending = self.pending_list.lock().unwrap_or_else(|e| e.into_inner());
            *pending = Some(tx);
        }
        self.send(&RemoteRequest::ListSessions)?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .context("timed out waiting for SessionList")
    }

    pub fn kill(&self, session_id: &str) -> Result<()> {
        self.send(&RemoteRequest::Kill {
            session_id: session_id.to_string(),
        })?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.remove(session_id);
        Ok(())
    }

    pub fn input(&self, session_id: &str, bytes: &[u8]) -> Result<()> {
        self.send(&RemoteRequest::Input {
            session_id: session_id.to_string(),
            bytes: bytes.to_vec(),
        })
    }

    pub fn resize(&self, session_id: &str, size: TerminalSize) -> Result<()> {
        self.send(&RemoteRequest::Resize {
            session_id: session_id.to_string(),
            cols: size.cols.max(1),
            rows: size.rows.max(1),
        })
    }

    pub fn detach(&self, session_id: &str) -> Result<()> {
        self.send(&RemoteRequest::Detach {
            session_id: session_id.to_string(),
        })?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.remove(session_id);
        Ok(())
    }

    /// Detach every pane (GUI quit — do not Shutdown the daemon).
    pub fn detach_all(&self) {
        let ids: Vec<String> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.keys().cloned().collect()
        };
        for id in ids {
            if let Err(err) = self.detach(&id) {
                debug!(session_id = %id, error = %err, "detach on quit failed");
            }
        }
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.detach_all();
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // Half-close writes so the helper sees EOF. Do not join the reader: with a
        // cloned UnixStream the reader holds the other fd, and joining here deadlocks
        // (reader waits for peer EOF; peer waits for us to finish Drop). Detach instead.
        if let Ok(mut writer) = self.writer.lock() {
            writer.shutdown_write();
            *writer = ControlWriter::Dyn(Box::new(std::io::sink()));
        }
        if let Some(handle) = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // Detach: thread exits when its read half sees EOF after the peer closes.
            std::mem::forget(handle);
        }
    }
}

impl RemotePane {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn output(&self) -> &Receiver<Vec<u8>> {
        self.output
            .as_ref()
            .expect("remote pane output already taken")
    }

    /// Take the output receiver (ACP continuous reader after handshake).
    pub fn take_output(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.output.take()
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.client.input(&self.session_id, bytes)
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        self.client.resize(&self.session_id, size)
    }

    pub fn has_exited(&mut self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for RemotePane {
    fn drop(&mut self) {
        match self.on_drop {
            PaneDropAction::Detach => {
                let _ = self.client.detach(&self.session_id);
            }
            PaneDropAction::Kill => {
                let _ = self.client.kill(&self.session_id);
            }
        }
    }
}
