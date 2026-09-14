//! PTY-hosted ACP agent TUI (display + input surface).
//!
//! The mediator owns [`crate::acp::AcpClient`]; this module renders transcript /
//! progress (markdown via `xai-grok-markdown`, prompt via `xai-ratatui-textarea`)
//! and emits submit events over a narrow IPC contract (Unix socket, or stdin JSON
//! lines when stdin is not a TTY). Invoked as `via --acp-tui`.
//! See Obsidian `Spike — ACP TUI binary` / `Spike — ACP TUI grok UI crates (PTY path)`.

mod app;
mod host;
mod protocol;

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};

use crate::agent_bus::{VIA_AGENT_ID_ENV, VIA_AGENT_ROLE_ENV};

use app::{App, InputEvent};
pub use host::{
    AcpTuiBridge, VIA_ACP_TUI_BIN_ENV, resolve_acp_tui_bin, socket_path_for_agent,
    spawn_env_and_args,
};
pub use protocol::{HostToTui, ModelChoice, TranscriptKind, TuiToHost, parse_host_line};

/// Environment variable: path to the per-agent control-plane Unix socket.
pub const VIA_ACP_UI_SOCKET_ENV: &str = "VIA_ACP_UI_SOCKET";

/// Options for [`run`], populated from `via --acp-tui` CLI flags.
#[derive(Debug, Clone, Default)]
pub struct Args {
    /// Agent id (defaults to `$VIA_AGENT_ID`, then `agent`).
    pub agent_id: Option<String>,
    /// Role label for the header (defaults to `$VIA_AGENT_ROLE`, then agent id).
    pub role: Option<String>,
    /// Seed a short demo transcript and accept keyboard-only use (no host required).
    pub demo: bool,
    /// Hide the prompt row (output / scrollback only).
    pub no_input: bool,
    /// Control-plane Unix socket (defaults to `$VIA_ACP_UI_SOCKET`).
    pub socket: Option<PathBuf>,
    /// Start with light appearance (host also pushes [`HostToTui::Appearance`]).
    pub light: bool,
}

/// Entry point for `via --acp-tui` (TTY display surface; no winit).
pub fn run(args: Args) -> Result<()> {
    let agent_id = args
        .agent_id
        .or_else(|| std::env::var(VIA_AGENT_ID_ENV).ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string());
    let role = args
        .role
        .or_else(|| std::env::var(VIA_AGENT_ROLE_ENV).ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| agent_id.clone());
    let socket_path = args
        .socket
        .or_else(|| std::env::var_os(VIA_ACP_UI_SOCKET_ENV).map(PathBuf::from));

    let (host_tx, host_rx) = mpsc::channel::<HostToTui>();
    let mut outbound: Box<dyn Outbound> = Box::new(StderrOutbound);
    let mut ipc_connected = false;

    if let Some(path) = socket_path.as_ref() {
        match connect_socket(path) {
            Ok(stream) => {
                let (reader, writer) = split_stream(stream)?;
                outbound = Box::new(SocketOutbound { writer });
                spawn_line_reader(BufReader::new(reader), host_tx.clone());
                ipc_connected = true;
            }
            Err(err) => {
                // Standalone / early host race: fall back rather than dying immediately.
                eprintln!("via --acp-tui: socket {}: {err:#}", path.display());
            }
        }
    } else if !std::io::stdin().is_terminal() {
        // Host→TUI stub without a socket: JSON lines on stdin; keyboard via /dev/tty (crossterm).
        spawn_stdin_reader(host_tx);
        ipc_connected = true;
    }

    let mut app = App::new(agent_id.clone(), role, !args.no_input, args.light);
    if args.demo {
        app.seed_demo();
    } else if !ipc_connected {
        app.push_system(
            "Waiting for host IPC (set VIA_ACP_UI_SOCKET, pipe JSON on stdin, or pass --demo)."
                .into(),
        );
    }

    outbound.send(&TuiToHost::Ready {
        agent_id: agent_id.clone(),
    })?;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &host_rx, outbound.as_mut());
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    host_rx: &mpsc::Receiver<HostToTui>,
    outbound: &mut dyn Outbound,
) -> Result<()> {
    loop {
        while let Ok(msg) = host_rx.try_recv() {
            if matches!(msg, HostToTui::Shutdown) {
                return Ok(());
            }
            app.apply_host(msg);
        }

        terminal.draw(|frame| app.draw(frame))?;

        if app.should_quit() {
            outbound.send(&TuiToHost::ShutdownAck)?;
            return Ok(());
        }

        if event::poll(Duration::from_millis(50)).context("poll terminal events")? {
            let event = event::read().context("read terminal event")?;
            dispatch_terminal_event(terminal, app, outbound, event)?;
        }

        app.tick_progress();
    }
}

fn dispatch_terminal_event<B>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App,
    outbound: &mut dyn Outbound,
    event: Event,
) -> Result<()>
where
    B: ratatui::backend::Backend,
{
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match app.handle_key(key) {
            InputEvent::None => {}
            InputEvent::Quit => {
                app.request_quit();
            }
            InputEvent::Submit(text) => {
                outbound.send(&TuiToHost::Submit { text })?;
            }
            InputEvent::SetModel(value) => {
                outbound.send(&TuiToHost::SetModel { value })?;
            }
        },
        // PTY relayout/resize delivers SIGWINCH (see `handle_resize`).
        Event::Resize(_, _) => handle_resize(terminal)?,
        _ => {}
    }
    Ok(())
}

