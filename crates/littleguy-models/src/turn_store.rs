//! Small file helpers for the raw turns of [`ClaudeModel`](crate::ClaudeModel).
//!
//! A turn file is a cache of blocks that the API wants back. A lost file
//! costs a replay, not data. So the helpers write through a rename and
//! refuse a symlink, and the reader turns every failure into a miss.

use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The SHA-256 of `key`, in hex.
pub fn hashed(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// The bytes of the regular file at `path`, or `None` when it does not
/// exist.
///
/// # Errors
///
/// Returns an error when `path` is a symlink or not a regular file, or a
/// read fails.
pub fn read_regular(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    std::fs::read(path).map(Some)
}

/// Write `bytes` to `path` through a scratch file and a rename.
///
/// The scratch file is new. An old file or a symlink at its name is removed
/// first, so a planted link cannot redirect the write. The data is synced
/// before the rename, and the directory after it.
///
/// # Errors
///
/// Returns an error when a write or the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    // Two threads of one process get two scratch names.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let scratch = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let failed = |e: std::io::Error| {
        let _ = std::fs::remove_file(&scratch);
        format!("cannot write {}: {e}", path.display())
    };
    let _ = std::fs::remove_file(&scratch);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&scratch)
        .map_err(failed)?;
    file.write_all(bytes).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    drop(file);
    std::fs::rename(&scratch, path).map_err(failed)?;
    if let Some(dir) = path.parent()
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_is_hex_sha256() {
        assert_eq!(
            hashed("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_written_file_reads_back_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        assert_eq!(read_regular(&path).unwrap(), None);
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(read_regular(&path).unwrap(), Some(b"two".to_vec()));
    }

    #[test]
    fn a_symlink_or_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_regular(&link).is_err());
        assert!(read_regular(dir.path()).is_err());
    }

    #[test]
    fn a_path_with_no_file_name_is_an_error() {
        assert!(write_atomic(Path::new("/"), b"x").is_err());
    }

    #[test]
    fn a_write_into_a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no").join("t.json");
        assert!(
            write_atomic(&path, b"x")
                .unwrap_err()
                .contains("cannot write")
        );
    }
}
