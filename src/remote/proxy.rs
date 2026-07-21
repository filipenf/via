//! Foreground stdio ↔ remote helper control socket bridge (`via --remote-proxy`).

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct ProxyArgs {
    pub socket: PathBuf,
}

pub fn run(args: ProxyArgs) -> Result<()> {
    let stream = connect_with_retry(&args.socket, 50, Duration::from_millis(100))?;
    stream
        .set_read_timeout(None)
        .context("clear proxy read timeout")?;

    let mut sock_read = stream
        .try_clone()
        .context("clone control socket for proxy read")?;
    let mut sock_write = stream;

    let stdin_to_sock = thread::Builder::new()
        .name("via-remote-proxy-in".into())
        .spawn(move || -> Result<()> {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 65536];
            loop {
                let n = stdin
                    .read(&mut buf)
                    .context("read stdin for remote proxy")?;
                if n == 0 {
                    break;
                }
                sock_write
                    .write_all(&buf[..n])
                    .context("write stdin to control socket")?;
                sock_write.flush().ok();
            }
            // Half-close so the helper sees EOF and ends the client session.
            let _ = sock_write.shutdown(std::net::Shutdown::Write);
            Ok(())
        })
        .context("spawn proxy stdin thread")?;

    // stdout ← socket on this thread
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 65536];
    loop {
        match sock_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout
                    .write_all(&buf[..n])
                    .context("write control socket to stdout")?;
                stdout.flush().ok();
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("read control socket in proxy"),
        }
    }

    match stdin_to_sock.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("remote proxy stdin thread panicked"),
    }
}

fn connect_with_retry(socket: &PathBuf, attempts: u32, delay: Duration) -> Result<UnixStream> {
    let mut last = None;
    for _ in 0..attempts {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last = Some(err);
                thread::sleep(delay);
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("connect failed")))
        .with_context(|| format!("connect remote control socket {}", socket.display()))
}
