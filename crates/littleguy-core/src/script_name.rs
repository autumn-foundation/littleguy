//! The name kernel of a saved script (ADR 0045): which names the assistant
//! may give a script.
//!
//! The daemon writes an accepted script to `<dir>/<name>.<ext>`. So a name
//! must stay one path component inside the scripts folder. It must never
//! name a hidden file, such as the retired folder `.retired`.
//!
//! A name has 1 to [`MAX_SCRIPT_NAME`] bytes. Each byte is a lower-case
//! ASCII letter, a digit or `-`. The first byte is not `-`, so a name never
//! reads as a flag. A name then holds no `/`, no `.`, no `\` and no control
//! byte.
//!
//! `formal/verus/src/script_name.rs` proves that the kernel is exactly that
//! reading. Each body between the mirror markers matches the proof.

/// The longest script name, in bytes.
pub const MAX_SCRIPT_NAME: usize = 64;

/// Whether `b` may appear in a script name: `a`-`z`, `0`-`9` or `-`.
const fn is_name_byte(b: u8) -> bool {
    // mirror-begin is_name_byte
    (97 <= b && b <= 122) || (48 <= b && b <= 57) || b == 45
    // mirror-end is_name_byte
}

/// Whether each byte of `p` from `i` on may appear in a name. The
/// recursion is as deep as the name is long.
const fn all_name_bytes(p: &[u8], i: usize) -> bool {
    // mirror-begin all_name_bytes
    i >= p.len() || (is_name_byte(p[i]) && all_name_bytes(p, i + 1))
    // mirror-end all_name_bytes
}

/// [`is_script_name`] on the bytes of a name.
#[must_use]
pub const fn is_script_name_bytes(p: &[u8]) -> bool {
    // mirror-begin is_script_name_bytes
    !p.is_empty() && p.len() <= 64 && p[0] != 45 && all_name_bytes(p, 0)
    // mirror-end is_script_name_bytes
}

/// Whether the assistant may give a script the name `name`.
#[must_use]
pub const fn is_script_name(name: &str) -> bool {
    is_script_name_bytes(name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_bound_is_the_proved_bound() {
        assert_eq!(MAX_SCRIPT_NAME, 64);
        assert!(is_script_name(&"a".repeat(MAX_SCRIPT_NAME)));
        assert!(!is_script_name(&"a".repeat(MAX_SCRIPT_NAME + 1)));
    }

    #[test]
    fn plain_names_pass() {
        for name in ["walk-repo", "a", "0", "x-1-y", "repo-walk-2", "a--b"] {
            assert!(is_script_name(name), "{name} is a name");
        }
    }

    #[test]
    fn a_name_that_leaves_the_folder_or_hides_is_refused() {
        for name in [
            "",
            "-rf",
            ".retired",
            "..",
            "../x",
            "a/b",
            "a.py",
            "a\\b",
            "A",
            "walk_repo",
            "a b",
            "a\nb",
            "a\0b",
            "é",
        ] {
            assert!(!is_script_name(name), "{name:?} is refused");
        }
    }

    /// The plain reading of a script name, for the property test.
    fn plain(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= MAX_SCRIPT_NAME
            && !name.starts_with('-')
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    proptest! {
        #[test]
        fn the_kernel_is_the_plain_reading(name in "[a-zA-Z0-9./\\\\_\t -]{0,70}") {
            prop_assert_eq!(is_script_name(&name), plain(&name));
        }

        #[test]
        fn an_accepted_name_is_one_plain_file_name(name in "[a-z0-9.-]{0,70}") {
            if is_script_name(&name) {
                let file = format!("{name}.py");
                let path = std::path::Path::new(&file);
                prop_assert_eq!(path.components().count(), 1);
                prop_assert!(!file.starts_with('.'));
            }
        }
    }
}
