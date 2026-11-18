//! The edits of a patch, and the kernel that applies them (ADR 0042).
//!
//! An edit is an exact replacement, a new file or a deleted file. A model
//! writes an exact replacement correctly far more often than a unified
//! diff. The `old` text of a replacement must occur exactly once in the
//! file as the earlier edits left it, so an edit never lands in the wrong
//! place. Overlapping matches count as two.
//!
//! The kernel is pure. The tool and the open both run it on the files at
//! the base commit, so the diff that the owner reads is the diff that the
//! daemon pushes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// One edit of a patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Edit {
    /// Replace the one occurrence of `old` in the file at `path` with
    /// `new`.
    Replace {
        /// The repository path.
        path: String,
        /// The exact text to replace. It occurs once.
        old: String,
        /// The text that takes its place.
        new: String,
    },
    /// Write a new file at `path`.
    Create {
        /// The repository path.
        path: String,
        /// The whole file.
        create: String,
    },
    /// Delete the file at `path`.
    Delete {
        /// The repository path.
        path: String,
        /// Always true.
        delete: bool,
    },
}

impl Edit {
    /// The repository path of the edit.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Replace { path, .. } | Self::Create { path, .. } | Self::Delete { path, .. } => {
                path
            }
        }
    }
}

/// Why the edits do not apply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// The patch has no edit.
    #[error("the patch has no edit")]
    Empty,
    /// A path is not one that a patch may write.
    #[error(
        "edit {index}: `{path}` is not a path that a patch may write. A path is relative, uses \
         `/`, has no empty, `.`, `..` or `.git` segment, and is not under `.github/`"
    )]
    Path {
        /// The 1-based number of the edit.
        index: usize,
        /// The path.
        path: String,
    },
    /// A delete edit does not say `delete: true`.
    #[error("edit {index}: a delete edit says `\"delete\": true`")]
    Delete {
        /// The 1-based number of the edit.
        index: usize,
    },
    /// A replacement has an empty `old`.
    #[error("edit {index}: `old` is empty. To write a new file, use `create`")]
    EmptyOld {
        /// The 1-based number of the edit.
        index: usize,
    },
    /// The file of a replacement or a delete does not exist.
    #[error("edit {index}: `{path}` does not exist at the base")]
    Missing {
        /// The 1-based number of the edit.
        index: usize,
        /// The path.
        path: String,
    },
    /// The file of a create exists.
    #[error("edit {index}: `{path}` exists at the base. Replace text in it instead")]
    Exists {
        /// The 1-based number of the edit.
        index: usize,
        /// The path.
        path: String,
    },
    /// The file is not UTF-8 text.
    #[error("edit {index}: `{path}` is not UTF-8 text, so a patch cannot edit it")]
    Binary {
        /// The 1-based number of the edit.
        index: usize,
        /// The path.
        path: String,
    },
    /// `old` does not occur exactly once.
    #[error(
        "edit {index}: `old` occurs {found} times in `{path}`, and it must occur exactly once. \
         Read the file at the base again, and quote more of the lines around the change"
    )]
    Matches {
        /// The 1-based number of the edit.
        index: usize,
        /// The path.
        path: String,
        /// How often it occurs, at most 2.
        found: usize,
    },
    /// The edits leave every file as it was.
    #[error("the edits change nothing")]
    NoChange,
}

/// A file at the base: its text, or `None` when it does not exist. A file
/// that is not UTF-8 text is `Some(Err(()))`.
pub type BaseFile = Option<Result<String, ()>>;

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The repository path.
    pub path: String,
    /// The text at the base, or `None` for a new file.
    pub before: Option<String>,
    /// The text after the edits, or `None` for a deleted file.
    pub after: Option<String>,
}

