//! Durable symbol hints for a repository, shared by every checkout of that repo.
//!
//! The cache key is the shared jj store, else the git common directory, else the
//! canonical working directory when neither VCS is present. Records are paths
//! relative to the checkout that produced them. A stale or missing file still
//! contributes its name; click falls through when the file is not in this tree.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config;
use crate::lsp_bridge::LspBridgeHandle;
use crate::reference_index::IndexedSymbol;
use crate::workspace::sanitize_path_segment;

const CACHE_FILE: &str = "symbols.json";
const MAX_CACHED_SYMBOLS: usize = 8_000;
const MAX_SYMBOLS_PER_QUERY: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepo {
    /// Working tree that is open now. Relative cache paths join onto this.
    pub checkout_root: PathBuf,
    /// Shared identity: jj store, git common dir, or the checkout itself.
    pub cache_key: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    symbols: Vec<CachedSymbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedSymbol {
    name: String,
    kind: u32,
    /// Repo-relative. Paths outside the checkout are not stored.
    path: String,
    line: u32,
}

pub fn resolve_repo(cwd: &Path) -> ResolvedRepo {
    let start = canonicalize_lossy(cwd);
    match find_marker(&start) {
        Some((VcsKind::Jj, marker)) => ResolvedRepo {
            checkout_root: marker.clone(),
            cache_key: jj_store(&marker),
        },
        Some((VcsKind::Git, marker)) => ResolvedRepo {
            checkout_root: marker.clone(),
            cache_key: git_common_dir(&marker),
        },
        None => ResolvedRepo {
            checkout_root: start.clone(),
            cache_key: start,
        },
    }
}

pub fn load_repo(repo: &ResolvedRepo) -> Vec<IndexedSymbol> {
    let path = cache_path(&repo.cache_key);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to read symbol cache");
            return Vec::new();
        }
    };
    let file = match serde_json::from_str::<CacheFile>(&text) {
        Ok(file) => file,
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to parse symbol cache");
            return Vec::new();
        }
    };
    file.symbols
        .into_iter()
        .map(|symbol| IndexedSymbol {
            name: symbol.name,
            kind: symbol.kind,
            path: resolve_cached_path(&repo.checkout_root, &symbol.path),
            line: symbol.line,
        })
        .collect()
}

