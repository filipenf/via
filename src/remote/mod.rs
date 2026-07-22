//! Remote execution helper: detachable PTY sessions over a via-owned control socket.
//!
//! Entry points (early-dispatch, no GUI):
//! - `via --remote-serve` — session authority (daemon or `--remote-foreground`)
//! - `via --remote-proxy` — stdio ↔ control socket (SSH pipe target)
//!
//! Local GUI attach (one helper per host; no session picker):
//! - `via --remote <host>` (primary) or `via remote <host>` (alias)
//! - Connect ensures the helper is up, then attaches. GUI quit = Detach.
//!
//! See Obsidian `Spike — Remote execution` / `Spec — Remote execution`.

mod client;
mod connect;
mod protocol;
mod proxy;
mod registry;
mod scrollback;
mod serve;

use std::path::PathBuf;

use anyhow::Result;

pub use client::{PtySpawnOpts, RemoteClient, RemotePane};
pub use connect::{ConnectOptions, connect};
#[allow(unused_imports)] // public API for protocol consumers / tests
pub use protocol::{RemoteEvent, RemoteRequest, SessionInfo, SessionKind};
#[allow(unused_imports)]
pub use registry::SessionRegistry;
pub use serve::{ServeArgs, VIA_REMOTE_FOREGROUND_ENV};

use crate::config;

/// CLI options for remote helper / proxy early-dispatch.
#[derive(Debug, Clone)]
pub struct Args {
    pub serve: bool,
    pub proxy: bool,
    pub foreground: bool,
    pub socket: Option<PathBuf>,
}

impl Args {
    pub fn wants_early_dispatch(&self) -> bool {
        self.serve || self.proxy
    }
}

/// Default control socket: `$XDG_DATA_HOME/via/remote/control.sock`.
pub fn default_control_socket() -> PathBuf {
    config::via_data_dir().join("remote").join("control.sock")
}

pub fn run(args: Args) -> Result<()> {
    let socket = args.socket.unwrap_or_else(default_control_socket);
    if args.serve {
        return serve::run(ServeArgs {
            socket,
            foreground: args.foreground || std::env::var_os(VIA_REMOTE_FOREGROUND_ENV).is_some(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        });
    }
    if args.proxy {
        return proxy::run(proxy::ProxyArgs { socket });
    }
    anyhow::bail!("remote helper invoked without --remote-serve or --remote-proxy");
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::{PtySpawnOpts, RemoteClient};
    use protocol::{RemoteEvent, RemoteRequest, read_frame, write_frame};
    use serve::{bind_control_socket, handle_client, handle_request};
    use std::io::{Cursor, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::pty::TerminalSize;

    #[test]
    fn in_process_request_spawn_list() {
        let mut reg = SessionRegistry::new(std::env::temp_dir());
        let events = handle_request(
            &mut reg,
            RemoteRequest::Spawn {
                session_id: "t".into(),
                argv: vec!["true".into()],
                env: vec![],
                cwd: None,
                cols: 80,
                rows: 24,
                replay_scrollback: true,
                role: None,
                label: None,
            },
        )
        .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RemoteEvent::Ready { session_id } if session_id == "t"))
        );
        let list = handle_request(&mut reg, RemoteRequest::ListSessions).unwrap();
        match &list[0] {
            RemoteEvent::SessionList { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, "t");
            }
            other => panic!("expected SessionList, got {other:?}"),
        }
    }

    #[test]
    fn local_socket_client_spawn_attach_output() {
        let dir = std::env::temp_dir().join(format!("via-remote-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        let listener = bind_control_socket(&socket).unwrap();
        let registry = Arc::new(Mutex::new(SessionRegistry::new(std::env::temp_dir())));

        let registry_server = Arc::clone(&registry);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            handle_client(stream, registry_server).expect("handle_client");
        });

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        write_frame(
            &mut stream,
            &RemoteRequest::Spawn {
                session_id: "echo".into(),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf 'from-helper\\n'; sleep 0.3".into(),
                ],
                env: vec![],
                cwd: None,
                cols: 80,
                rows: 24,
                replay_scrollback: true,
                role: None,
                label: None,
            },
        )
        .unwrap();
        write_frame(
            &mut stream,
            &RemoteRequest::Attach {
                session_id: "echo".into(),
            },
        )
        .unwrap();
        stream.flush().unwrap();

        let mut reader = stream.try_clone().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut saw_ready = false;
        let mut saw_hello = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !(saw_ready && saw_hello) {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    while let Some(ev) =
                        protocol::try_read_frame_from_buf::<RemoteEvent>(&mut buf).unwrap()
                    {
                        match ev {
                            RemoteEvent::Ready { session_id } if session_id == "echo" => {
                                saw_ready = true;
                            }
                            RemoteEvent::Output { bytes, .. }
                            | RemoteEvent::Replay { bytes, .. }
                                if String::from_utf8_lossy(&bytes).contains("from-helper") =>
                            {
                                saw_hello = true;
                            }
                            _ => {}
                        }
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("{err}"),
            }
        }
        assert!(saw_ready, "expected Ready");
        assert!(saw_hello, "expected Output/Replay with from-helper");

        write_frame(&mut stream, &RemoteRequest::Shutdown).unwrap();
        drop(stream);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_client_writes_spawn_to_buffer() {
        // No live helper: ensure spawn_or_attach encodes requests without deadlocking Drop.
        let writer = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let client = RemoteClient::from_rw(
            Box::new(SharedWriter(Arc::clone(&writer))),
            Box::new(std::io::empty()),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let pane = client
            .spawn_or_attach(
                "t",
                vec!["true".into()],
                vec![],
                None,
                TerminalSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                PtySpawnOpts::primary_screen("t"),
            )
            .unwrap();
        let bytes = writer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // CBOR text strings for type tags still appear as ASCII in the payload.
        assert!(
            bytes.windows(5).any(|w| w == b"spawn"),
            "requests={bytes:?}"
        );
        assert!(
            bytes.windows(6).any(|w| w == b"attach"),
            "requests={bytes:?}"
        );
        // Two length-prefixed frames (Spawn + Attach).
        let mut cursor = Cursor::new(&bytes);
        let first: RemoteRequest = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(first, RemoteRequest::Spawn { .. }));
        let second: RemoteRequest = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(second, RemoteRequest::Attach { .. }));
        drop(pane);
        drop(client);
    }
}