/// Checks the shape of `edits`: at least one, each path a patch path, each
/// delete true and each `old` not empty. It needs no file.
///
/// # Errors
///
/// Returns the first edit that is not valid.
pub fn check(edits: &[Edit]) -> Result<(), EditError> {
    if edits.is_empty() {
        return Err(EditError::Empty);
    }
    for (n, edit) in edits.iter().enumerate() {
        let index = n + 1;
        if !littleguy_core::patch_path::is_patch_path(edit.path()) {
            return Err(EditError::Path {
                index,
                path: edit.path().to_owned(),
            });
        }
        match edit {
            Edit::Delete { delete: false, .. } => return Err(EditError::Delete { index }),
            Edit::Replace { old, .. } if old.is_empty() => {
                return Err(EditError::EmptyOld { index });
            }
            _ => {}
        }
    }
    Ok(())
}

/// How often `needle` occurs in `hay`, counting overlaps, up to 2.
fn occurrences(hay: &str, needle: &str) -> usize {
    match (hay.find(needle), hay.rfind(needle)) {
        (None, _) | (_, None) => 0,
        (Some(first), Some(last)) if first == last => 1,
        _ => 2,
    }
}

/// Applies `edits` in order to `base`, which holds each file that an edit
/// names. Returns the files that changed, by path.
///
/// # Errors
///
/// Returns the first edit that does not apply, or [`EditError::NoChange`].
pub fn apply(base: &BTreeMap<String, BaseFile>, edits: &[Edit]) -> Result<Vec<Change>, EditError> {
    check(edits)?;
    let mut files: BTreeMap<&str, Option<String>> = BTreeMap::new();
    for (n, edit) in edits.iter().enumerate() {
        let index = n + 1;
        let path = edit.path();
        if !files.contains_key(path) {
            let text = match base.get(path).cloned().flatten() {
                None => None,
                Some(Ok(text)) => Some(text),
                Some(Err(())) => {
                    return Err(EditError::Binary {
                        index,
                        path: path.to_owned(),
                    });
                }
            };
            files.insert(path, text);
        }
        let slot = files.entry(path).or_default();
        let missing = || EditError::Missing {
            index,
            path: path.to_owned(),
        };
        match edit {
            Edit::Replace { old, new, .. } => {
                let text = slot.as_mut().ok_or_else(missing)?;
                let found = occurrences(text, old);
                if found != 1 {
                    return Err(EditError::Matches {
                        index,
                        path: path.to_owned(),
                        found,
                    });
                }
                *text = text.replacen(old.as_str(), new, 1);
            }
            Edit::Create { create, .. } => {
                if slot.is_some() {
                    return Err(EditError::Exists {
                        index,
                        path: path.to_owned(),
                    });
                }
                *slot = Some(create.clone());
            }
            Edit::Delete { .. } => {
                if slot.take().is_none() {
                    return Err(missing());
                }
            }
        }
    }
    let changes: Vec<Change> = files
        .into_iter()
        .filter_map(|(path, after)| {
            let before = base.get(path).cloned().flatten().and_then(Result::ok);
            (before != after).then(|| Change {
                path: path.to_owned(),
                before,
                after,
            })
        })
        .collect();
    if changes.is_empty() {
        return Err(EditError::NoChange);
    }
    Ok(changes)
}

