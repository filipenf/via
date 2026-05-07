use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspClientInfo {
    pub id: i32,
    pub name: String,
    pub root: String,
    pub languages: Vec<String>,
    pub capabilities_summary: CapabilitiesSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitiesSummary {
    pub definition: bool,
    pub references: bool,
    pub hover: bool,
    pub document_symbol: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum LspBridgeMessage {
    LspClients {
        clients: Vec<LspClientInfo>,
    },
    LspRequest {
        request_id: u64,
        method: String,
        params: serde_json::Value,
        client_id: Option<i32>,
    },
    LspResponse {
        request_id: u64,
        result: Option<serde_json::Value>,
        error: Option<String>,
    },
}

struct OutgoingRequest {
    id: u64,
    method: String,
    params: serde_json::Value,
    client_id: Option<i32>,
}

pub struct LspBridgeState {
    pub clients: Vec<LspClientInfo>,
}

impl Default for LspBridgeState {
    fn default() -> Self {
        Self { clients: vec![] }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct LspBridgeHandle {
    outgoing: mpsc::Sender<OutgoingRequest>,
    state: Arc<Mutex<LspBridgeState>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>,
    next_request_id: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl LspBridgeHandle {
    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<serde_json::Value> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });
        self.send_request("textDocument/definition", params, None).await
    }

    /// Returns the currently known LSP clients attached in Neovim.
    /// The agent can use this to discover available language servers and their capabilities.
    pub async fn clients(&self) -> Vec<LspClientInfo> {
        self.state.lock().await.clients.clone()
    }

    async fn send_request(
        &self,
        method: &str,
        params: serde_json::Value,
        client_id: Option<i32>,
    ) -> Result<serde_json::Value> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = oneshot::channel();

        {
            let mut pend = self.pending.lock().await;
            pend.insert(id, reply_tx);
        }

        let req = OutgoingRequest {
            id,
            method: method.to_owned(),
            params,
            client_id,
        };

        if self.outgoing.send(req).await.is_err() {
            let mut pend = self.pending.lock().await;
            pend.remove(&id);
            anyhow::bail!("lsp bridge disconnected");
        }

        match reply_rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(anyhow::anyhow!("lsp error: {}", e)),
            Err(_) => Err(anyhow::anyhow!("lsp request cancelled")),
        }
    }
}

pub fn spawn_listener(
    socket_path: PathBuf,
    working_directory: PathBuf,
) -> (
    LspBridgeHandle,
    JoinHandle<()>,
    Arc<Mutex<LspBridgeState>>,
    mpsc::Receiver<Vec<LspClientInfo>>,
) {
    let (out_tx, out_rx) = mpsc::channel::<OutgoingRequest>(32);
    let (client_updates_tx, client_updates_rx) = mpsc::channel::<Vec<LspClientInfo>>(8);
    let state = Arc::new(Mutex::new(LspBridgeState::default()));
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));

    let handle = LspBridgeHandle {
        outgoing: out_tx,
        state: state.clone(),
        pending: pending.clone(),
        next_request_id: next_id,
    };

    let state_for_listener = state.clone();
    let listener_handle = tokio::spawn(async move {
        if let Err(error) = run_listener(
            socket_path,
            working_directory,
            out_rx,
            pending,
            state_for_listener,
            client_updates_tx,
        )
        .await
        {
            error!(%error, "lsp bridge listener stopped");
        }
    });

    (handle, listener_handle, state, client_updates_rx)
}

async fn run_listener(
    socket_path: PathBuf,
    _working_directory: PathBuf,
    mut out_rx: mpsc::Receiver<OutgoingRequest>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>,
    state: Arc<Mutex<LspBridgeState>>,
    client_updates: mpsc::Sender<Vec<LspClientInfo>>,
) -> Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).with_context(|| {
            format!("failed to remove stale lsp bridge socket {}", socket_path.display())
        })?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind lsp bridge socket {}", socket_path.display()))?;
    let _socket_file = SocketFile { path: socket_path.clone() };

    info!(socket = %socket_path.display(), "lsp bridge listener ready");

    // Accept the first connection (Lua shim). For MVP we support one active bridge.
    let (stream, _) = listener.accept().await.context("failed to accept lsp bridge connection")?;
    let (reader, writer) = stream.into_split();

    // Spawn reader task (handles incoming lsp_clients and lsp_response)
    let st = state.clone();
    let pend_reader = pending.clone();
    tokio::spawn(async move {
        if let Err(error) = reader_loop(reader, st, pend_reader, client_updates).await {
            debug!(%error, "lsp bridge reader stopped");
        }
    });

    // Spawn writer task that consumes outgoing requests and writes JSON lines
    tokio::spawn(async move {
        if let Err(error) = writer_loop(writer, &mut out_rx).await {
            debug!(%error, "lsp bridge writer stopped");
        }
    });

    // For MVP we don't loop for more connections; one is enough.
    Ok(())
}

