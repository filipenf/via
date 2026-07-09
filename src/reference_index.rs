use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::nvim::FileTarget;

/// One document-symbol location from an open buffer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexedSymbol {
    pub name: String,
    pub kind: u32,
    pub path: PathBuf,
    /// 1-based line (matches Neovim / FileTarget).
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLoc {
    pub path: PathBuf,
    pub line: u32,
    pub kind: u32,
}

/// Snapshot of known files + open-buffer symbols for Ctrl-held cue scoring and click resolution.
///
/// Built from Neovim open buffers + VCS changed paths + document symbols. Always partial —
/// treat as a ranking signal, not a hard filter. File and symbol snapshots update independently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceIndex {
    pub buffers: HashSet<PathBuf>,
    pub basenames: HashMap<String, Vec<PathBuf>>,
    pub vcs_working_tree: HashSet<PathBuf>,
    pub vcs_branch: HashSet<PathBuf>,
    pub symbols_by_name: HashMap<String, Vec<SymbolLoc>>,
}

impl ReferenceIndex {
    pub fn from_parts(
        buffers: impl IntoIterator<Item = PathBuf>,
        vcs_working_tree: impl IntoIterator<Item = PathBuf>,
        vcs_branch: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let mut index = Self::default();
        index.set_files(buffers, vcs_working_tree, vcs_branch);
        index
    }