/// The unified diff of `changes`, in the form of `git diff`.
#[must_use]
pub fn diff(changes: &[Change]) -> String {
    let mut out = String::new();
    for change in changes {
        let path = &change.path;
        let before = change.before.as_deref().unwrap_or_default();
        let after = change.after.as_deref().unwrap_or_default();
        let old = if change.before.is_some() {
            format!("a/{path}")
        } else {
            String::from("/dev/null")
        };
        let new = if change.after.is_some() {
            format!("b/{path}")
        } else {
            String::from("/dev/null")
        };
        let _ = writeln!(out, "diff --git a/{path} b/{path}");
        if change.before.is_none() {
            out.push_str("new file mode 100644\n");
        } else if change.after.is_none() {
            out.push_str("deleted file mode 100644\n");
        }
        let text = similar::TextDiff::from_lines(before, after);
        let _ = write!(
            out,
            "{}",
            text.unified_diff().context_radius(3).header(&old, &new)
        );
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The digest of a patch: SHA-256 over its title, body, base and diff, in
/// lower-case hex. The parts are joined by a NUL byte, which none holds.
#[must_use]
pub fn digest(title: &str, body: &str, base: &str, diff: &str) -> String {
    let mut hash = Sha256::new();
    for (n, part) in [title, body, base, diff].into_iter().enumerate() {
        if n > 0 {
            hash.update([0u8]);
        }
        hash.update(part.as_bytes());
    }
    hash.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn base(files: &[(&str, &str)]) -> BTreeMap<String, BaseFile> {
        files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), Some(Ok((*text).to_owned()))))
            .collect()
    }

    fn replace(path: &str, old: &str, new: &str) -> Edit {
        Edit::Replace {
            path: path.to_owned(),
            old: old.to_owned(),
            new: new.to_owned(),
        }
    }

    #[test]
    fn edits_parse_from_the_json_that_the_model_writes() {
        let edits: Vec<Edit> = serde_json::from_str(
            r#"[{"path":"a.rs","old":"x","new":"y"},{"path":"b.md","create":"hi"},{"path":"c","delete":true}]"#,
        )
        .unwrap();
        assert_eq!(
            edits,
            [
                replace("a.rs", "x", "y"),
                Edit::Create {
                    path: String::from("b.md"),
                    create: String::from("hi")
                },
                Edit::Delete {
                    path: String::from("c"),
                    delete: true
                },
            ]
        );
    }

    #[test]
    fn a_replacement_lands_where_its_text_occurs_once() {
        let files = base(&[("src/lib.rs", "fn a() {}\nfn b() {}\n")]);
        let changes = apply(
            &files,
            &[replace("src/lib.rs", "fn b() {}", "fn b() { a() }")],
        )
        .unwrap();
        assert_eq!(
            changes,
            [Change {
                path: String::from("src/lib.rs"),
                before: Some(String::from("fn a() {}\nfn b() {}\n")),
                after: Some(String::from("fn a() {}\nfn b() { a() }\n")),
            }]
        );
        let shown = diff(&changes);
        assert!(
            shown.starts_with(
                "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n"
            ),
            "{shown}"
        );
        assert!(shown.contains("-fn b() {}\n+fn b() { a() }\n"), "{shown}");
    }

    #[test]
    fn edits_apply_in_order_to_one_file() {
        let files = base(&[("a", "one two")]);
        let changes = apply(
            &files,
            &[replace("a", "one", "1"), replace("a", "1 two", "1 2")],
        )
        .unwrap();
        assert_eq!(changes[0].after.as_deref(), Some("1 2"));
    }

    #[test]
    fn an_ambiguous_or_missing_text_is_refused_with_its_count() {
        let files = base(&[("a", "x x"), ("b", "aaa")]);
        assert_eq!(
            apply(&files, &[replace("a", "x", "y")]).unwrap_err(),
            EditError::Matches {
                index: 1,
                path: String::from("a"),
                found: 2
            }
        );
        // Overlapping matches count as two.
        assert!(matches!(
            apply(&files, &[replace("b", "aa", "b")]).unwrap_err(),
            EditError::Matches { found: 2, .. }
        ));
        assert!(matches!(
            apply(&files, &[replace("a", "z", "y")]).unwrap_err(),
            EditError::Matches { found: 0, .. }
        ));
        assert!(matches!(
            apply(&files, &[replace("gone.rs", "z", "y")]).unwrap_err(),
            EditError::Missing { index: 1, .. }
        ));
    }

    #[test]
    fn create_and_delete_follow_the_files() {
        let files = base(&[("old.rs", "x\n")]);
        let created = Edit::Create {
            path: String::from("new.rs"),
            create: String::from("y\n"),
        };
        let deleted = Edit::Delete {
            path: String::from("old.rs"),
            delete: true,
        };
        let changes = apply(&files, &[created, deleted.clone()]).unwrap();
        let shown = diff(&changes);
        assert!(
            shown.contains("new file mode 100644\n--- /dev/null\n+++ b/new.rs\n"),
            "{shown}"
        );
        assert!(
            shown.contains("deleted file mode 100644\n--- a/old.rs\n+++ /dev/null\n"),
            "{shown}"
        );
        let exists = Edit::Create {
            path: String::from("old.rs"),
            create: String::new(),
        };
        assert!(matches!(
            apply(&files, &[exists]).unwrap_err(),
            EditError::Exists { .. }
        ));
        // A file that a delete removed can be written again.
        let again = Edit::Create {
            path: String::from("old.rs"),
            create: String::from("z\n"),
        };
        assert_eq!(
            apply(&files, &[deleted.clone(), again]).unwrap()[0]
                .after
                .as_deref(),
            Some("z\n")
        );
        assert!(matches!(
            apply(&files, &[deleted.clone(), deleted]).unwrap_err(),
            EditError::Missing { index: 2, .. }
        ));
    }

    #[test]
    fn a_bad_shape_is_refused_before_any_file_is_read() {
        let none: &[Edit] = &[];
        assert_eq!(check(none).unwrap_err(), EditError::Empty);
        for path in [".github/workflows/ci.yml", "../x", "/etc/passwd", "a//b"] {
            assert!(matches!(
                check(&[replace(path, "a", "b")]).unwrap_err(),
                EditError::Path { index: 1, .. }
            ));
        }
        assert_eq!(
            check(&[replace("a", "", "b")]).unwrap_err(),
            EditError::EmptyOld { index: 1 }
        );
        let undeleted = Edit::Delete {
            path: String::from("a"),
            delete: false,
        };
        assert_eq!(
            check(&[undeleted]).unwrap_err(),
            EditError::Delete { index: 1 }
        );
    }

    #[test]
    fn a_binary_file_or_no_change_is_refused() {
        let mut files = base(&[("a", "x")]);
        files.insert(String::from("img.png"), Some(Err(())));
        assert!(matches!(
            apply(&files, &[replace("img.png", "a", "b")]).unwrap_err(),
            EditError::Binary { .. }
        ));
        assert_eq!(
            apply(&files, &[replace("a", "x", "x")]).unwrap_err(),
            EditError::NoChange
        );
    }

    #[test]
    fn the_digest_covers_each_part() {
        let one = digest("t", "b", "base", "diff");
        assert_eq!(one.len(), 64);
        assert_eq!(one, digest("t", "b", "base", "diff"));
        for other in [
            digest("t2", "b", "base", "diff"),
            digest("t", "b2", "base", "diff"),
            digest("t", "b", "base2", "diff"),
            digest("t", "b", "base", "diff2"),
            // The separator keeps the parts apart.
            digest("tb", "", "base", "diff"),
        ] {
            assert_ne!(one, other);
        }
    }

    proptest! {
        #[test]
        fn a_unique_text_is_replaced_and_nothing_else_moves(
            prefix in "[a-c]{0,12}",
            old in "[x-z]{1,4}",
            suffix in "[a-c]{0,12}",
            new in "[a-z]{0,6}",
        ) {
            let text = format!("{prefix}{old}{suffix}");
            let files = base(&[("f", &text)]);
            let result = apply(&files, &[replace("f", &old, &new)]);
            if old == new {
                prop_assert_eq!(result.unwrap_err(), EditError::NoChange);
            } else {
                let changes = result.unwrap();
                prop_assert_eq!(changes[0].after.clone(), Some(format!("{prefix}{new}{suffix}")));
            }
        }

        #[test]
        fn a_repeated_text_is_always_refused(
            middle in "[a-c]{0,6}",
            old in "[x-z]{1,4}",
        ) {
            let text = format!("{old}{middle}{old}");
            let files = base(&[("f", &text)]);
            let refused = apply(&files, &[replace("f", &old, "n")]).unwrap_err();
            let is_matches = matches!(refused, EditError::Matches { found: 2, .. });
            prop_assert!(is_matches);
        }
    }
}