async fn reader_loop(
    reader: OwnedReadHalf,
    state: Arc<Mutex<LspBridgeState>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>,
    client_updates: mpsc::Sender<Vec<LspClientInfo>>,
) -> Result<()> {
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await.context("failed to read lsp bridge message")? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LspBridgeMessage>(&line) {
            Ok(LspBridgeMessage::LspClients { clients }) => {
                let mut st = state.lock().await;
                st.clients = clients.clone();
                debug!(count = st.clients.len(), "lsp clients updated");
                if client_updates.try_send(clients).is_err() {
                    debug!("dropping lsp client update");
                }
            }
            Ok(LspBridgeMessage::LspResponse { request_id, result, error }) => {
                let mut pend = pending.lock().await;
                if let Some(sender) = pend.remove(&request_id) {
                    let _ = if let Some(err) = error {
                        sender.send(Err(err))
                    } else {
                        sender.send(Ok(result.unwrap_or(serde_json::Value::Null)))
                    };
                } else {
                    debug!(request_id, "received response for unknown request id");
                }
            }
            Ok(_) => { /* other messages ignored for now */ }
            Err(error) => {
                warn!(%error, message = %line, "invalid lsp bridge message");
            }
        }
    }

    Ok(())
}

async fn writer_loop(
    mut writer: OwnedWriteHalf,
    rx: &mut mpsc::Receiver<OutgoingRequest>,
) -> Result<()> {
    while let Some(req) = rx.recv().await {
        let msg = LspBridgeMessage::LspRequest {
            request_id: req.id,
            method: req.method,
            params: req.params,
            client_id: req.client_id,
        };
        let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
        if writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
    Ok(())
}

struct SocketFile {
    path: PathBuf,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[allow(dead_code)]
pub async fn request_definition(
    handle: &LspBridgeHandle,
    uri: &str,
    line: u32,
    character: u32,
) -> Result<serde_json::Value> {
    handle.definition(uri, line, character).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    #[tokio::test]
    async fn roundtrip_definition_request() {
        let socket_path = std::env::temp_dir().join(format!("spectre-lsp-test-{}.sock", std::process::id()));
        let socket_path_for_sim = socket_path.clone();

        let (handle, _listener, _state, _updates) =
            spawn_listener(socket_path.clone(), std::env::current_dir().unwrap());

        // Simulate the Lua shim: connect, announce clients, then echo back definition responses
        let sim = tokio::spawn(async move {
            // Give listener time to start accepting
            tokio::time::sleep(Duration::from_millis(50)).await;

            let stream = UnixStream::connect(&socket_path_for_sim).await.expect("shim connect");
            let (reader, mut writer) = stream.into_split();

            // Announce a client that supports definition
            let clients_msg = serde_json::json!({
                "type": "lsp_clients",
                "clients": [{
                    "id": 1,
                    "name": "test-lsp",
                    "root": "/tmp",
                    "languages": ["rust"],
                    "capabilities_summary": {
                        "definition": true,
                        "references": false,
                        "hover": false,
                        "document_symbol": false
                    }
                }]
            });
            writer.write_all((serde_json::to_string(&clients_msg).unwrap() + "\n").as_bytes()).await.unwrap();

            // Read requests and reply to definition ones
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(LspBridgeMessage::LspRequest { request_id, method, .. }) = serde_json::from_str::<LspBridgeMessage>(&line) {
                    if method == "textDocument/definition" {
                        let resp = LspBridgeMessage::LspResponse {
                            request_id,
                            result: Some(serde_json::json!([
                                { "uri": "file:///tmp/test.rs", "range": { "start": { "line": 10, "character": 5 }, "end": { "line": 10, "character": 9 } } }
                            ])),
                            error: None,
                        };
                        let line = serde_json::to_string(&resp).unwrap() + "\n";
                        let _ = writer.write_all(line.as_bytes()).await;
                        break; // one request is enough for the test
                    }
                }
            }
        });

        // Now call from the Rust side
        let result = timeout(Duration::from_secs(2), handle.definition("file:///tmp/test.rs", 10, 5)).await;

        assert!(result.is_ok(), "definition call timed out");
        let value = result.unwrap().expect("definition failed");
        assert!(value.is_array());
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["uri"], "file:///tmp/test.rs");

        // cleanup
        let _ = sim.await;
        let _ = std::fs::remove_file(&socket_path);
    }
}