    /// Replace file paths; preserve existing symbol map.
    pub fn set_files(
        &mut self,
        buffers: impl IntoIterator<Item = PathBuf>,
        vcs_working_tree: impl IntoIterator<Item = PathBuf>,
        vcs_branch: impl IntoIterator<Item = PathBuf>,
    ) {
        self.buffers = buffers.into_iter().collect();
        self.vcs_working_tree = vcs_working_tree.into_iter().collect();
        self.vcs_branch = vcs_branch.into_iter().collect();

        let mut basenames: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in self
            .buffers
            .iter()
            .chain(self.vcs_working_tree.iter())
            .chain(self.vcs_branch.iter())
        {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let entry = basenames.entry(name.to_string()).or_default();
                if !entry.iter().any(|p| p == path) {
                    entry.push(path.clone());
                }
            }
        }
        self.basenames = basenames;
    }

    /// Replace symbol map; preserve existing file paths.
    pub fn set_symbols(&mut self, symbols: impl IntoIterator<Item = IndexedSymbol>) {
        let mut symbols_by_name: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
        for sym in symbols {
            let loc = SymbolLoc {
                path: sym.path,
                line: sym.line,
                kind: sym.kind,
            };
            let entry = symbols_by_name.entry(sym.name).or_default();
            if !entry.iter().any(|existing| {
                existing.path == loc.path && existing.line == loc.line && existing.kind == loc.kind
            }) {
                entry.push(loc);
            }
        }
        self.symbols_by_name = symbols_by_name;
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
            && self.vcs_working_tree.is_empty()
            && self.vcs_branch.is_empty()
            && self.symbols_by_name.is_empty()
    }

    /// Unique absolute path for a bare basename, if the index has exactly one candidate.
    pub fn unique_path_for_basename(&self, basename: &str) -> Option<PathBuf> {
        let paths = self.paths_for_basename(basename);
        if paths.len() == 1 {
            Some(paths[0].clone())
        } else {
            None
        }
    }

    /// All indexed absolute paths for a basename (empty if unknown).
    pub fn paths_for_basename(&self, basename: &str) -> &[PathBuf] {
        self.basenames
            .get(basename)
            .map(|paths| paths.as_slice())
            .unwrap_or(&[])
    }

    pub fn contains_basename(&self, basename: &str) -> bool {
        self.basenames.contains_key(basename)
    }

    /// When the basename is ambiguous in the index, return candidates for Lua.
    ///
    /// Does **not** unique-rewrite: cue-time [`Self::file_target_for_token`] already
    /// rewrites bare basenames, and path-shaped opens must keep their concrete path
    /// (e.g. `vendor/main.rs` must not become a unique indexed `src/main.rs`).
    /// Cold / unique / unknown basenames leave `target` unchanged and return no candidates.
    pub fn resolve_open_from_index(
        &self,
        path: PathBuf,
        line: Option<u32>,
    ) -> (FileTarget, Vec<PathBuf>) {
        let Some(basename) = path.file_name().and_then(|n| n.to_str()) else {
            return (FileTarget { path, line }, Vec::new());
        };

        let paths = self.paths_for_basename(basename);
        if paths.len() > 1 {
            return (FileTarget { path, line }, paths.to_vec());
        }

        (FileTarget { path, line }, Vec::new())
    }

    pub fn contains_symbol(&self, name: &str) -> bool {
        self.symbols_by_name.contains_key(name)
    }

    /// Unique symbol location if the index has exactly one candidate for `name`.
    pub fn unique_symbol(&self, name: &str) -> Option<&SymbolLoc> {
        let locs = self.symbols_by_name.get(name)?;
        if locs.len() == 1 {
            Some(&locs[0])
        } else {
            None
        }
    }

    /// Score a file reference for cue eligibility / ranking.
    /// Higher is better. Heuristic-only baseline is low; index hits raise score.
    pub fn score_file(&self, path: &Path, token_has_path_shape: bool) -> i32 {
        let mut score = 0i32;
        if token_has_path_shape {
            score += 10;
        }
        if self.buffers.contains(path) {
            score += 50;
        }
        if self.vcs_working_tree.contains(path) {
            score += 30;
        } else if self.vcs_branch.contains(path) {
            score += 20;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(paths) = self.basenames.get(name) {
                if paths.len() == 1 {
                    score += 40;
                } else if paths.iter().any(|p| p == path) {
                    score += 15;
                } else {
                    score += 5;
                }
            }
        }
        score
    }

    pub fn score_symbol(&self, name: &str) -> i32 {
        let Some(locs) = self.symbols_by_name.get(name) else {
            return 0;
        };
        let mut score = 20;
        if locs.len() == 1 {
            score += 40;
        } else {
            score += 10;
        }
        if symbol_token_is_strong(name) {
            score += 15;
        }
        score
    }

    /// Whether a scanned token should become a file cue.
    pub fn should_cue_file_token(&self, token: &str) -> bool {
        if token_has_file_shape(token) {
            return true;
        }
        // Bare basename (no path separators): only if known in index.
        let basename = token_basename(token);
        self.contains_basename(basename)
    }

    /// Whether a scanned token should become a symbol cue.
    ///
    /// Shape-qualified tokens (`::` / `#`) always cue. Bare identifiers cue only when
    /// present in the index and strong enough (length ≥ 3, `_`, or qualified with `.`/`::`/`#`).
    pub fn should_cue_symbol_token(&self, token: &str) -> bool {
        if looks_like_scanned_symbol_shape(token) {
            return true;
        }
        if !symbol_token_is_strong(token) {
            return false;
        }
        self.contains_symbol(token)
    }

    /// Resolve a token to a FileTarget, rewriting unique bare basenames via the index.
    pub fn file_target_for_token(&self, token: &str, working_directory: &Path) -> FileTarget {
        let path_part = token_path_part(token);
        if path_part.contains('/') || path_part.contains('\\') {
            return FileTarget::parse(token, working_directory);
        }

        let basename = token_basename(token);
        let line = FileTarget::parse(token, working_directory).line;

        if let Some(path) = self.unique_path_for_basename(basename) {
            return FileTarget { path, line };
        }

        FileTarget::parse(token, working_directory)
    }

    /// Resolve an indexed symbol to a FileTarget when unique; otherwise None.
    pub fn file_target_for_symbol(&self, name: &str) -> Option<FileTarget> {
        let loc = self.unique_symbol(name)?;
        Some(FileTarget {
            path: loc.path.clone(),
            line: Some(loc.line),
        })
    }
}

pub fn token_has_file_shape(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || token.contains('.')
        || token
            .rsplit_once(':')
            .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
}

/// Strength gate for bare symbol cues (no uppercase bias).
pub fn symbol_token_is_strong(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.chars().count() >= 3 {
        return true;
    }
    if token.contains('_') {
        return true;
    }
    // Short but already qualified (rare): allow.
    looks_like_qualified_symbol(token)
}

