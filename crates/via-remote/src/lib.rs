//! Standalone remote execution helper for via.
//!
//! Detachable PTY / ACP-stdio sessions over a via-owned Unix control socket.
//! Ships as a small standalone binary (`via-remote serve` / `via-remote proxy`)
//! so headless hosts never need the full via GUI dependency tree
//! (libghostty-vt, winit, softbuffer, …).
//!
//! The [`protocol`] module defines the shared host ↔ helper control-plane
//! messages; [`pty`] provides the portable-pty-backed session; [`registry`]
//! owns the in-process session table; [`serve`] is the control-socket daemon
//! and [`proxy`] bridges stdio to that socket for SSH pipes.

pub mod protocol;
pub mod pty;
pub mod registry;
pub mod scrollback;
pub mod serve;

/// stdio ↔ control socket bridge (SSH pipe target).
pub mod proxy;

pub use proxy::ProxyArgs;
pub use serve::{ServeArgs, VIA_REMOTE_FOREGROUND_ENV};

/// Default control socket: `$XDG_DATA_HOME/via/remote/control.sock`.
pub fn default_control_socket() -> std::path::PathBuf {
    via_data_dir().join("remote").join("control.sock")
}

/// via-remote's data directory: `$XDG_DATA_HOME/via`, falling back to
/// `$HOME/.local/share/via`, then the system temp dir. Mirrors the client-side
/// layout so client and helper always agree on the default socket path.
pub fn via_data_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        let dir = std::path::PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("via");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home).join(".local/share/via");
    }
    std::env::temp_dir().join("via")
}
