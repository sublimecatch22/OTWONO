//! Path containment used by file capabilities.
//!
//! Comparison is on normalised path *components*, so `/home/u/docs` covers
//! `/home/u/docs/a.txt` but not `/home/u/docsx`, and `..` cannot climb out of a
//! grant.

use std::path::{Component, Path, PathBuf};

/// Normalise without touching the filesystem: resolve `.` and `..`
/// lexically, and drop trailing separators.
pub fn normalise(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when `candidate` is `root` or lies beneath it.
pub fn is_prefix_of(root: &str, candidate: &str) -> bool {
    let root = normalise(root);
    let candidate = normalise(candidate);
    candidate.starts_with(&root)
}

/// Whether a path escapes the sandbox once `..` is applied. Used to refuse a
/// model-supplied path before it reaches the filesystem.
pub fn escapes(root: &str, candidate: &str) -> bool {
    !is_prefix_of(root, candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_covers_its_own_contents() {
        assert!(is_prefix_of("/home/u/docs", "/home/u/docs"));
        assert!(is_prefix_of("/home/u/docs", "/home/u/docs/a.txt"));
        assert!(is_prefix_of(
            "/home/u/docs",
            "/home/u/docs/deep/nested/a.txt"
        ));
    }

    #[test]
    fn a_similarly_named_sibling_is_not_covered() {
        assert!(!is_prefix_of("/home/u/docs", "/home/u/docsx"));
        assert!(!is_prefix_of("/home/u/docs", "/home/u/docs-backup/a.txt"));
        assert!(!is_prefix_of("/home/u/docs", "/home/u"));
    }

    #[test]
    fn parent_traversal_cannot_escape_a_grant() {
        for attempt in [
            "/home/u/docs/../secrets/key.txt",
            "/home/u/docs/./../../etc/passwd",
            "/home/u/docs/a/../../..",
        ] {
            assert!(escapes("/home/u/docs", attempt), "{attempt} should escape");
        }
    }

    #[test]
    fn harmless_traversal_inside_the_grant_is_allowed() {
        assert!(!escapes("/home/u/docs", "/home/u/docs/a/../b.txt"));
        assert!(!escapes("/home/u/docs", "/home/u/docs/./notes.md"));
    }

    #[test]
    fn trailing_separators_do_not_change_the_answer() {
        assert!(is_prefix_of("/home/u/docs/", "/home/u/docs/a.txt"));
        assert!(is_prefix_of("/home/u/docs", "/home/u/docs/"));
    }
}
