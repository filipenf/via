//! Shared test utilities — compiled only under `cfg(test)`, never shipped in
//! the binary. Add `use crate::test_support::*;` in any `#[cfg(test)] mod tests`
//! to reach these helpers.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::session::SessionManifest;
use crate::util::now_millis;

/// Global mutex that serializes tests which mutate process-wide environment
/// variables such as `VIA_SESSION`. `cargo test` runs tests in parallel in the
/// same process, so unsynchronized `std::env::set_var`/`remove_var` calls are
/// racy and can make tests flake. Acquire this lock around any test block that
/// touches the environment.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the global environment mutation lock. Hold the returned guard for the
/// duration of the test (or closure) that sets/unsets environment variables.
pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Create a unique temporary directory under `std::env::temp_dir()`.
///
/// The path is `via-test-<label>-<pid>-<millis>` so parallel tests don't collide.
/// Callers are responsible for cleanup (`std::fs::remove_dir_all`).
pub fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "via-test-{label}-{}-{}",
        std::process::id(),
        now_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a [`SessionManifest`] with a given editor socket path and default
/// fields. No file I/O — use this when the test passes the manifest directly
/// to functions that take `&SessionManifest`.
pub fn session_with_socket(socket: PathBuf) -> SessionManifest {
    SessionManifest {
        pid: std::process::id(),
        cwd: PathBuf::from("/repo"),
        nvim_socket: PathBuf::new(),
        editor_socket: socket,
        agents_dir: PathBuf::new(),
        orchestration_enabled: false,
        workspace_id: None,
        board_id: None,
        started_at_unix: None,
    }
}

/// Write a [`SessionManifest`] to `dir/session.json` so `session::resolve_session()`
/// can find it via `VIA_SESSION`. Also creates `dir/agents/` and a placeholder
/// `dir/nvim.sock` so `SessionManifest::is_live()` returns true.
///
/// Returns the manifest path (set this as `VIA_SESSION` in the test environment).
pub fn write_session_manifest(dir: &Path) -> PathBuf {
    let manifest_path = dir.join("session.json");
    let agents_dir = dir.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let nvim_socket = dir.join("nvim.sock");
    std::fs::write(&nvim_socket, b"").unwrap();
    let manifest = SessionManifest {
        pid: std::process::id(),
        cwd: dir.to_path_buf(),
        nvim_socket,
        editor_socket: PathBuf::new(),
        agents_dir: agents_dir.clone(),
        orchestration_enabled: false,
        workspace_id: None,
        board_id: None,
        started_at_unix: None,
    };
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest_path
}
