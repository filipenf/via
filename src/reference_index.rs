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
    /// Cached hints whose file is absent in this checkout must not open an empty buffer.
    pub require_file: bool,
}

enum SymbolLookup {
    Unique(FileTarget),
    Ambiguous,
    Absent,
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
    /// Combined lookup map: open-buffer symbols layered over the repo cache.
    pub symbols_by_name: HashMap<String, Vec<SymbolLoc>>,
    buffer_symbols: HashMap<String, Vec<SymbolLoc>>,
    cached_symbols: HashMap<String, Vec<SymbolLoc>>,
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

    /// Replace open-buffer symbols; preserve the repo symbol cache and file paths.
    pub fn set_symbols(&mut self, symbols: impl IntoIterator<Item = IndexedSymbol>) {
        self.buffer_symbols = group_symbols(symbols, false);
        self.rebuild_symbol_maps();
    }

    /// Merge repo-cache hints. A later open-buffer snapshot for the same path wins.
    pub fn merge_cached_symbols(&mut self, symbols: impl IntoIterator<Item = IndexedSymbol>) {
        for (name, locs) in group_symbols(symbols, true) {
            let entry = self.cached_symbols.entry(name).or_default();
            for loc in locs {
                if !entry
                    .iter()
                    .any(|existing| existing.path == loc.path && existing.line == loc.line)
                {
                    entry.push(loc);
                }
            }
        }
        self.rebuild_symbol_maps();
    }

