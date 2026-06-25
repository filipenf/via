//! The "agent bus": discovery + messaging primitives shared by agents running inside via.
//!
//! Two file-backed surfaces live under the per-process `agents_dir` (resolved from the
//! `VIA_SESSION` manifest):
//!
//! - `registry.json`: the list of known agent panes, written by the UI on spawn. This is the
//!   discovery surface (`via agent list`).
//! - `inbox/<id>/<seq>.json`: per-recipient mailbox files under the session `agents/` directory
//!   (e.g. `via-<pid>/agents/inbox/reviewer/…`). `via agent send` appends one file per message;
//!   `via agent inbox` drains it. One file per message avoids append races and makes "consume" a
//!   simple delete.
//!
//! The mailbox is the source of truth for inter-agent messages; the optional pane notification
//! (a one-line ping written via the editor socket) is a convenience on top.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variable carrying an agent's own identity inside its pane.
pub const VIA_AGENT_ID_ENV: &str = "VIA_AGENT_ID";
/// Environment variable carrying an agent's role label inside its pane.
pub const VIA_AGENT_ROLE_ENV: &str = "VIA_AGENT_ROLE";

const REGISTRY_FILE: &str = "registry.json";
const INBOX_DIR: &str = "inbox";

/// A single agent pane known to via.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    /// True for the primary/orchestrator pane.
    #[serde(default)]
    pub primary: bool,
}

/// A message addressed to an agent's mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Sender id (`VIA_AGENT_ID`, or `"unknown"` when unset).
    pub from: String,
    /// Recipient id.
    pub to: String,
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// Free-form message body.
    pub text: String,
}

/// Path to the registry file inside an agents directory.
pub fn registry_path(agents_dir: &Path) -> PathBuf {
    agents_dir.join(REGISTRY_FILE)
}

fn inbox_dir(agents_dir: &Path, id: &str) -> PathBuf {
    agents_dir.join(INBOX_DIR).join(sanitize_id(id))
}

/// Read the agent registry. Returns an empty list when the file is absent.
pub fn read_registry(agents_dir: &Path) -> Result<Vec<AgentRecord>> {
    let path = registry_path(agents_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("parse agent registry {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("read agent registry {}", path.display())),
    }
}

/// Atomically (re)write the agent registry.
pub fn write_registry(agents_dir: &Path, records: &[AgentRecord]) -> Result<()> {
    let path = registry_path(agents_dir);
    let serialized = serde_json::to_vec_pretty(records).context("serialize agent registry")?;
    write_atomic(&path, &serialized)
}

/// Append a message to a recipient's mailbox spool.
pub fn enqueue(agents_dir: &Path, message: &Message) -> Result<PathBuf> {
    let dir = inbox_dir(agents_dir, &message.to);
    let path = dir.join(unique_message_filename(message.ts));
    let serialized = serde_json::to_vec(message).context("serialize message")?;
    write_atomic(&path, &serialized)?;
    Ok(path)
}

/// Read (and, unless `peek`, remove) all pending messages for `id`, ordered oldest-first.
pub fn drain_inbox(agents_dir: &Path, id: &str, peek: bool) -> Result<Vec<Message>> {
    let dir = inbox_dir(agents_dir, id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read inbox {}", dir.display())),
    };

    // Sort by filename: the timestamp-prefixed names are lexicographically chronological.
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();

    let mut messages = Vec::with_capacity(files.len());
    for path in files {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read message {}", path.display()))?;
        match serde_json::from_str::<Message>(&contents) {
            Ok(message) => {
                messages.push(message);
                if !peek {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove message {}", path.display()))?;
                }
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "skipping unparseable inbox message");
                if !peek {
                    if let Err(remove_err) = std::fs::remove_file(&path) {
                        tracing::warn!(
                            path = %path.display(),
                            %remove_err,
                            "failed to remove corrupt inbox message"
                        );
                    }
                }
            }
        }
    }

    Ok(messages)
}

