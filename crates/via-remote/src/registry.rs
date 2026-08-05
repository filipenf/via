//! In-process detachable PTY / stdio registry (no SSH).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::pty::{OutputNotifier, PtySession, TerminalSize};

use super::protocol::{RemoteEvent, RemoteRequest, SessionInfo, SessionKind};
use super::scrollback::{DEFAULT_SCROLLBACK_CAP, ScrollbackRing};

struct WakeNotifier {
    flag: Arc<AtomicBool>,
}

impl OutputNotifier for WakeNotifier {
    fn notify_output(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

struct SessionMeta {
    replay_scrollback: bool,
    role: Option<String>,
    label: Option<String>,
}

struct SpawnPtyArgs {
    session_id: String,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    meta: SessionMeta,
}

struct PtyPane {
    session_id: String,
    pty: PtySession,
    scrollback: ScrollbackRing,
    cols: u16,
    rows: u16,
    /// True while a client is attached and should receive live `Output`.
    attached: bool,
    exited: bool,
    exit_code: Option<i32>,
    meta: SessionMeta,
}

struct StdioPane {
    session_id: String,
    child: Child,
    stdin: Option<ChildStdin>,
    output_rx: Receiver<Vec<u8>>,
    scrollback: ScrollbackRing,
    attached: bool,
    exited: bool,
    exit_code: Option<i32>,
    meta: SessionMeta,
}

enum Pane {
    Pty(PtyPane),
    Stdio(StdioPane),
}

impl Pane {
    fn attached_mut(&mut self) -> &mut bool {
        match self {
            Self::Pty(p) => &mut p.attached,
            Self::Stdio(p) => &mut p.attached,
        }
    }

    fn exited(&self) -> bool {
        match self {
            Self::Pty(p) => p.exited,
            Self::Stdio(p) => p.exited,
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Pty(p) => p.exit_code,
            Self::Stdio(p) => p.exit_code,
        }
    }

    fn cols_rows(&self) -> (u16, u16) {
        match self {
            Self::Pty(p) => (p.cols, p.rows),
            Self::Stdio(_) => (0, 0),
        }
    }

    fn scrollback_snapshot(&self) -> Vec<u8> {
        match self {
            Self::Pty(p) => p.scrollback.snapshot(),
            Self::Stdio(p) => p.scrollback.snapshot(),
        }
    }

    fn replay_scrollback(&self) -> bool {
        match self {
            Self::Pty(p) => p.meta.replay_scrollback,
            Self::Stdio(_) => false,
        }
    }

    fn session_info(&self) -> SessionInfo {
        let (cols, rows) = self.cols_rows();
        match self {
            Self::Pty(p) => SessionInfo {
                session_id: p.session_id.clone(),
                alive: !p.exited,
                cols,
                rows,
                kind: SessionKind::Pty,
                replay_scrollback: p.meta.replay_scrollback,
                role: p.meta.role.clone(),
                label: p.meta.label.clone(),
            },
            Self::Stdio(p) => SessionInfo {
                session_id: p.session_id.clone(),
                alive: !p.exited,
                cols,
                rows,
                kind: SessionKind::Stdio,
                replay_scrollback: false,
                role: p.meta.role.clone(),
                label: p.meta.label.clone(),
            },
        }
    }

    #[cfg(test)]
    fn scrollback_len(&self) -> usize {
        match self {
            Self::Pty(p) => p.scrollback.snapshot().len(),
            Self::Stdio(p) => p.scrollback.snapshot().len(),
        }
    }
}

/// Owns detachable PTY / stdio sessions for the remote helper.
pub struct SessionRegistry {
    panes: HashMap<String, Pane>,
    wake: Arc<AtomicBool>,
    scrollback_cap: usize,
    default_cwd: PathBuf,
}

impl SessionRegistry {
    pub fn new(default_cwd: impl Into<PathBuf>) -> Self {
        Self::with_scrollback_cap(default_cwd, DEFAULT_SCROLLBACK_CAP)
    }

    pub fn with_scrollback_cap(default_cwd: impl Into<PathBuf>, scrollback_cap: usize) -> Self {
        Self {
            panes: HashMap::new(),
            wake: Arc::new(AtomicBool::new(false)),
            scrollback_cap,
            default_cwd: default_cwd.into(),
        }
    }

    /// Mark every pane detached (client disconnected; processes keep running).
    pub fn detach_all(&mut self) {
        for pane in self.panes.values_mut() {
            *pane.attached_mut() = false;
        }
    }

    /// Apply one request; returns events to send immediately (e.g. Ready, Replay, errors).
    pub fn handle(&mut self, request: RemoteRequest) -> Result<Vec<RemoteEvent>> {
        match request {
            RemoteRequest::ListSessions => Ok(vec![RemoteEvent::SessionList {
                sessions: self.list_info(),
            }]),
            RemoteRequest::Spawn {
                session_id,
                argv,
                env,
                cwd,
                cols,
                rows,
                replay_scrollback,
                role,
                label,
            } => self.spawn_pty(SpawnPtyArgs {
                session_id,
                argv,
                env,
                cwd,
                cols,
                rows,
                meta: SessionMeta {
                    replay_scrollback,
                    role,
                    label,
                },
            }),
            RemoteRequest::SpawnStdio {
                session_id,
                argv,
                env,
                cwd,
                role,
                label,
            } => self.spawn_stdio(
                session_id,
                argv,
                env,
                cwd,
                SessionMeta {
                    replay_scrollback: false,
                    role,
                    label,
                },
            ),
            RemoteRequest::Attach { session_id } => self.attach(&session_id),
            RemoteRequest::Detach { session_id } => self.detach(&session_id),
            RemoteRequest::Kill { session_id } => self.kill(&session_id),
            RemoteRequest::Resize {
                session_id,
                cols,
                rows,
            } => self.resize(&session_id, cols, rows),
            RemoteRequest::Input { session_id, bytes } => self.input(&session_id, &bytes),
            RemoteRequest::Shutdown => {
                for (_, mut pane) in self.panes.drain() {
                    if let Pane::Stdio(ref mut s) = pane {
                        let _ = s.child.kill();
                        let _ = s.child.wait();
                    }
                }
                Ok(vec![])
            }
        }
    }

    /// Drain output into scrollback; emit `Output` for attached panes and `Exit` once.
    pub fn poll_events(&mut self) -> Vec<RemoteEvent> {
        self.wake.store(false, Ordering::Release);
        let mut events = Vec::new();
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            match pane {
                Pane::Pty(p) => {
                    while let Ok(chunk) = p.pty.output().try_recv() {
                        if chunk.is_empty() {
                            continue;
                        }
                        p.scrollback.push(&chunk);
                        if p.attached && !p.exited {
                            events.push(RemoteEvent::Output {
                                session_id: id.clone(),
                                bytes: chunk,
                            });
                        }
                    }
                    if !p.exited && p.pty.has_exited() {
                        p.exited = true;
                        p.exit_code = None;
                        events.push(RemoteEvent::Exit {
                            session_id: id.clone(),
                            code: p.exit_code,
                        });
                    }
                }
                Pane::Stdio(p) => {
                    while let Ok(chunk) = p.output_rx.try_recv() {
                        if chunk.is_empty() {
                            continue;
                        }
                        p.scrollback.push(&chunk);
                        if p.attached && !p.exited {
                            events.push(RemoteEvent::Output {
                                session_id: id.clone(),
                                bytes: chunk,
                            });
                        }
                    }
                    if !p.exited {
                        match p.child.try_wait() {
                            Ok(Some(status)) => {
                                p.exited = true;
                                p.exit_code = status.code();
                                p.stdin.take();
                                events.push(RemoteEvent::Exit {
                                    session_id: id.clone(),
                                    code: p.exit_code,
                                });
                            }
                            Ok(None) => {}
                            Err(_) => {
                                p.exited = true;
                                p.stdin.take();
                                events.push(RemoteEvent::Exit {
                                    session_id: id.clone(),
                                    code: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        events
    }

    fn list_info(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = self.panes.values().map(|p| p.session_info()).collect();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        sessions
    }

    fn spawn_pty(&mut self, args: SpawnPtyArgs) -> Result<Vec<RemoteEvent>> {
        let SpawnPtyArgs {
            session_id,
            argv,
            env,
            cwd,
            cols,
            rows,
            meta,
        } = args;
        if session_id.is_empty() {
            bail!("session_id must be non-empty");
        }
        if argv.is_empty() {
            bail!("argv must be non-empty");
        }
        // Idempotent reconnect: host may re-Spawn an existing id. Still apply the
        // client's current size so a reattached nvim/agent PTY matches the local VT
        // (stale cols/rows cause classic alt-screen corruption on scroll).
        if self.panes.contains_key(&session_id) {
            let _ = self.resize(&session_id, cols, rows);
            return Ok(vec![RemoteEvent::Ready { session_id }]);
        }

        let (program, args) = argv.split_first().expect("argv non-empty");
        let cwd_path = cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_cwd.clone());
        let env_refs: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let size = TerminalSize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        let notifier = WakeNotifier {
            flag: Arc::clone(&self.wake),
        };
        let pty = PtySession::spawn_with_args(program, args, &cwd_path, &env_refs, size, notifier)
            .with_context(|| format!("spawn PTY for session `{session_id}`"))?;

        self.panes.insert(
            session_id.clone(),
            Pane::Pty(PtyPane {
                session_id: session_id.clone(),
                pty,
                scrollback: ScrollbackRing::with_capacity(self.scrollback_cap),
                cols: size.cols,
                rows: size.rows,
                attached: false,
                exited: false,
                exit_code: None,
                meta,
            }),
        );
        Ok(vec![RemoteEvent::Ready { session_id }])
    }

    fn spawn_stdio(
        &mut self,
        session_id: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        meta: SessionMeta,
    ) -> Result<Vec<RemoteEvent>> {
        if session_id.is_empty() {
            bail!("session_id must be non-empty");
        }
        if argv.is_empty() {
            bail!("argv must be non-empty");
        }
        if self.panes.contains_key(&session_id) {
            return Ok(vec![RemoteEvent::Ready { session_id }]);
        }

        let (program, args) = argv.split_first().expect("argv non-empty");
        let cwd_path = cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_cwd.clone());
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&cwd_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn stdio process for session `{session_id}`"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("stdio session stdout")?;
        let stderr = child.stderr.take().context("stdio session stderr")?;
        let (tx, rx) = unbounded();
        let wake = Arc::clone(&self.wake);
        spawn_pipe_reader(stdout, tx.clone(), Arc::clone(&wake));
        spawn_pipe_reader(stderr, tx, Arc::clone(&wake));

        self.panes.insert(
            session_id.clone(),
            Pane::Stdio(StdioPane {
                session_id: session_id.clone(),
                child,
                stdin,
                output_rx: rx,
                scrollback: ScrollbackRing::with_capacity(self.scrollback_cap.min(256 * 1024)),
                attached: false,
                exited: false,
                exit_code: None,
                meta,
            }),
        );
        Ok(vec![RemoteEvent::Ready { session_id }])
    }

    fn attach(&mut self, session_id: &str) -> Result<Vec<RemoteEvent>> {
        let Some(pane) = self.panes.get_mut(session_id) else {
            return Ok(vec![RemoteEvent::Error {
                message: format!("unknown session `{session_id}`"),
                session_id: Some(session_id.into()),
            }]);
        };
        *pane.attached_mut() = true;
        let mut events = vec![RemoteEvent::Ready {
            session_id: session_id.into(),
        }];
        // Spike: primary-screen replay only; nvim/alt-screen and ACP stdio skip Replay.
        if pane.replay_scrollback() {
            let snap = pane.scrollback_snapshot();
            if !snap.is_empty() {
                events.push(RemoteEvent::Replay {
                    session_id: session_id.into(),
                    bytes: snap,
                });
            }
        } else if let Pane::Pty(p) = pane {
            // Nudge nvim to redraw after reattach (SIGWINCH via resize).
            let size = TerminalSize {
                rows: p.rows.max(1),
                cols: p.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            };
            let _ = p.pty.resize(size);
        }
        if pane.exited() {
            events.push(RemoteEvent::Exit {
                session_id: session_id.into(),
                code: pane.exit_code(),
            });
        }
        Ok(events)
    }

    fn detach(&mut self, session_id: &str) -> Result<Vec<RemoteEvent>> {
        let Some(pane) = self.panes.get_mut(session_id) else {
            return Ok(vec![RemoteEvent::Error {
                message: format!("unknown session `{session_id}`"),
                session_id: Some(session_id.into()),
            }]);
        };
        *pane.attached_mut() = false;
        Ok(vec![])
    }

    fn kill(&mut self, session_id: &str) -> Result<Vec<RemoteEvent>> {
        let Some(mut pane) = self.panes.remove(session_id) else {
            return Ok(vec![RemoteEvent::Error {
                message: format!("unknown session `{session_id}`"),
                session_id: Some(session_id.into()),
            }]);
        };
        match &mut pane {
            Pane::Pty(p) => {
                // PtySession drops / kills with the value.
                let _ = p;
            }
            Pane::Stdio(p) => {
                let _ = p.child.kill();
                let _ = p.child.wait();
            }
        }
        Ok(vec![RemoteEvent::Exit {
            session_id: session_id.into(),
            code: None,
        }])
    }

    fn resize(&mut self, session_id: &str, cols: u16, rows: u16) -> Result<Vec<RemoteEvent>> {
        let Some(pane) = self.panes.get_mut(session_id) else {
            return Ok(vec![RemoteEvent::Error {
                message: format!("unknown session `{session_id}`"),
                session_id: Some(session_id.into()),
            }]);
        };
        match pane {
            Pane::Pty(p) => {
                let size = TerminalSize {
                    rows: rows.max(1),
                    cols: cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                };
                p.pty.resize(size)?;
                p.cols = size.cols;
                p.rows = size.rows;
            }
            Pane::Stdio(_) => {
                // No-op for piped ACP processes.
            }
        }
        Ok(vec![])
    }

    fn input(&mut self, session_id: &str, bytes: &[u8]) -> Result<Vec<RemoteEvent>> {
        let Some(pane) = self.panes.get_mut(session_id) else {
            return Ok(vec![RemoteEvent::Error {
                message: format!("unknown session `{session_id}`"),
                session_id: Some(session_id.into()),
            }]);
        };
        if pane.exited() {
            return Ok(vec![RemoteEvent::Error {
                message: format!("session `{session_id}` has exited"),
                session_id: Some(session_id.into()),
            }]);
        }
        match pane {
            Pane::Pty(p) => p.pty.write_all(bytes)?,
            Pane::Stdio(p) => {
                let Some(stdin) = p.stdin.as_mut() else {
                    return Ok(vec![RemoteEvent::Error {
                        message: format!("session `{session_id}` stdin closed"),
                        session_id: Some(session_id.into()),
                    }]);
                };
                stdin.write_all(bytes)?;
                stdin.flush()?;
            }
        }
        Ok(vec![])
    }

    #[cfg(test)]
    pub(super) fn scrollback_len(&self, session_id: &str) -> Option<usize> {
        self.panes.get(session_id).map(|p| p.scrollback_len())
    }
}

fn spawn_pipe_reader(
    mut reader: impl Read + Send + 'static,
    tx: Sender<Vec<u8>>,
    wake: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("via-remote-stdio-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = tx.send(buf[..n].to_vec());
                        wake.store(true, Ordering::Release);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .expect("spawn stdio reader");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(reg: &mut SessionRegistry, pred: impl Fn(&SessionRegistry) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let _ = reg.poll_events();
            if pred(reg) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for registry condition");
    }

    #[test]
    fn spawn_attach_captures_output_and_survives_detach() {
        let mut reg = SessionRegistry::with_scrollback_cap(std::env::temp_dir(), 64 * 1024);
        let events = reg
            .handle(RemoteRequest::Spawn {
                session_id: "echo".into(),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf 'hello-remote\\n'; sleep 0.2".into(),
                ],
                env: vec![],
                cwd: None,
                cols: 80,
                rows: 24,
                replay_scrollback: true,
                role: None,
                label: None,
            })
            .unwrap();
        assert!(matches!(
            &events[..],
            [RemoteEvent::Ready { session_id }] if session_id == "echo"
        ));

        wait_for(&mut reg, |r| r.scrollback_len("echo").unwrap_or(0) > 0);

        let attach = reg
            .handle(RemoteRequest::Attach {
                session_id: "echo".into(),
            })
            .unwrap();
        let replay = attach.iter().find_map(|e| match e {
            RemoteEvent::Replay { bytes, .. } => Some(bytes.clone()),
            _ => None,
        });
        let replay = replay.expect("expected Replay with scrollback");
        assert!(
            String::from_utf8_lossy(&replay).contains("hello-remote"),
            "replay={:?}",
            String::from_utf8_lossy(&replay)
        );

        reg.handle(RemoteRequest::Detach {
            session_id: "echo".into(),
        })
        .unwrap();

        // Process still listed while alive / after exit until Shutdown.
        let list = reg.handle(RemoteRequest::ListSessions).unwrap();
        match &list[0] {
            RemoteEvent::SessionList { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, "echo");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn spawn_existing_session_is_idempotent() {
        let mut reg = SessionRegistry::new(std::env::temp_dir());
        reg.handle(RemoteRequest::Spawn {
            session_id: "shell".into(),
            argv: vec!["sh".into(), "-c".into(), "sleep 2".into()],
            env: vec![],
            cwd: None,
            cols: 80,
            rows: 24,
            replay_scrollback: true,
            role: None,
            label: None,
        })
        .unwrap();
        let again = reg
            .handle(RemoteRequest::Spawn {
                session_id: "shell".into(),
                argv: vec!["sh".into(), "-c".into(), "echo nope".into()],
                env: vec![],
                cwd: None,
                cols: 80,
                rows: 24,
                replay_scrollback: true,
                role: None,
                label: None,
            })
            .unwrap();
        assert!(matches!(
            &again[..],
            [RemoteEvent::Ready { session_id }] if session_id == "shell"
        ));
        reg.handle(RemoteRequest::Kill {
            session_id: "shell".into(),
        })
        .unwrap();
    }

    #[test]
    fn spawn_stdio_echo_roundtrip() {
        let mut reg = SessionRegistry::new(std::env::temp_dir());
        reg.handle(RemoteRequest::SpawnStdio {
            session_id: "acp".into(),
            argv: vec!["sh".into(), "-c".into(), "cat".into()],
            env: vec![],
            cwd: None,
            role: None,
            label: None,
        })
        .unwrap();
        reg.handle(RemoteRequest::Attach {
            session_id: "acp".into(),
        })
        .unwrap();
        reg.handle(RemoteRequest::Input {
            session_id: "acp".into(),
            bytes: b"hello-stdio\n".to_vec(),
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw = false;
        while Instant::now() < deadline {
            for ev in reg.poll_events() {
                if let RemoteEvent::Output { bytes, .. } = ev {
                    if String::from_utf8_lossy(&bytes).contains("hello-stdio") {
                        saw = true;
                    }
                }
            }
            if saw {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw, "expected stdio Output with hello-stdio");
        reg.handle(RemoteRequest::Kill {
            session_id: "acp".into(),
        })
        .unwrap();
    }

    #[test]
    fn input_resize_unknown_session_errors() {
        let mut reg = SessionRegistry::new(std::env::temp_dir());
        let err = reg
            .handle(RemoteRequest::Input {
                session_id: "missing".into(),
                bytes: b"x".to_vec(),
            })
            .unwrap();
        assert!(matches!(
            &err[..],
            [RemoteEvent::Error {
                session_id: Some(id),
                ..
            }] if id == "missing"
        ));

        reg.handle(RemoteRequest::Spawn {
            session_id: "shell".into(),
            argv: vec!["sh".into(), "-c".into(), "cat".into()],
            env: vec![],
            cwd: None,
            cols: 40,
            rows: 10,
            replay_scrollback: true,
            role: None,
            label: None,
        })
        .unwrap();
        reg.handle(RemoteRequest::Resize {
            session_id: "shell".into(),
            cols: 100,
            rows: 30,
        })
        .unwrap();
        reg.handle(RemoteRequest::Input {
            session_id: "shell".into(),
            bytes: b"hi\n".to_vec(),
        })
        .unwrap();
        reg.handle(RemoteRequest::Shutdown).unwrap();
    }

    #[test]
    fn idempotent_spawn_resizes_existing_pty() {
        let mut reg = SessionRegistry::with_scrollback_cap(std::env::temp_dir(), 64 * 1024);
        reg.handle(RemoteRequest::Spawn {
            session_id: "nvim".into(),
            argv: vec!["sh".into(), "-c".into(), "sleep 2".into()],
            env: vec![],
            cwd: None,
            cols: 40,
            rows: 10,
            replay_scrollback: false,
            role: Some("editor".into()),
            label: Some("nvim".into()),
        })
        .unwrap();

        // Reconnect with a different view size — must update stored geometry.
        reg.handle(RemoteRequest::Spawn {
            session_id: "nvim".into(),
            argv: vec!["sh".into(), "-c".into(), "sleep 2".into()],
            env: vec![],
            cwd: None,
            cols: 120,
            rows: 40,
            replay_scrollback: false,
            role: Some("editor".into()),
            label: Some("nvim".into()),
        })
        .unwrap();

        let info = reg
            .handle(RemoteRequest::ListSessions)
            .unwrap()
            .into_iter()
            .find_map(|ev| match ev {
                RemoteEvent::SessionList { sessions } => Some(sessions),
                _ => None,
            })
            .expect("session list");
        let nvim = info.iter().find(|s| s.session_id == "nvim").unwrap();
        assert_eq!(nvim.cols, 120);
        assert_eq!(nvim.rows, 40);
        reg.handle(RemoteRequest::Shutdown).unwrap();
    }

    #[test]
    fn detach_keeps_process_reattach_replays_scrollback() {
        let mut reg = SessionRegistry::with_scrollback_cap(std::env::temp_dir(), 64 * 1024);
        reg.handle(RemoteRequest::Spawn {
            session_id: "agent".into(),
            argv: vec![
                "sh".into(),
                "-c".into(),
                "printf 'survive-disconnect\\n'; sleep 2".into(),
            ],
            env: vec![],
            cwd: None,
            cols: 80,
            rows: 24,
            replay_scrollback: true,
            role: Some("agent".into()),
            label: Some("agent".into()),
        })
        .unwrap();

        wait_for(&mut reg, |r| r.scrollback_len("agent").unwrap_or(0) > 0);

        // Simulate SSH/proxy drop: detach without killing.
        reg.detach_all();
        let list = reg.handle(RemoteRequest::ListSessions).unwrap();
        match &list[0] {
            RemoteEvent::SessionList { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert!(sessions[0].alive);
                assert!(sessions[0].replay_scrollback);
                assert_eq!(sessions[0].role.as_deref(), Some("agent"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let attach = reg
            .handle(RemoteRequest::Attach {
                session_id: "agent".into(),
            })
            .unwrap();
        let replay = attach.iter().find_map(|e| match e {
            RemoteEvent::Replay { bytes, .. } => Some(bytes.clone()),
            _ => None,
        });
        let replay = replay.expect("expected Replay after reattach");
        assert!(
            String::from_utf8_lossy(&replay).contains("survive-disconnect"),
            "replay={:?}",
            String::from_utf8_lossy(&replay)
        );
        reg.handle(RemoteRequest::Kill {
            session_id: "agent".into(),
        })
        .unwrap();
    }

    #[test]
    fn nvim_attach_skips_scrollback_replay() {
        let mut reg = SessionRegistry::with_scrollback_cap(std::env::temp_dir(), 64 * 1024);
        reg.handle(RemoteRequest::Spawn {
            session_id: "nvim".into(),
            argv: vec![
                "sh".into(),
                "-c".into(),
                "printf 'nvim-bytes\\n'; sleep 0.3".into(),
            ],
            env: vec![],
            cwd: None,
            cols: 80,
            rows: 24,
            replay_scrollback: false,
            role: Some("editor".into()),
            label: Some("nvim".into()),
        })
        .unwrap();
        wait_for(&mut reg, |r| r.scrollback_len("nvim").unwrap_or(0) > 0);
        let attach = reg
            .handle(RemoteRequest::Attach {
                session_id: "nvim".into(),
            })
            .unwrap();
        assert!(
            attach
                .iter()
                .all(|e| !matches!(e, RemoteEvent::Replay { .. })),
            "nvim must not Replay scrollback: {attach:?}"
        );
        assert!(
            attach
                .iter()
                .any(|e| matches!(e, RemoteEvent::Ready { session_id } if session_id == "nvim"))
        );
        reg.handle(RemoteRequest::Kill {
            session_id: "nvim".into(),
        })
        .unwrap();
    }
}