    fn rebuild_symbol_maps(&mut self) {
        let mut combined = self.cached_symbols.clone();
        for (name, locs) in &self.buffer_symbols {
            let entry = combined.entry(name.clone()).or_default();
            let buffer_paths: HashSet<&PathBuf> = locs.iter().map(|loc| &loc.path).collect();
            entry.retain(|existing| !buffer_paths.contains(&existing.path));
            entry.extend(locs.iter().cloned());
        }
        self.symbols_by_name = combined;
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
    ///
    /// Leading-truncated paths (`...` / `…`) use longest path-suffix match first; when
    /// ambiguous, open-buffer hits are preferred over the full suffix set.
    pub fn resolve_open_from_index(
        &self,
        path: PathBuf,
        line: Option<u32>,
    ) -> (FileTarget, Vec<PathBuf>) {
        if let Some(resolved) = self.resolve_truncated(&path, line) {
            return resolved;
        }

        let Some(basename) = path.file_name().and_then(|n| n.to_str()) else {
            return (FileTarget { path, line }, Vec::new());
        };

        let paths = self.paths_for_basename(basename);
        if paths.len() > 1 {
            return (FileTarget { path, line }, paths.to_vec());
        }

        (FileTarget { path, line }, Vec::new())
    }

    /// Indexed paths whose slash-normalized form ends at a path-component boundary
    /// with `suffix` (e.g. `long/path/main.rs`).
    pub fn paths_matching_suffix(&self, suffix: &str) -> Vec<PathBuf> {
        let suffix = normalize_slashes(suffix);
        if suffix.is_empty() {
            return Vec::new();
        }

        let mut matches = Vec::new();
        for path in self.indexed_paths() {
            let ps = normalize_slashes(&path.to_string_lossy());
            if path_ends_with_suffix(&ps, &suffix) && !matches.iter().any(|p| p == path) {
                matches.push(path.clone());
            }
        }
        matches
    }

    /// Resolve a leading-truncated path via longest suffix match against the index.
    ///
    /// “Leading” means the first `...` / `…` marker in the string (raw token or
    /// cwd-joined absolute path like `/repo/...z/foo.rs`), not necessarily byte 0.
    /// A literal path component containing `...` (e.g. `foo...bar`) can therefore
    /// false-positive; that is accepted for v1.
    ///
    /// Returns `None` when the path is not truncated or no suffix matches.
    /// Unique (or single open-buffer) hits rewrite the target and return no candidates;
    /// otherwise the original path is kept and candidates are returned for Lua.
    pub fn resolve_truncated(
        &self,
        path: &Path,
        line: Option<u32>,
    ) -> Option<(FileTarget, Vec<PathBuf>)> {
        let lossy = path.to_string_lossy();
        let query = truncated_query_from(&lossy)?;

        for suffix in path_suffix_queries(query) {
            let hits = self.paths_matching_suffix(&suffix);
            if hits.is_empty() {
                continue;
            }
            return Some(self.finish_truncated_hits(path, line, hits));
        }
        None
    }

    fn finish_truncated_hits(
        &self,
        original: &Path,
        line: Option<u32>,
        hits: Vec<PathBuf>,
    ) -> (FileTarget, Vec<PathBuf>) {
        if hits.len() == 1 {
            return (
                FileTarget {
                    path: hits[0].clone(),
                    line,
                },
                Vec::new(),
            );
        }

        let buf_hits: Vec<PathBuf> = hits
            .iter()
            .filter(|p| self.buffers.contains(*p))
            .cloned()
            .collect();
        let candidates = if buf_hits.is_empty() { hits } else { buf_hits };

        if candidates.len() == 1 {
            return (
                FileTarget {
                    path: candidates[0].clone(),
                    line,
                },
                Vec::new(),
            );
        }

        (
            FileTarget {
                path: original.to_path_buf(),
                line,
            },
            candidates,
        )
    }

    fn indexed_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.buffers
            .iter()
            .chain(self.vcs_working_tree.iter())
            .chain(self.vcs_branch.iter())
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
    /// These shapes always cue, including when the defining file is not open:
    /// `::` / `#`, a `.` that is not a filename, and mixed-case identifiers
    /// (`SyncWorkflow`). Other bare identifiers cue only when present in the
    /// index and strong enough (length ≥ 3, `_`, or qualified).
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
    ///
    /// Leading-truncated paths (`...` / `…`) rewrite when the longest suffix match is unique
    /// (or collapses to a single open buffer). Ambiguous truncated hits keep the parsed
    /// path so [`Self::resolve_open_from_index`] can inject candidates.
    pub fn file_target_for_token(&self, token: &str, working_directory: &Path) -> FileTarget {
        let parsed = FileTarget::parse(token, working_directory);

        if let Some((target, candidates)) = self.resolve_truncated(&parsed.path, parsed.line) {
            if candidates.is_empty() {
                return target;
            }
            return parsed;
        }

        let path_part = token_path_part(token);
        if path_part.contains('/') || path_part.contains('\\') {
            return parsed;
        }

        let basename = token_basename(token);
        if let Some(path) = self.unique_path_for_basename(basename) {
            return FileTarget {
                path,
                line: parsed.line,
            };
        }

        parsed
    }

    /// Resolve an indexed symbol to a FileTarget when unique; otherwise None.
    ///
    /// A single qualifier (`SourceKind.ACTIVE`, `Foo::bar`) tries the member, then the
    /// head, when the full string is not itself indexed. An ambiguous member stays unresolved
    /// so the caller can open a symbol picker instead of jumping to the type.
    pub fn file_target_for_symbol(&self, name: &str) -> Option<FileTarget> {
        match self.usable_symbol_target(name) {
            SymbolLookup::Unique(target) => return Some(target),
            SymbolLookup::Ambiguous => return None,
            SymbolLookup::Absent => {}
        }
        let (head, member) = qualified_symbol_parts(name)?;
        if symbol_token_is_strong(member) {
            match self.usable_symbol_target(member) {
                SymbolLookup::Unique(target) => return Some(target),
                SymbolLookup::Ambiguous => return None,
                SymbolLookup::Absent => {}
            }
        }
        if symbol_token_is_strong(head) {
            if let SymbolLookup::Unique(target) = self.usable_symbol_target(head) {
                return Some(target);
            }
        }
        None
    }

    /// Locations whose file must exist are ignored when it does not, so one stale
    /// cache hit does not make a live location look ambiguous.
    fn usable_symbol_target(&self, name: &str) -> SymbolLookup {
        let Some(locs) = self.symbols_by_name.get(name) else {
            return SymbolLookup::Absent;
        };
        let usable: Vec<&SymbolLoc> = locs
            .iter()
            .filter(|loc| !loc.require_file || loc.path.is_file())
            .collect();
        match usable.as_slice() {
            [] => SymbolLookup::Absent,
            [loc] => loc_to_file_target(loc)
                .map(SymbolLookup::Unique)
                .unwrap_or(SymbolLookup::Absent),
            _ => SymbolLookup::Ambiguous,
        }
    }

    /// Workspace-symbol query for a token that did not resolve to a unique location.
    ///
    /// Prefers an indexed member (`ACTIVE`) or head (`SourceKind`) over the raw
    /// qualified string, which document-symbol indexes do not store.
    pub fn symbol_open_query<'a>(&'a self, token: &'a str) -> &'a str {
        if self.contains_symbol(token) {
            return token;
        }
        let Some((head, member)) = qualified_symbol_parts(token) else {
            return token;
        };
        if symbol_token_is_strong(member) && self.contains_symbol(member) {
            return member;
        }
        if symbol_token_is_strong(head) && self.contains_symbol(head) {
            return head;
        }
        token
    }
}

fn loc_to_file_target(loc: &SymbolLoc) -> Option<FileTarget> {
    if loc.require_file && !loc.path.is_file() {
        return None;
    }
    Some(FileTarget {
        path: loc.path.clone(),
        line: Some(loc.line),
    })
}

fn group_symbols(
    symbols: impl IntoIterator<Item = IndexedSymbol>,
    require_file: bool,
) -> HashMap<String, Vec<SymbolLoc>> {
    let mut grouped: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
    for sym in symbols {
        let loc = SymbolLoc {
            path: sym.path,
            line: sym.line,
            kind: sym.kind,
            require_file,
        };
        let entry = grouped.entry(sym.name).or_default();
        if !entry.iter().any(|existing| {
            existing.path == loc.path && existing.line == loc.line && existing.kind == loc.kind
        }) {
            entry.push(loc);
        }
    }
    grouped
}

pub fn token_has_file_shape(token: &str) -> bool {
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    let path = token_path_part(token);
    if looks_like_dotted_filename(path) {
        return true;
    }
    // `README:10` has no extension, but a trailing line number is still a file cue.
    // Dotted qualifiers (`SourceKind.ACTIVE:3`) are not.
    path != token && !path.contains('.')
}

/// A `.` names a file when the last segment looks like an extension (`constants.py`),
/// not a symbol qualifier (`SourceKind.ACTIVE`, `obj.method_name`).
fn looks_like_dotted_filename(path: &str) -> bool {
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() {
        return false;
    }
    // Dotfiles: `.env`, `.gitignore`.
    if stem.is_empty() {
        return ext
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    }
    if !is_extension_shaped(ext) {
        return false;
    }
    // `AGENTS.md` / `my_module.py` / `MyClass.java`: a code-like stem is still a file when
    // the suffix is a short extension. `MyClass.method` (a longer lowercase word) stays a symbol.
    if stem_looks_like_code_qualifier(stem) && !extension_applies_to_code_like_stem(ext) {
        return false;
    }
    true
}

fn is_extension_shaped(ext: &str) -> bool {
    (1..=8).contains(&ext.len())
        && ext
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn stem_looks_like_code_qualifier(stem: &str) -> bool {
    stem.split('.')
        .any(|seg| seg.chars().any(|c| c.is_ascii_uppercase() || c == '_'))
}

fn extension_applies_to_code_like_stem(ext: &str) -> bool {
    // 1–4 lowercase characters covers `py`, `rs`, `java`, `toml`, `lock`.
    // Longer suffixes are filenames only when they are known types, so `method` is not one.
    ext.len() <= 4
        || matches!(
            ext,
            "astro" | "cmake" | "gradle" | "graphql" | "proto" | "svelte"
        )
}

const ASCII_ELLIPSIS: &str = "...";
const UNICODE_ELLIPSIS: &str = "\u{2026}";

/// Path query after the first `...` / `…` marker in `s`, if any.
///
/// “Leading” means the earliest marker by byte index (cwd-joined `/repo/...z/foo.rs`
/// counts), not necessarily byte 0. A literal component like `foo...bar` can
/// therefore false-positive; that is accepted for v1.
fn truncated_query_from(s: &str) -> Option<&str> {
    let ascii = s
        .find(ASCII_ELLIPSIS)
        .map(|idx| (idx, ASCII_ELLIPSIS.len()));
    let unicode = s
        .find(UNICODE_ELLIPSIS)
        .map(|idx| (idx, UNICODE_ELLIPSIS.len()));
    let (idx, marker_len) = match (ascii, unicode) {
        (Some(a), Some(u)) if u.0 < a.0 => u,
        (Some(a), _) => a,
        (None, Some(u)) => u,
        (None, None) => return None,
    };
    let after = s[idx + marker_len..].trim_start_matches(['/', '\\']);
    if after.is_empty() { None } else { Some(after) }
}

fn normalize_slashes(s: &str) -> String {
    s.replace('\\', "/")
}

fn path_ends_with_suffix(path: &str, suffix: &str) -> bool {
    if !path.ends_with(suffix) {
        return false;
    }
    if path.len() == suffix.len() {
        return true;
    }
    path.as_bytes()
        .get(path.len() - suffix.len() - 1)
        .is_some_and(|b| *b == b'/')
}

/// Progressive path suffixes, longest first (`a/b/c.rs` → `a/b/c.rs`, `b/c.rs`, `c.rs`).
fn path_suffix_queries(stripped: &str) -> Vec<String> {
    let normalized = normalize_slashes(stripped);
    let components: Vec<&str> = normalized.split('/').filter(|c| !c.is_empty()).collect();
    let mut out = Vec::with_capacity(components.len());
    for start in 0..components.len() {
        out.push(components[start..].join("/"));
    }
    out
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

/// Cold-index shape: `::` / `#`, a `.` qualifier that is not a filename, or a
/// mixed-case identifier (`SyncWorkflow`, `parseXML`).
pub(crate) fn looks_like_scanned_symbol_shape(token: &str) -> bool {
    if token_has_file_shape(token) {
        return false;
    }
    if token.contains("::") || token.contains('#') {
        return true;
    }
    if token.contains('.') {
        return true;
    }
    looks_like_mixed_case_identifier(token)
}

/// Pascal/camel case: an uppercase letter after the first character, and a lowercase letter.
///
/// Sentence case (`Workflow`) and all-caps (`API`) stay out so prose is not underlined.
/// Those still cue when the open-buffer index contains them.
fn looks_like_mixed_case_identifier(token: &str) -> bool {
    let mut has_lower = false;
    let mut has_internal_upper = false;
    for (index, ch) in token.chars().enumerate() {
        if ch.is_ascii_lowercase() {
            has_lower = true;
        } else if index > 0 && ch.is_ascii_uppercase() {
            has_internal_upper = true;
        }
    }
    has_lower && has_internal_upper
}

/// Tokens worth a background `workspace/symbol` query.
///
/// Mixed-case, qualified, and snake_case identifiers. Plain words and filenames
/// are left out so agent prose does not turn into a query per word.
pub fn symbol_query_candidates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut index = 0;
    while index < chars.len() {
        if !is_symbol_query_char(chars[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && is_symbol_query_char(chars[index]) {
            index += 1;
        }
        let raw: String = chars[start..index].iter().collect();
        let token = raw.trim_matches(['.', ':', '#']).to_string();
        if is_symbol_query_candidate(&token) && seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

pub fn is_symbol_query_candidate(token: &str) -> bool {
    if token.is_empty() || token_has_file_shape(token) {
        return false;
    }
    let starts = token
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    if !starts {
        return false;
    }
    if !token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '#'))
    {
        return false;
    }
    if looks_like_scanned_symbol_shape(token) {
        return true;
    }
    token.contains('_') && symbol_token_is_strong(token)
}

fn is_symbol_query_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '#')
}

/// One `Head.Member` / `Head::Member` / `Head#Member` pair. Deeper paths stay intact.
fn qualified_symbol_parts(name: &str) -> Option<(&str, &str)> {
    for sep in ["::", ".", "#"] {
        let Some((head, member)) = name.split_once(sep) else {
            continue;
        };
        if head.is_empty()
            || member.is_empty()
            || contains_qualifier_sep(head)
            || contains_qualifier_sep(member)
        {
            continue;
        }
        return Some((head, member));
    }
    None
}

fn contains_qualifier_sep(name: &str) -> bool {
    name.contains("::") || name.contains('.') || name.contains('#')
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
    fn mixed_case_identifier_cues_without_open_buffer_index() {
        let idx = ReferenceIndex::default();
        assert!(idx.should_cue_symbol_token("SyncWorkflow"));
        assert!(idx.should_cue_symbol_token("parseXML"));
        assert!(!idx.should_cue_symbol_token("Workflow"));
        assert!(!idx.should_cue_symbol_token("workflow"));
        assert!(!idx.should_cue_symbol_token("API"));
        assert!(!idx.should_cue_symbol_token("get_collector_statuses"));
    }

    #[test]
    fn symbol_query_candidates_skip_plain_words_and_filenames() {
        let names = symbol_query_candidates(
            "when SyncWorkflow calls get_collector_statuses see src/main.rs and Workflow",
        );
        assert_eq!(
            names,
            vec![
                "SyncWorkflow".to_string(),
                "get_collector_statuses".to_string()
            ]
        );
        assert!(!is_symbol_query_candidate("Workflow"));
        assert!(!is_symbol_query_candidate("collectors"));
        assert!(is_symbol_query_candidate("SourceKind.ACTIVE"));
    }

    #[test]
    fn buffer_symbols_keep_every_definition_in_the_same_file() {
        let mut idx = ReferenceIndex::default();
        idx.merge_cached_symbols([symbol("new", "/repo/src/lib.rs", 3)]);
        idx.set_symbols([
            symbol("new", "/repo/src/lib.rs", 10),
            symbol("new", "/repo/src/lib.rs", 40),
        ]);
        assert!(idx.unique_symbol("new").is_none());
        assert_eq!(
            idx.symbols_by_name.get("new").map(|locs| locs.len()),
            Some(2)
        );
    }

    #[test]
    fn stale_cache_hit_does_not_hide_a_live_location() {
        let file = std::env::temp_dir().join(format!("via-symbol-live-{}", std::process::id()));
        std::fs::write(&file, "fn sync() {}\n").unwrap();
        let mut idx = ReferenceIndex::default();
        idx.merge_cached_symbols([
            symbol("SyncWorkflow", "/no/such/via-symbol.py", 1),
            symbol("SyncWorkflow", file.to_str().unwrap(), 4),
        ]);
        let target = idx.file_target_for_symbol("SyncWorkflow").unwrap();
        assert_eq!(target.path, file);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn buffer_symbol_refresh_keeps_cached_names() {
        let mut idx = ReferenceIndex::default();
        idx.merge_cached_symbols([symbol("SyncWorkflow", "/repo/src/sync.py", 4)]);
        idx.set_symbols([symbol("parse_event", "/repo/src/main.rs", 10)]);
        assert!(idx.contains_symbol("SyncWorkflow"));
        assert!(idx.contains_symbol("parse_event"));
    }

    #[test]
    fn short_indexed_name_without_strength_does_not_cue() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("ab", "/repo/src/main.rs", 10)]);
        assert!(!idx.should_cue_symbol_token("ab"));
    }

