//! Remote helper daemon: Unix control socket + [`SessionRegistry`].

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::protocol::{RemoteEvent, RemoteRequest, try_read_frame_from_buf, write_frame};
use super::registry::SessionRegistry;

/// Env set after daemon re-exec so nested `--serve` stays foreground.
pub const VIA_REMOTE_FOREGROUND_ENV: &str = "VIA_REMOTE_FOREGROUND";

#[derive(Debug, Clone)]
pub struct ServeArgs {
    pub socket: PathBuf,
    pub foreground: bool,
    pub cwd: PathBuf,
}

pub fn run(args: ServeArgs) -> Result<()> {
    if !args.foreground && std::env::var_os(VIA_REMOTE_FOREGROUND_ENV).is_none() {
        return daemonize_and_reexec(&args);
    }

    let listener = bind_control_socket(&args.socket)?;
    info!(
        socket = %args.socket.display(),
        cwd = %args.cwd.display(),
        "remote helper listening"
    );
    eprintln!("via remote helper listening on {}", args.socket.display());

    let registry = Arc::new(Mutex::new(SessionRegistry::new(args.cwd)));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let registry = Arc::clone(&registry);
                let socket = args.socket.clone();
                std::thread::Builder::new()
                    .name("via-remote-client".into())
                    .spawn(move || {
                        if let Err(err) = handle_client(stream, registry) {
                            warn!(error = %err, socket = %socket.display(), "remote client ended");
                        }
                    })
                    .context("spawn remote client thread")?;
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("accept remote control connection"),
        }
    }
    Ok(())
}

/// Bind (after removing a stale socket). Public for local-subprocess tests.
pub fn bind_control_socket(socket: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create remote socket dir {}", parent.display()))?;
    }
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("remove stale remote socket {}", socket.display()))?;
    }
    UnixListener::bind(socket)
        .with_context(|| format!("bind remote control socket {}", socket.display()))
}

fn daemonize_and_reexec(args: &ServeArgs) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let pid1 = unsafe { libc::fork() };
        if pid1 < 0 {
            return Err(std::io::Error::last_os_error()).context("serve first fork");
        }
        if pid1 > 0 {
            println!("via remote helper socket: {}", args.socket.display());
            unsafe { libc::_exit(0) };
        }
        if unsafe { libc::setsid() } < 0 {
            return Err(std::io::Error::last_os_error()).context("serve setsid");
        }
        let pid2 = unsafe { libc::fork() };
        if pid2 < 0 {
            return Err(std::io::Error::last_os_error()).context("serve second fork");
        }
        if pid2 > 0 {
            unsafe { libc::_exit(0) };
        }

        let exe = std::env::current_exe().context("current_exe for serve re-exec")?;
        let mut cmd = Command::new(&exe);
        cmd.arg("serve")
            .arg("--foreground")
            .arg("--socket")
            .arg(&args.socket);
        cmd.env(VIA_REMOTE_FOREGROUND_ENV, "1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let err = cmd.exec();
        Err(err).context("exec serve daemon")
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        anyhow::bail!("remote helper is only supported on Unix")
    }
}

/// Serve a single accepted client (used by tests and the accept loop).
pub fn handle_client(stream: UnixStream, registry: Arc<Mutex<SessionRegistry>>) -> Result<()> {
    stream
        .set_nonblocking(false)
        .context("set remote client blocking")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .context("set remote client read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("set remote client write timeout")?;

    let mut writer = stream.try_clone().context("clone remote client stream")?;
    let mut reader = stream;
    let mut buf = Vec::new();
    let mut read_tmp = [0u8; 4096];

    loop {
        match reader.read(&mut read_tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&read_tmp[..n]);
                while let Some(req) = try_read_frame_from_buf::<RemoteRequest>(&mut buf)? {
                    if !dispatch_request(req, &registry, &mut writer)? {
                        detach_all(&registry);
                        return Ok(());
                    }
                }
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                flush_poll(&registry, &mut writer)?;
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("read remote control stream"),
        }
        flush_poll(&registry, &mut writer)?;
    }

    detach_all(&registry);
    Ok(())
}

/// Returns `false` when the client requested Shutdown (connection should end).
fn dispatch_request(
    req: RemoteRequest,
    registry: &Mutex<SessionRegistry>,
    writer: &mut UnixStream,
) -> Result<bool> {
    let keep_going = !matches!(req, RemoteRequest::Shutdown);
    let events = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .handle(req)?;
    write_events(writer, &events)?;
    Ok(keep_going)
}

fn detach_all(registry: &Mutex<SessionRegistry>) {
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    reg.detach_all();
}

fn flush_poll(registry: &Mutex<SessionRegistry>, writer: &mut UnixStream) -> Result<()> {
    let events = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .poll_events();
    write_events(writer, &events)
}

fn write_events(writer: &mut impl Write, events: &[RemoteEvent]) -> Result<()> {
    for event in events {
        write_frame(writer, event).context("write remote event")?;
    }
    writer.flush().context("flush remote events")?;
    Ok(())
}

/// Drive one request against an in-memory registry (unit / in-process tests).
#[cfg(test)]
pub fn handle_request(
    registry: &mut SessionRegistry,
    req: RemoteRequest,
) -> Result<Vec<RemoteEvent>> {
    let mut events = registry.handle(req)?;
    events.extend(registry.poll_events());
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{try_read_frame_from_buf, write_frame};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

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
                    while let Some(ev) = try_read_frame_from_buf::<RemoteEvent>(&mut buf).unwrap() {
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
}