/// Send a single newline-delimited JSON line to the editor Unix socket (fire-and-forget),
/// mirroring what `nvim/via.lua` does. Used for `spawn`/`send` pane notifications.
#[cfg(unix)]
pub fn notify_editor_socket(socket_path: &Path, payload: &serde_json::Value) -> Result<()> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect editor socket {}", socket_path.display()))?;
    let mut line = serde_json::to_vec(payload).context("serialize editor event")?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .context("write editor event to socket")?;
    stream.flush().context("flush editor socket")?;
    Ok(())
}

#[cfg(not(unix))]
pub fn notify_editor_socket(_socket_path: &Path, _payload: &serde_json::Value) -> Result<()> {
    anyhow::bail!("editor socket notifications are only supported on Unix")
}

fn unique_message_filename(ts_millis: u64) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{ts_millis:013}-{}-{seq:06}.json", std::process::id())
}

/// Keep ids filesystem-safe so a malicious or sloppy id cannot escape the agents dir.
fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized.starts_with('.') {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "via-agent-bus-{}-{}-{}",
            label,
            std::process::id(),
            crate::util::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn registry_round_trips() {
        let dir = temp_dir("registry");
        let records = vec![
            AgentRecord {
                id: "orchestrator".to_string(),
                role: Some("orchestrator".to_string()),
                command: Some("opencode acp".to_string()),
                primary: true,
            },
            AgentRecord {
                id: "reviewer".to_string(),
                role: Some("reviewer".to_string()),
                command: None,
                primary: false,
            },
        ];
        write_registry(&dir, &records).unwrap();
        assert_eq!(read_registry(&dir).unwrap(), records);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_registry_missing_is_empty() {
        let dir = temp_dir("registry-missing");
        assert!(read_registry(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enqueue_and_drain_preserves_order() {
        let dir = temp_dir("inbox");
        for i in 0..3 {
            enqueue(
                &dir,
                &Message {
                    from: "orchestrator".to_string(),
                    to: "reviewer".to_string(),
                    ts: 1_000 + i,
                    text: format!("message {i}"),
                },
            )
            .unwrap();
        }

        // Peek does not consume.
        let peeked = drain_inbox(&dir, "reviewer", true).unwrap();
        assert_eq!(peeked.len(), 3);
        assert_eq!(peeked[0].text, "message 0");
        assert_eq!(peeked[2].text, "message 2");

        // Drain consumes; a second drain is empty.
        let drained = drain_inbox(&dir, "reviewer", false).unwrap();
        assert_eq!(drained.len(), 3);
        assert!(drain_inbox(&dir, "reviewer", false).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enqueue_writes_under_inbox_subdir() {
        let dir = temp_dir("inbox-layout");
        let path = enqueue(
            &dir,
            &Message {
                from: "orchestrator".to_string(),
                to: "reviewer".to_string(),
                ts: 42,
                text: "hello".to_string(),
            },
        )
        .unwrap();
        assert!(
            path.starts_with(dir.join("inbox").join("reviewer")),
            "expected inbox/reviewer layout, got {}",
            path.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drain_unknown_recipient_is_empty() {
        let dir = temp_dir("inbox-unknown");
        assert!(drain_inbox(&dir, "nobody", false).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drain_removes_corrupt_messages_when_not_peeking() {
        let dir = temp_dir("inbox-corrupt");
        let inbox = dir.join("inbox").join("reviewer");
        std::fs::create_dir_all(&inbox).unwrap();
        let bad = inbox.join("0000000000042-bad.json");
        std::fs::write(&bad, b"not json").unwrap();

        assert!(drain_inbox(&dir, "reviewer", true).unwrap().is_empty());
        assert!(bad.exists());

        assert!(drain_inbox(&dir, "reviewer", false).unwrap().is_empty());
        assert!(!bad.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitize_id_blocks_traversal() {
        assert_eq!(sanitize_id("../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_id("reviewer"), "reviewer");
        assert_eq!(sanitize_id(""), "_");
    }
}
