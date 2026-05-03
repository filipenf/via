use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, unbounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tracing::{debug, info};

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    output: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl PtySession {
    pub fn spawn_with_args<I, S>(
        command: &str,
        args: I,
        cwd: &Path,
        size: TerminalSize,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.into())
            .context("failed to open PTY")?;

        let mut command = CommandBuilder::new(command);
        for arg in args {
            command.arg(arg);
        }
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(command)
            .context("failed to spawn terminal command")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;
        let (output_tx, output_rx) = unbounded();

        let reader_thread = thread::spawn(move || {
            let mut buffer = [0; 8192];

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if output_tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        debug!(%error, "PTY reader stopped");
                        break;
                    }
                }
            }
        });

        info!(pid = ?child.process_id(), "spawned PTY child");

        Ok(Self {
            master: pair.master,
            writer,
            child,
            output: output_rx,
            reader: Some(reader_thread),
        })
    }

    pub fn output(&self) -> &Receiver<Vec<u8>> {
        &self.output
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write to PTY")?;
        self.writer.flush().context("failed to flush PTY writer")
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        self.master
            .resize(size.into())
            .context("failed to resize PTY")
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        }
    }
}
