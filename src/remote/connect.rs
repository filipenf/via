//! Connect the local GUI to a remote helper (SSH ensure + proxy, or local Unix socket).
//!
//! **Ensure-on-connect:** if the control socket is not accepting connections, start
//! `via --remote-serve` (locally or via `ssh <host> -- via --remote-serve`) before
//! opening the proxy. One helper per host — callers never pick among multiple
//! remote sessions.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::info;

use super::client::RemoteClient;
use super::default_control_socket;

/// How the local GUI reaches the helper.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// SSH destination (`user@host`, Host alias, …). Ignored when `unix_socket` is set.
    pub host: String,
    /// Override control socket on the remote (or local when using unix transport).
    pub socket: Option<PathBuf>,
    /// When set (or host is `local` / env `VIA_REMOTE_SOCKET`), skip SSH and open this socket.
    pub unix_socket: Option<PathBuf>,
}

impl ConnectOptions {
    pub fn from_host(host: &str, socket: Option<PathBuf>) -> Self {
        let unix_from_env = std::env::var_os("VIA_REMOTE_SOCKET").map(PathBuf::from);
        let use_local = host == "local" || host == "unix" || unix_from_env.is_some();
        let unix_socket = if use_local {
            Some(
                socket
                    .clone()
                    .or(unix_from_env)
                    .unwrap_or_else(default_control_socket),
            )
        } else {
            None
        };
        Self {
            host: host.to_string(),
            socket,
            unix_socket,
        }
    }
}

/// Ensure the helper is up and return a live [`RemoteClient`].
pub fn connect(opts: ConnectOptions) -> Result<Arc<RemoteClient>> {
    let wake = Arc::new(AtomicBool::new(false));
    if let Some(socket) = &opts.unix_socket {
        return connect_unix(socket, wake);
    }
    connect_ssh(&opts.host, opts.socket.as_deref(), wake)
}

fn connect_unix(socket: &Path, wake: Arc<AtomicBool>) -> Result<Arc<RemoteClient>> {
    ensure_local_helper(socket)?;
    let stream = wait_connect_unix(socket, 50, Duration::from_millis(100))?;
    info!(socket = %socket.display(), "connected to local remote helper");
    RemoteClient::from_unix_stream(stream, wake)
}

fn ensure_local_helper(socket: &Path) -> Result<()> {
    if socket.exists() {
        // Probe: if connect works, helper is up.
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(socket);
    }
    let exe = std::env::current_exe().context("current_exe for local remote-serve")?;
    let status = Command::new(&exe)
        .arg("--remote-serve")
        .arg("--remote-socket")
        .arg(socket)
        .status()
        .context("start local via --remote-serve")?;
    if !status.success() {
        bail!("via --remote-serve failed with {status}");
    }
    // Daemonize path prints and exits parent immediately; wait for socket.
    wait_connect_unix(socket, 50, Duration::from_millis(100))?;
    Ok(())
}

fn connect_ssh(
    host: &str,
    socket: Option<&Path>,
    wake: Arc<AtomicBool>,
) -> Result<Arc<RemoteClient>> {
    ensure_remote_helper(host, socket)?;
    let mut cmd = ssh_proxy_command(host, socket);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("ssh {host} via --remote-proxy"))?;
    let stdin = child.stdin.take().context("ssh proxy stdin")?;
    let stdout = child.stdout.take().context("ssh proxy stdout")?;
    info!(%host, "connected to remote helper via SSH proxy");
    Ok(RemoteClient::from_stdio(stdin, stdout, child, wake))
}

fn ssh_proxy_command(host: &str, socket: Option<&Path>) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("--")
        .arg("via")
        .arg("--remote-proxy");
    if let Some(socket) = socket {
        cmd.arg("--remote-socket").arg(socket);
    }
    cmd
}

/// Spike ensure rule: if `control.sock` is live, skip starting a new daemon.
fn ensure_remote_helper(host: &str, socket: Option<&Path>) -> Result<()> {
    if remote_helper_alive(host, socket) {
        info!(%host, "remote helper already running");
        return Ok(());
    }
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("--")
        .arg("via")
        .arg("--remote-serve");
    if let Some(socket) = socket {
        cmd.arg("--remote-socket").arg(socket);
    }
    let status = cmd
        .status()
        .with_context(|| format!("ssh {host} via --remote-serve"))?;
    if !status.success() {
        bail!("remote via --remote-serve via ssh failed with {status}");
    }
    // Parent of daemonize exits 0 immediately; give the re-exec a moment, then probe.
    for _ in 0..50 {
        if remote_helper_alive(host, socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("remote helper did not become reachable after via --remote-serve on {host}");
}

/// Probe by opening a short-lived SSH → `--remote-proxy` with stdin closed.
/// Proxy exits 0 after connect+EOF when the daemon is listening; connect failure → non-zero.
fn remote_helper_alive(host: &str, socket: Option<&Path>) -> bool {
    let mut cmd = ssh_proxy_command(host, socket);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(cmd.status(), Ok(status) if status.success())
}

fn wait_connect_unix(
    socket: &Path,
    attempts: u32,
    delay: Duration,
) -> Result<std::os::unix::net::UnixStream> {
    let mut last = None;
    for _ in 0..attempts {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last = Some(err);
                std::thread::sleep(delay);
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("connect failed")))
        .with_context(|| format!("connect remote control socket {}", socket.display()))
}