/// Cold-index shape rule matching historical `looks_like_scanned_symbol` (`::` or `#`).
fn looks_like_scanned_symbol_shape(token: &str) -> bool {
    token.contains("::") || token.contains('#')
}

fn looks_like_qualified_symbol(token: &str) -> bool {
    token.contains("::") || token.contains('#') || token.contains('.')
}

fn token_path_part(token: &str) -> &str {
    // Strip trailing :line / :line-range for basename checks, matching FileTarget::parse.
    if let Some((path, last)) = token.rsplit_once(':') {
        if last.parse::<u32>().is_ok() || last.contains('-') {
            return path;
        }
    }
    token
}

fn token_basename(token: &str) -> &str {
    let path_part = token_path_part(token);
    Path::new(path_part)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path_part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> ReferenceIndex {
        ReferenceIndex::from_parts(
            [
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/src/lib.rs"),
            ],
            [PathBuf::from("/repo/src/main.rs")],
            [PathBuf::from("/repo/src/editor.rs")],
        )
    }

    fn symbol(name: &str, path: &str, line: u32) -> IndexedSymbol {
        IndexedSymbol {
            name: name.to_string(),
            kind: 12, // Function
            path: PathBuf::from(path),
            line,
        }
    }

    #[test]
    fn builds_basename_map() {
        let idx = index();
        assert!(idx.contains_basename("main.rs"));
        assert!(idx.contains_basename("editor.rs"));
        assert_eq!(
            idx.unique_path_for_basename("main.rs"),
            Some(PathBuf::from("/repo/src/main.rs"))
        );
    }

    #[test]
    fn cues_bare_basename_when_indexed() {
        let idx = index();
        assert!(idx.should_cue_file_token("main.rs"));
    }

    #[test]
    fn does_not_cue_unknown_extensionless_basename() {
        let idx = index();
        assert!(!idx.should_cue_file_token("Makefile"));
        assert!(!idx.should_cue_file_token("LICENSE"));
    }

    #[test]
    fn cues_extension_basename_via_shape_even_when_unknown() {
        // Shape-based: keep cueing `*.rs` etc. even if index is cold.
        let idx = ReferenceIndex::default();
        assert!(idx.should_cue_file_token("unknown.rs"));
    }

    #[test]
    fn always_cues_path_shaped_tokens() {
        let idx = ReferenceIndex::default();
        assert!(idx.should_cue_file_token("src/new_file.rs"));
    }

    #[test]
    fn resolves_unique_bare_basename() {
        let idx = index();
        let target = idx.file_target_for_token("main.rs", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
    }

    #[test]
    fn resolves_unique_bare_basename_with_line() {
        let idx = index();
        let target = idx.file_target_for_token("main.rs:42", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(target.line, Some(42));
    }

    #[test]
    fn ambiguous_bare_basename_falls_back_to_relative_target() {
        let idx = ReferenceIndex::from_parts(
            [PathBuf::from("/repo/src/main.rs")],
            [PathBuf::from("/repo/tests/main.rs")],
            [],
        );

        let target = idx.file_target_for_token("main.rs:42", Path::new("/repo"));

        assert_eq!(target.path, PathBuf::from("/repo/main.rs"));
        assert_eq!(target.line, Some(42));
    }

    #[test]
    fn paths_for_basename_returns_all_candidates() {
        let idx = ReferenceIndex::from_parts(
            [PathBuf::from("/repo/src/main.rs")],
            [PathBuf::from("/repo/tests/main.rs")],
            [],
        );
        let paths = idx.paths_for_basename("main.rs");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&PathBuf::from("/repo/src/main.rs")));
        assert!(paths.contains(&PathBuf::from("/repo/tests/main.rs")));
        assert!(idx.paths_for_basename("missing.rs").is_empty());
    }

    #[test]
    fn resolve_open_unique_basename_keeps_path() {
        let idx = index();
        let (target, candidates) =
            idx.resolve_open_from_index(PathBuf::from("/repo/main.rs"), Some(7));
        assert_eq!(target.path, PathBuf::from("/repo/main.rs"));
        assert_eq!(target.line, Some(7));
        assert!(candidates.is_empty());
    }

    #[test]
    fn resolve_open_path_shaped_does_not_steal_unique_indexed_basename() {
        let idx = index();
        // Index has unique `/repo/src/main.rs`; a path-shaped click must stay put.
        let (target, candidates) =
            idx.resolve_open_from_index(PathBuf::from("/repo/other/main.rs"), Some(9));
        assert_eq!(target.path, PathBuf::from("/repo/other/main.rs"));
        assert_eq!(target.line, Some(9));
        assert!(candidates.is_empty());

        let (relative, relative_candidates) =
            idx.resolve_open_from_index(PathBuf::from("vendor/main.rs"), None);
        assert_eq!(relative.path, PathBuf::from("vendor/main.rs"));
        assert!(relative_candidates.is_empty());
    }

    #[test]
    fn resolve_open_returns_ambiguous_candidates() {
        let idx = ReferenceIndex::from_parts(
            [PathBuf::from("/repo/src/main.rs")],
            [PathBuf::from("/repo/tests/main.rs")],
            [],
        );
        let (target, candidates) =
            idx.resolve_open_from_index(PathBuf::from("/repo/main.rs"), Some(3));
        assert_eq!(target.path, PathBuf::from("/repo/main.rs"));
        assert_eq!(target.line, Some(3));
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn resolve_open_unknown_basename_has_no_candidates() {
        let idx = index();
        let (target, candidates) =
            idx.resolve_open_from_index(PathBuf::from("/repo/unknown.rs"), None);
        assert_eq!(target.path, PathBuf::from("/repo/unknown.rs"));
        assert!(candidates.is_empty());
    }

    #[test]
    fn scores_buffer_paths_highest() {
        let idx = index();
        let buffer = Path::new("/repo/src/main.rs");
        let branch = Path::new("/repo/src/editor.rs");
        assert!(idx.score_file(buffer, true) > idx.score_file(branch, true));
    }

    #[test]
    fn set_symbols_preserves_files() {
        let mut idx = index();
        idx.set_symbols([symbol("parse_event", "/repo/src/main.rs", 10)]);
        assert!(idx.contains_basename("main.rs"));
        assert!(idx.contains_symbol("parse_event"));
    }

    #[test]
    fn set_files_preserves_symbols() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("parse_event", "/repo/src/main.rs", 10)]);
        idx.set_files([PathBuf::from("/repo/src/lib.rs")], [], []);
        assert!(idx.contains_symbol("parse_event"));
        assert!(idx.contains_basename("lib.rs"));
    }

    #[test]
    fn unique_symbol_resolves_to_file_target() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("parse_event", "/repo/src/main.rs", 10)]);
        let target = idx.file_target_for_symbol("parse_event").unwrap();
        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(target.line, Some(10));
    }

    #[test]
    fn ambiguous_symbol_has_no_unique_target() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([
            symbol("parse", "/repo/src/a.rs", 1),
            symbol("parse", "/repo/src/b.rs", 2),
        ]);
        assert!(idx.file_target_for_symbol("parse").is_none());
        assert!(idx.contains_symbol("parse"));
    }

    #[test]
    fn strength_gate_rejects_short_unqualified_names() {
        assert!(!symbol_token_is_strong("i"));
        assert!(!symbol_token_is_strong("ab"));
        assert!(symbol_token_is_strong("abc"));
        assert!(symbol_token_is_strong("a_b"));
        assert!(symbol_token_is_strong("a::b"));
    }

    #[test]
    fn should_cue_symbol_requires_index_for_bare_ids() {
        let mut idx = ReferenceIndex::default();
        assert!(!idx.should_cue_symbol_token("parse"));
        idx.set_symbols([symbol("parse", "/repo/src/main.rs", 10)]);
        assert!(idx.should_cue_symbol_token("parse"));
        // Qualified still cues with cold index.
        let cold = ReferenceIndex::default();
        assert!(cold.should_cue_symbol_token("Foo::bar"));
    }

    #[test]
    fn short_indexed_name_without_strength_does_not_cue() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("ab", "/repo/src/main.rs", 10)]);
        assert!(!idx.should_cue_symbol_token("ab"));
    }
}