pub fn store_repo(repo: &ResolvedRepo, symbols: &[IndexedSymbol]) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }
    let path = cache_path(&repo.cache_key);
    let Some(parent) = path.parent() else {
        anyhow::bail!("symbol cache path has no parent");
    };
    fs::create_dir_all(parent)
        .with_context(|| format!("create symbol cache dir {}", parent.display()))?;
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(parent.join("symbols.lock"))
        .with_context(|| format!("open symbol cache lock in {}", parent.display()))?;
    lock_exclusive(&lock_file)?;

    let mut existing = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
        .map(|file| file.symbols)
        .unwrap_or_default();

    let mut changed = false;
    for symbol in symbols {
        let Some(relative) = relativize(&repo.checkout_root, &symbol.path) else {
            continue;
        };
        let record = CachedSymbol {
            name: symbol.name.clone(),
            kind: symbol.kind,
            path: relative,
            line: symbol.line,
        };
        if let Some(index) = existing.iter().position(|item| {
            item.name == record.name && item.path == record.path && item.line == record.line
        }) {
            if existing[index].kind != record.kind {
                let mut updated = existing.remove(index);
                updated.kind = record.kind;
                existing.push(updated);
                changed = true;
            }
        } else {
            existing.push(record);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    if existing.len() > MAX_CACHED_SYMBOLS {
        let drop_count = existing.len() - MAX_CACHED_SYMBOLS;
        existing.drain(0..drop_count);
    }

    let count = existing.len();
    let payload = serde_json::to_string(&CacheFile { symbols: existing })?;
    let tmp = parent.join(format!(
        "symbols.{}.{}.json.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::write(&tmp, payload).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    info!(path = %path.display(), count, "wrote symbol cache");
    Ok(())
}

/// `None` means no language server is connected yet; the caller should retry.
pub async fn query_workspace(lsp: &LspBridgeHandle, query: &str) -> Option<Vec<IndexedSymbol>> {
    let clients = lsp.clients().await;
    if clients.is_empty() {
        return None;
    }
    let targets: Vec<Option<i32>> = clients
        .iter()
        .filter(|client| client.capabilities_summary.workspace_symbol)
        .map(|client| Some(client.id))
        .collect();
    if targets.is_empty() {
        return Some(Vec::new());
    }

    let mut found = Vec::new();
    for client_id in targets {
        let request = lsp.workspace_symbol(query, client_id);
        match tokio::time::timeout(Duration::from_secs(8), request).await {
            Ok(Ok(value)) => found.extend(indexed_symbols_from_workspace_result(&value)),
            Ok(Err(error)) => {
                tracing::debug!(%error, %query, ?client_id, "workspace symbol query failed");
            }
            Err(_) => {
                tracing::debug!(%query, ?client_id, "workspace symbol query timed out");
            }
        }
    }
    retain_exact_symbol_matches(query, &mut found);
    found.truncate(MAX_SYMBOLS_PER_QUERY);
    Some(found)
}

fn retain_exact_symbol_matches(query: &str, found: &mut Vec<IndexedSymbol>) {
    let wanted = query.rsplit(['.', ':', '#']).next().unwrap_or(query);
    found.retain(|symbol| symbol.name == query || symbol.name == wanted);
}

pub fn indexed_symbols_from_workspace_result(value: &serde_json::Value) -> Vec<IndexedSymbol> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(name) = item.get("name").and_then(|name| name.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let kind = item.get("kind").and_then(|kind| kind.as_u64()).unwrap_or(0) as u32;
        if kind == 8 || kind == 13 {
            continue;
        }
        let Some((uri, line)) = symbol_location(item) else {
            continue;
        };
        let Some(path) = file_uri_to_path(&uri) else {
            continue;
        };
        out.push(IndexedSymbol {
            name: name.to_string(),
            kind,
            path,
            line,
        });
    }
    out
}

fn cache_path(cache_key: &Path) -> PathBuf {
    config::via_data_dir()
        .join("repos")
        .join(cache_dir_name(cache_key))
        .join(CACHE_FILE)
}

/// Readable prefix plus a hash of the full key, so different paths that sanitize
/// to the same string do not share a cache, and the name stays within one path segment.
fn cache_dir_name(cache_key: &Path) -> String {
    let raw = cache_key.to_string_lossy();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in raw.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let mut readable = sanitize_path_segment(&raw);
    const PREFIX: usize = 80;
    if readable.len() > PREFIX {
        readable.truncate(PREFIX);
    }
    format!("{readable}-{hash:016x}")
}

fn resolve_cached_path(checkout_root: &Path, stored: &str) -> PathBuf {
    let path = PathBuf::from(stored);
    if path.is_absolute() {
        path
    } else {
        checkout_root.join(path)
    }
}

fn relativize(checkout_root: &Path, path: &Path) -> Option<String> {
    let canonical = canonical_symbol_path(path);
    canonical
        .strip_prefix(checkout_root)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
}

fn canonical_symbol_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(parent) = fs::canonicalize(parent) {
            return parent.join(name);
        }
    }
    path.to_path_buf()
}

fn lock_exclusive(file: &fs::File) -> Result<()> {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("lock symbol cache")
    }
}

#[derive(Clone, Copy)]
enum VcsKind {
    Jj,
    Git,
}