    #[test]
    fn truncated_unique_suffix_rewrites_file_target() {
        let idx =
            ReferenceIndex::from_parts([PathBuf::from("/repo/some/long/path/main.rs")], [], []);
        let target = idx.file_target_for_token("...z/some/long/path/main.rs", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/some/long/path/main.rs"));
    }

    #[test]
    fn resolve_open_unique_truncated_rewrites_and_has_no_candidates() {
        let idx =
            ReferenceIndex::from_parts([PathBuf::from("/repo/some/long/path/main.rs")], [], []);
        let (target, candidates) = idx
            .resolve_open_from_index(PathBuf::from("/repo/...z/some/long/path/main.rs"), Some(9));
        assert_eq!(target.path, PathBuf::from("/repo/some/long/path/main.rs"));
        assert_eq!(target.line, Some(9));
        assert!(candidates.is_empty());
    }

    #[test]
    fn truncated_unique_suffix_preserves_line() {
        let idx =
            ReferenceIndex::from_parts([PathBuf::from("/repo/some/long/path/main.rs")], [], []);
        let target =
            idx.file_target_for_token("...z/some/long/path/main.rs:42", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/some/long/path/main.rs"));
        assert_eq!(target.line, Some(42));
    }

    #[test]
    fn truncated_unicode_ellipsis_rewrites() {
        let idx = ReferenceIndex::from_parts([PathBuf::from("/repo/src/lib.rs")], [], []);
        let target = idx.file_target_for_token("\u{2026}/src/lib.rs", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/src/lib.rs"));
    }

    #[test]
    fn truncated_ambiguous_prefers_open_buffers() {
        let idx = ReferenceIndex::from_parts(
            [PathBuf::from("/repo/src/main.rs")],
            [PathBuf::from("/repo/tests/main.rs")],
            [],
        );
        // Basename-only suffix after dropping junk still hits both; buffer wins.
        let (target, candidates) =
            idx.resolve_open_from_index(PathBuf::from("/repo/...z/main.rs"), Some(3));
        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(target.line, Some(3));
        assert!(candidates.is_empty());
    }

    #[test]
    fn truncated_ambiguous_without_buffer_returns_candidates() {
        let idx = ReferenceIndex::from_parts(
            [],
            [
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/tests/main.rs"),
            ],
            [],
        );
        let original = PathBuf::from("/cwd/.../main.rs");
        let (target, candidates) = idx.resolve_open_from_index(original.clone(), None);
        assert_eq!(target.path, original);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn truncated_multi_buffer_returns_buffer_subset_as_candidates() {
        let idx = ReferenceIndex::from_parts(
            [
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/tests/main.rs"),
            ],
            [PathBuf::from("/repo/vendor/main.rs")],
            [],
        );
        let original = PathBuf::from("/cwd/.../main.rs");
        let (target, candidates) = idx.resolve_open_from_index(original.clone(), Some(1));
        assert_eq!(target.path, original);
        assert_eq!(target.line, Some(1));
        assert_eq!(candidates.len(), 2);
        assert!(candidates.contains(&PathBuf::from("/repo/src/main.rs")));
        assert!(candidates.contains(&PathBuf::from("/repo/tests/main.rs")));
        assert!(!candidates.contains(&PathBuf::from("/repo/vendor/main.rs")));
    }

    #[test]
    fn truncated_longest_suffix_preferred_over_shorter() {
        let idx = ReferenceIndex::from_parts(
            [
                PathBuf::from("/repo/other/path/main.rs"),
                PathBuf::from("/repo/some/long/path/main.rs"),
            ],
            [],
            [],
        );
        let target = idx.file_target_for_token("...z/some/long/path/main.rs", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/some/long/path/main.rs"));
    }

    #[test]
    fn non_truncated_path_shaped_still_does_not_steal_basename() {
        let idx = index();
        let target = idx.file_target_for_token("vendor/main.rs", Path::new("/repo"));
        assert_eq!(target.path, PathBuf::from("/repo/vendor/main.rs"));
    }

    #[test]
    fn truncated_query_from_detects_markers() {
        assert_eq!(
            truncated_query_from("...z/some/long/path/main.rs"),
            Some("z/some/long/path/main.rs")
        );
        assert_eq!(
            truncated_query_from("/repo/.../src/lib.rs"),
            Some("src/lib.rs")
        );
        assert_eq!(
            truncated_query_from("\u{2026}/src/lib.rs"),
            Some("src/lib.rs")
        );
        assert!(truncated_query_from("vendor/main.rs").is_none());
        assert!(truncated_query_from("...").is_none());
    }

    #[test]
    fn truncated_query_from_uses_earliest_marker() {
        // Unicode ellipsis before ASCII `...` must win by byte position.
        assert_eq!(truncated_query_from("\u{2026}foo...bar"), Some("foo...bar"));
        assert_eq!(
            truncated_query_from("...foo\u{2026}bar"),
            Some("foo\u{2026}bar")
        );
    }

    #[test]
    fn dotted_symbol_qualifier_is_not_a_filename() {
        assert!(!token_has_file_shape("SourceKind.ACTIVE"));
        assert!(!token_has_file_shape("WidgetKind.PRIMARY"));
        assert!(!token_has_file_shape("MyClass.method"));
        assert!(!token_has_file_shape("obj.method_name"));
        assert!(token_has_file_shape("constants.py"));
        assert!(token_has_file_shape("my_module.py"));
        assert!(token_has_file_shape("AGENTS.md"));
        assert!(token_has_file_shape("MyClass.java"));
        assert!(token_has_file_shape("unknown.rs"));
        assert!(token_has_file_shape("src/new_file.rs"));
        assert!(token_has_file_shape(".gitignore"));
        assert!(token_has_file_shape("README:10"));
    }

    #[test]
    fn dotted_symbol_cues_without_a_file_index_hit() {
        let idx = ReferenceIndex::default();
        assert!(idx.should_cue_symbol_token("SourceKind.ACTIVE"));
        assert!(!idx.should_cue_file_token("SourceKind.ACTIVE"));
        assert!(idx.should_cue_file_token("constants.py"));
        assert!(!idx.should_cue_symbol_token("constants.py"));
    }

    #[test]
    fn qualified_symbol_resolves_unique_indexed_member() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("ACTIVE", "/repo/pkg/types.py", 22)]);
        let target = idx.file_target_for_symbol("SourceKind.ACTIVE").unwrap();
        assert_eq!(target.path, PathBuf::from("/repo/pkg/types.py"));
        assert_eq!(target.line, Some(22));
    }

    #[test]
    fn qualified_symbol_falls_back_to_unique_head() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([symbol("SourceKind", "/repo/pkg/types.py", 13)]);
        let target = idx.file_target_for_symbol("SourceKind.ACTIVE").unwrap();
        assert_eq!(target.line, Some(13));
    }

    #[test]
    fn ambiguous_qualified_member_stays_a_symbol_query() {
        let mut idx = ReferenceIndex::default();
        idx.set_symbols([
            symbol("ACTIVE", "/repo/a.py", 1),
            symbol("ACTIVE", "/repo/b.py", 2),
            symbol("SourceKind", "/repo/types.py", 13),
        ]);
        assert!(idx.file_target_for_symbol("SourceKind.ACTIVE").is_none());
        assert_eq!(idx.symbol_open_query("SourceKind.ACTIVE"), "ACTIVE");
    }
}