trait Outbound: Send {
    fn send(&mut self, msg: &TuiToHost) -> Result<()>;
}

/// Full repaint after the PTY master resizes the pane (SIGWINCH surface as a
/// crossterm `Event::Resize`).
///
/// Without an explicit clear, ratatui diffs against a stale-sized buffer and the
/// next draws only patch part of the new area: truncated mid-line content gets
/// stuck top-left and the pane looks frozen even though the agent keeps working
/// (e.g. window restore after lock screen, or any relayout that resizes panes).
fn handle_resize<B>(terminal: &mut ratatui::Terminal<B>) -> Result<()>
where
    B: ratatui::backend::Backend,
{
    terminal.clear().context("clear terminal after resize")
}

struct StderrOutbound;

impl Outbound for StderrOutbound {
    fn send(&mut self, msg: &TuiToHost) -> Result<()> {
        let line = protocol::encode_tui_line(msg)?;
        let mut out = std::io::stderr().lock();
        writeln!(out, "{line}").context("write TUI outbound line")?;
        out.flush().context("flush TUI outbound")?;
        Ok(())
    }
}

struct SocketOutbound {
    writer: std::os::unix::net::UnixStream,
}

impl Outbound for SocketOutbound {
    fn send(&mut self, msg: &TuiToHost) -> Result<()> {
        let line = protocol::encode_tui_line(msg)?;
        writeln!(self.writer, "{line}").context("write TUI socket line")?;
        self.writer.flush().context("flush TUI socket")?;
        Ok(())
    }
}

fn connect_socket(path: &std::path::Path) -> Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path)
        .with_context(|| format!("connect ACP UI socket {}", path.display()))
}

fn split_stream(
    stream: std::os::unix::net::UnixStream,
) -> Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    let reader = stream
        .try_clone()
        .context("clone ACP UI socket for reader")?;
    Ok((reader, stream))
}

fn spawn_stdin_reader(tx: mpsc::Sender<HostToTui>) {
    thread::Builder::new()
        .name("via-acp-tui-ipc-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            spawn_line_reader_loop(BufReader::new(stdin), &tx);
        })
        .expect("spawn ACP TUI stdin IPC reader");
}

fn spawn_line_reader<R>(reader: R, tx: mpsc::Sender<HostToTui>)
where
    R: BufRead + Send + 'static,
{
    thread::Builder::new()
        .name("via-acp-tui-ipc".into())
        .spawn(move || {
            spawn_line_reader_loop(reader, &tx);
        })
        .expect("spawn ACP TUI IPC reader");
}

fn spawn_line_reader_loop<R>(mut reader: R, tx: &mpsc::Sender<HostToTui>)
where
    R: BufRead,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => match parse_host_line(line.trim()) {
                Ok(Some(msg)) => {
                    let shutdown = matches!(msg, HostToTui::Shutdown);
                    if tx.send(msg).is_err() {
                        break;
                    }
                    if shutdown {
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.send(HostToTui::Transcript {
                        kind: TranscriptKind::System,
                        text: format!("IPC parse error: {err}"),
                    });
                }
            },
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::{Backend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    #[test]
    fn args_demo_defaults() {
        let args = Args {
            agent_id: Some("coder".into()),
            demo: true,
            ..Args::default()
        };
        assert!(args.demo);
        assert_eq!(args.agent_id.as_deref(), Some("coder"));
        assert!(!args.no_input);
        assert!(args.socket.is_none());
    }

    /// Minimal backend that records how often the screen is cleared, so the
    /// resize path can be tested without a real PTY.
    #[derive(Debug, Default)]
    struct ClearSpyBackend {
        size: Size,
        clears: usize,
    }

    impl Backend for ClearSpyBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let _ = content.count();
            Ok(())
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> std::io::Result<Position> {
            Ok(Position::ORIGIN)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, _: P) -> std::io::Result<()> {
            Ok(())
        }

        fn clear(&mut self) -> std::io::Result<()> {
            self.clears += 1;
            Ok(())
        }

        fn size(&self) -> std::io::Result<Size> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> std::io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size::default(),
            })
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sigwinch_resize_issues_full_clear() {
        struct DiscardOutbound;
        impl Outbound for DiscardOutbound {
            fn send(&mut self, _: &TuiToHost) -> Result<()> {
                Ok(())
            }
        }

        let mut terminal = ratatui::Terminal::new(ClearSpyBackend {
            size: Size {
                width: 30,
                height: 8,
            },
            clears: 0,
        })
        .unwrap();
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        let mut outbound = DiscardOutbound;

        // What crossterm delivers for SIGWINCH after a host relayout: without
        // the clear, the next diffed draws leave truncated content stuck and
        // the pane looks frozen while the agent keeps working.
        dispatch_terminal_event(&mut terminal, &mut app, &mut outbound, Event::Resize(30, 8))
            .unwrap();

        assert_eq!(
            terminal.backend().clears,
            1,
            "resize must clear the stale frame so the next draw repaints fully"
        );
    }
}
