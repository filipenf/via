use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::nvim::FileTarget;

/// Snapshot of known files for Ctrl-held cue scoring and click resolution.
///
/// Built from Neovim open buffers + VCS changed paths. Always partial — treat
/// as a ranking signal, not a hard filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceIndex {
    pub buffers: HashSet<PathBuf>,
    pub basenames: HashMap<String, Vec<PathBuf>>,
    pub vcs_working_tree: HashSet<PathBuf>,
    pub vcs_branch: HashSet<PathBuf>,
}

impl ReferenceIndex {
    pub fn from_parts(
        buffers: impl IntoIterator<Item = PathBuf>,
        vcs_working_tree: impl IntoIterator<Item = PathBuf>,
        vcs_branch: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let buffers: HashSet<PathBuf> = buffers.into_iter().collect();
        let vcs_working_tree: HashSet<PathBuf> = vcs_working_tree.into_iter().collect();
        let vcs_branch: HashSet<PathBuf> = vcs_branch.into_iter().collect();

        let mut basenames: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in buffers
            .iter()
            .chain(vcs_working_tree.iter())
            .chain(vcs_branch.iter())
        {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let entry = basenames.entry(name.to_string()).or_default();
                if !entry.iter().any(|p| p == path) {
                    entry.push(path.clone());
                }
            }
        }

        Self {
            buffers,
            basenames,
            vcs_working_tree,
            vcs_branch,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty() && self.vcs_working_tree.is_empty() && self.vcs_branch.is_empty()
    }

    /// Unique absolute path for a bare basename, if the index has exactly one candidate.
    pub fn unique_path_for_basename(&self, basename: &str) -> Option<PathBuf> {
        let paths = self.basenames.get(basename)?;
        if paths.len() == 1 {
            Some(paths[0].clone())
        } else {
            None
        }
    }

    pub fn contains_basename(&self, basename: &str) -> bool {
        self.basenames.contains_key(basename)
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

    /// Whether a scanned token should become a file cue.
    pub fn should_cue_file_token(&self, token: &str) -> bool {
        if token_has_file_shape(token) {
            return true;
        }
        // Bare basename (no path separators): only if known in index.
        let basename = token_basename(token);
        self.contains_basename(basename)
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
}

pub fn token_has_file_shape(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || token.contains('.')
        || token
            .rsplit_once(':')
            .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
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
    fn scores_buffer_paths_highest() {
        let idx = index();
        let buffer = Path::new("/repo/src/main.rs");
        let branch = Path::new("/repo/src/editor.rs");
        assert!(idx.score_file(buffer, true) > idx.score_file(branch, true));
    }
}