fn find_marker(start: &Path) -> Option<(VcsKind, PathBuf)> {
    let mut dir = start.to_path_buf();
    loop {
        let jj = dir.join(".jj");
        if jj.is_dir() {
            return Some((VcsKind::Jj, dir));
        }
        let git = dir.join(".git");
        if git.is_dir() || git.is_file() {
            return Some((VcsKind::Git, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn jj_store(marker: &Path) -> PathBuf {
    let jj_dir = marker.join(".jj");
    let pointer = jj_dir.join("repo");
    if pointer.is_dir() {
        return canonicalize_lossy(&pointer);
    }
    if let Ok(text) = fs::read_to_string(&pointer) {
        let raw = text.trim();
        if !raw.is_empty() {
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                jj_dir.join(path)
            };
            return canonicalize_lossy(&path);
        }
    }
    canonicalize_lossy(&pointer)
}

fn git_common_dir(marker: &Path) -> PathBuf {
    if let Some(line) = command_stdout(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        marker,
    ) {
        return canonicalize_lossy(Path::new(line.trim()));
    }
    if let Some(line) = command_stdout("git", &["rev-parse", "--git-common-dir"], marker) {
        let path = PathBuf::from(line.trim());
        let path = if path.is_absolute() {
            path
        } else {
            marker.join(path)
        };
        return canonicalize_lossy(&path);
    }
    git_common_dir_from_marker(marker)
}

fn git_common_dir_from_marker(marker: &Path) -> PathBuf {
    let git = marker.join(".git");
    if git.is_dir() {
        return canonicalize_lossy(&git);
    }
    let Ok(text) = fs::read_to_string(&git) else {
        return canonicalize_lossy(&git);
    };
    let Some(gitdir) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
    else {
        return canonicalize_lossy(&git);
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        marker.join(gitdir)
    };
    if let Ok(common) = fs::read_to_string(gitdir.join("commondir")) {
        let raw = common.trim();
        if !raw.is_empty() {
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                gitdir.join(path)
            };
            return canonicalize_lossy(&path);
        }
    }
    canonicalize_lossy(&gitdir)
}

fn command_stdout(program: &str, args: &[&str], cwd: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn symbol_location(item: &serde_json::Value) -> Option<(String, u32)> {
    let location = item.get("location")?;
    if let Some(uri) = location.as_str() {
        return Some((uri.to_string(), 1));
    }
    let uri = location.get("uri")?.as_str()?.to_string();
    let line = location
        .pointer("/range/start/line")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as u32
        + 1;
    Some((uri, line))
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut path = percent_decode(rest);
    if let Some(stripped) = path.strip_prefix("localhost/") {
        path = format!("/{stripped}");
    }
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' && index + 2 < input.len() {
            if let Ok(hex) = std::str::from_utf8(&input[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    bytes.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        bytes.push(input[index]);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_workspace_symbol_locations() {
        let value = json!([
            {
                "name": "SyncWorkflow",
                "kind": 5,
                "location": {
                    "uri": "file:///repo/src/sync.py",
                    "range": { "start": { "line": 9, "character": 0 } }
                }
            },
            { "name": "temp", "kind": 13, "location": { "uri": "file:///repo/src/sync.py" } }
        ]);
        let symbols = indexed_symbols_from_workspace_result(&value);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "SyncWorkflow");
        assert_eq!(symbols[0].path, PathBuf::from("/repo/src/sync.py"));
        assert_eq!(symbols[0].line, 10);
    }

    #[test]
    fn exact_query_match_drops_fuzzy_names() {
        let mut found = vec![
            IndexedSymbol {
                name: "SyncWorkflow".into(),
                kind: 12,
                path: PathBuf::from("/repo/a.rs"),
                line: 1,
            },
            IndexedSymbol {
                name: "SyncWorkflowRunner".into(),
                kind: 12,
                path: PathBuf::from("/repo/a.rs"),
                line: 2,
            },
        ];
        retain_exact_symbol_matches("pkg.SyncWorkflow", &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "SyncWorkflow");
    }

    #[test]
    fn cache_dir_names_do_not_collide_when_sanitized_text_matches() {
        assert_ne!(
            cache_dir_name(Path::new("/a/b_c")),
            cache_dir_name(Path::new("/a_b/c"))
        );
    }

    #[test]
    fn store_writes_symbols_json_once_per_name() {
        let _guard = crate::test_support::env_lock();
        let root = std::env::temp_dir().join(format!(
            "via-symbol-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let previous = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: env_lock serializes process-global env changes.
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        let result = std::panic::catch_unwind(|| {
            let cwd = root.join("proj");
            fs::create_dir_all(cwd.join("src")).unwrap();
            fs::write(cwd.join("src/lib.rs"), "fn sync() {}\n").unwrap();
            let symbol = IndexedSymbol {
                name: "SyncWorkflow".into(),
                kind: 12,
                path: cwd.join("src/lib.rs"),
                line: 4,
            };
            let repo = resolve_repo(&cwd);
            store_repo(&repo, std::slice::from_ref(&symbol)).unwrap();
            let path = cache_path(&repo.cache_key);
            assert!(path.is_file(), "{}", path.display());

            store_repo(
                &repo,
                &[IndexedSymbol {
                    line: 9,
                    ..symbol.clone()
                }],
            )
            .unwrap();
            let updated = fs::read_to_string(&path).unwrap();
            assert_eq!(updated.matches("SyncWorkflow").count(), 2);
            assert!(updated.contains("\"line\":4"));
            assert!(updated.contains("\"line\":9"));
            let loaded = load_repo(&repo);
            assert_eq!(loaded.len(), 2);

            store_repo(
                &repo,
                &[IndexedSymbol {
                    line: 9,
                    ..symbol.clone()
                }],
            )
            .unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            store_repo(&repo, &[IndexedSymbol { line: 9, ..symbol }]).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

            store_repo(
                &repo,
                &[IndexedSymbol {
                    name: "Elsewhere".into(),
                    kind: 12,
                    path: PathBuf::from("/etc/hostname"),
                    line: 1,
                }],
            )
            .unwrap();
            let after_outside = fs::read_to_string(&path).unwrap();
            assert!(!after_outside.contains("Elsewhere"));
        });
        if let Some(previous) = previous {
            unsafe { std::env::set_var("XDG_DATA_HOME", previous) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
        let _ = fs::remove_dir_all(&root);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn directory_without_vcs_uses_that_directory_as_the_cache_key() {
        let dir = std::env::temp_dir().join(format!("via-symbol-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let resolved = resolve_repo(&dir);
        let expected = canonicalize_lossy(&dir);
        assert_eq!(resolved.checkout_root, expected);
        assert_eq!(resolved.cache_key, expected);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jj_workspaces_share_the_store_pointer() {
        let root = std::env::temp_dir().join(format!("via-jj-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let primary = root.join("primary");
        let secondary = root.join("secondary");
        fs::create_dir_all(primary.join(".jj").join("repo")).unwrap();
        fs::create_dir_all(secondary.join(".jj")).unwrap();
        fs::write(secondary.join(".jj").join("repo"), "../../primary/.jj/repo").unwrap();

        let primary_repo = resolve_repo(&primary);
        let secondary_repo = resolve_repo(&secondary);
        assert_eq!(primary_repo.cache_key, secondary_repo.cache_key);
        assert_eq!(
            primary_repo.cache_key,
            canonicalize_lossy(&primary.join(".jj").join("repo"))
        );
        assert_eq!(secondary_repo.checkout_root, canonicalize_lossy(&secondary));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_worktrees_share_the_common_dir() {
        let root = std::env::temp_dir().join(format!("via-git-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let main = root.join("main");
        let wt = root.join("wt");
        if !git(&main, &["init"]) {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        fs::write(main.join("README"), "hi").unwrap();
        if !git(&main, &["add", "README"]) || !git(&main, &["commit", "-m", "init"]) {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        if !git(
            &main,
            &["worktree", "add", "--detach", wt.to_str().unwrap()],
        ) {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let main_repo = resolve_repo(&main);
        let wt_repo = resolve_repo(&wt);
        assert_eq!(main_repo.cache_key, wt_repo.cache_key);
        assert_ne!(main_repo.checkout_root, wt_repo.checkout_root);
        let _ = fs::remove_dir_all(&root);
    }

    fn git(cwd: &Path, args: &[&str]) -> bool {
        fs::create_dir_all(cwd).ok();
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "via")
            .env("GIT_AUTHOR_EMAIL", "via@example.com")
            .env("GIT_COMMITTER_NAME", "via")
            .env("GIT_COMMITTER_EMAIL", "via@example.com")
            .output()
            .is_ok_and(|output| output.status.success())
    }
}
