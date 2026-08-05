//! Remote execution client: attach the local GUI to a remote helper.
//!
//! The helper itself (`via-remote serve` / `via-remote proxy`) lives in the
//! standalone [`via_remote`] crate so headless hosts never build the GUI. This
//! module keeps only the client side: [`RemoteClient`] (length-prefixed CBOR
//! control connection) and [`connect`] (SSH / local-socket ensure + attach).
//!
//! Local GUI attach (one helper per host; no session picker):
//! - `via --remote <host>` (primary) or `via remote <host>` (alias)
//! - Connect ensures the helper is up, then attaches. GUI quit = Detach.
//!
//! See Obsidian `Spike — Remote execution` / `Spec — Remote execution`.

mod client;
mod connect;

use std::path::PathBuf;

pub use client::{PtySpawnOpts, RemoteClient, RemotePane};
pub use connect::{ConnectOptions, connect};

/// Client-side control socket path. The default is owned by [`via_remote`], so
/// client and helper agree unless `VIA_REMOTE_SOCKET` / `--remote-socket`
/// overrides it.
pub fn default_control_socket() -> PathBuf {
    via_remote::default_control_socket()
}
