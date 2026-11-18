//! The path kernel of a patch (ADR 0042): which repository paths a patch of
//! the assistant may write.
//!
//! The assistant proposes patches, and the owner opens each one as a draft
//! PR. Code in a PR runs in CI, and a workflow file runs with the rights of
//! the repository. So a patch never writes under `.github/`. It also never
//! names a path that leaves the tree, and never writes the `.git` folder.
//!
//! A path is relative and uses `/`. It is not empty. Each segment is not
//! empty, not `.`, not `..` and not `.git`. The first segment is not
//! `.github`. The two names match in any ASCII case, because a file system
//! that folds case would open the same folder. No byte is a control byte,
//! DEL or `\`.
//!
//! `formal/verus/src/patch_path.rs` proves that the kernel is exactly that
//! reading. Each body between the mirror markers matches the proof.

/// Whether `b` may not appear in a path: a control byte, DEL or `\`.
const fn is_bad_byte(b: u8) -> bool {
    // mirror-begin is_bad_byte
    b < 32 || b == 127 || b == 92
    // mirror-end is_bad_byte
}

/// `b` in ASCII lower case.
const fn lower(b: u8) -> u8 {
    // mirror-begin lower
    if 65 <= b && b <= 90 { b + 32 } else { b }
    // mirror-end lower
}

/// Whether a segment of `p` ends at `i`: the end of the path or a `/`.
const fn ends_at(p: &[u8], i: usize) -> bool {
    // mirror-begin ends_at
    i == p.len() || (i < p.len() && p[i] == 47)
    // mirror-end ends_at
}

/// Whether a segment of `p` starts at `i`, for `i` inside the path.
const fn starts_at(p: &[u8], i: usize) -> bool {
    // mirror-begin starts_at
    i == 0 || (i <= p.len() && p[i - 1] == 47)
    // mirror-end starts_at
}

/// Whether the segment at `i` is `.` or `..`.
const fn is_dot_segment(p: &[u8], i: usize) -> bool {
    // mirror-begin is_dot_segment
    i < p.len()
        && p[i] == 46
        && (ends_at(p, i + 1) || (i + 1 < p.len() && p[i + 1] == 46 && ends_at(p, i + 2)))
    // mirror-end is_dot_segment
}

/// Whether the segment at `i` is `.git` in any case.
const fn is_git_segment(p: &[u8], i: usize) -> bool {
    // mirror-begin is_git_segment
    i + 3 < p.len()
        && p[i] == 46
        && lower(p[i + 1]) == 103
        && lower(p[i + 2]) == 105
        && lower(p[i + 3]) == 116
        && ends_at(p, i + 4)
    // mirror-end is_git_segment
}

/// Whether the `/` at `i` makes an empty segment: a leading `/`, a
/// trailing `/` or `//`.
const fn is_empty_segment(p: &[u8], i: usize) -> bool {
    // mirror-begin is_empty_segment
    i < p.len() && p[i] == 47 && (i == 0 || i + 1 == p.len() || p[i + 1] == 47)
    // mirror-end is_empty_segment
}

/// Whether the byte at `i` keeps the path valid.
const fn byte_ok(p: &[u8], i: usize) -> bool {
    // mirror-begin byte_ok
    !is_bad_byte(p[i])
        && !is_empty_segment(p, i)
        && !(starts_at(p, i) && (is_dot_segment(p, i) || is_git_segment(p, i)))
    // mirror-end byte_ok
}

/// Whether each byte of `p` from `i` on keeps the path valid. The
/// recursion is as deep as the path is long.
const fn all_ok(p: &[u8], i: usize) -> bool {
    // mirror-begin all_ok
    i >= p.len() || (byte_ok(p, i) && all_ok(p, i + 1))
    // mirror-end all_ok
}

/// Whether the first segment of `p` is `.github` in any case.
const fn is_github_first(p: &[u8]) -> bool {
    // mirror-begin is_github_first
    p.len() >= 7
        && p[0] == 46
        && lower(p[1]) == 103
        && lower(p[2]) == 105
        && lower(p[3]) == 116
        && lower(p[4]) == 104
        && lower(p[5]) == 117
        && lower(p[6]) == 98
        && ends_at(p, 7)
    // mirror-end is_github_first
}

/// [`is_patch_path`] on the bytes of a path.
#[must_use]
pub const fn is_patch_path_bytes(p: &[u8]) -> bool {
    // mirror-begin is_patch_path_bytes
    !p.is_empty() && !is_github_first(p) && all_ok(p, 0)
    // mirror-end is_patch_path_bytes
}

/// Whether a patch may write the repository path `path`.
#[must_use]
pub const fn is_patch_path(path: &str) -> bool {
    is_patch_path_bytes(path.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_path_of_the_tree_is_a_patch_path() {
        for path in [
            "README.md",
            "crates/littleguy/src/notes.rs",
            "docs/adr/0042-x.md",
            ".gitignore",
            ".githubx/file",
            "a/.github/b",
            "a/..b/c",
            "a/b..",
            "...",
            "a/.gitx",
            "a.git/b",
        ] {
            assert!(is_patch_path(path), "{path} is in the tree");
        }
    }

    #[test]
    fn a_path_that_leaves_the_tree_or_reaches_ci_is_refused() {
        for path in [
            "",
            "/etc/passwd",
            "a//b",
            "a/",
            "/",
            ".",
            "..",
            "../x",
            "a/../b",
            "a/./b",
            "a/..",
            ".github",
            ".github/workflows/ci.yml",
            ".GitHub/workflows/ci.yml",
            ".git/config",
            "a/.git/hooks/pre-commit",
            "a/.GIT",
            "a\\b",
            "a\nb",
            "a\u{7f}b",
            "a\tb",
        ] {
            assert!(!is_patch_path(path), "{path:?} is refused");
        }
    }

    /// The plain reading of a patch path, for the property test.
    fn plain(path: &str) -> bool {
        if path.is_empty() || path.bytes().any(|b| b < 32 || b == 127 || b == b'\\') {
            return false;
        }
        let segments: Vec<&str> = path.split('/').collect();
        if segments[0].eq_ignore_ascii_case(".github") {
            return false;
        }
        segments
            .iter()
            .all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.eq_ignore_ascii_case(".git"))
    }

    proptest! {
        #[test]
        fn the_kernel_is_the_plain_reading(path in "[a-zA-Z./\\\\\t_-]{0,24}") {
            prop_assert_eq!(is_patch_path(&path), plain(&path));
        }

        #[test]
        fn the_kernel_is_the_plain_reading_near_the_names(
            parts in prop::collection::vec(
                prop::sample::select(vec![
                    ".github", ".GITHUB", ".git", ".Git", ".", "..", "", "src", "a.rs",
                    ".githubx", "..x", "x..",
                ]),
                1..5,
            )
        ) {
            let path = parts.join("/");
            prop_assert_eq!(is_patch_path(&path), plain(&path));
        }
    }
}
