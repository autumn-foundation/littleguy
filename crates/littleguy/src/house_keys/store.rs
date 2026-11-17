//! The sealed file and the key ring that opens it.
//!
//! The file is JSON: a version and a map from each name to its sealed
//! value. `KeyRing::seal` gives `sm1.<key id>.<nonce>.<ciphertext>`, so each
//! value names the key that opens it. The AAD binds each value to its
//! name, so a value copied to another name does not open.
//!
//! A write takes an exclusive lock on a lock file, reads the file, changes
//! it, writes a temporary file with mode `0600`, and renames it over the
//! file. A reader never sees half a file, and two writers never lose a key.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use littleguy_core::house_keys::is_sealable;
use san_mateo::{KeyRing, Secret};
use serde::{Deserialize, Serialize};

use super::{DEFAULT_KEY_ID, KEY_ID_VAR, KEY_VAR, RETIRED_VAR, path_for};

/// The format of the file. A file of another version fails to read.
const VERSION: u32 = 1;

/// The first part of each AAD. A new format gets a new tag.
const AAD_TAG: &str = "littleguy/house-key/v1";

/// The file on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sealed {
    version: u32,
    keys: BTreeMap<String, String>,
}

/// The AAD of the key named `name`.
fn aad(name: &str) -> Vec<u8> {
    san_mateo::crypto::aad(&[AAD_TAG, name])
}

/// The key id that a sealed value names, if it has the sealed form.
fn key_id(sealed: &str) -> Option<&str> {
    let mut parts = sealed.split('.');
    match (parts.next(), parts.next()) {
        (Some(san_mateo::crypto::SEALED_PREFIX), Some(id)) => Some(id),
        _ => None,
    }
}

/// The ring that `env` sets up, or `None` when it has no [`KEY_VAR`].
///
/// [`KEY_VAR`] is the active key, under the id in [`KEY_ID_VAR`] or
/// [`DEFAULT_KEY_ID`]. Each `id=base64` of [`RETIRED_VAR`] joins the ring
/// and opens what it sealed. No error shows a key.
///
/// # Errors
///
/// Returns an error when a key is not 32 bytes of base64, an id is not
/// valid, or two keys share an id.
pub fn ring(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<KeyRing>> {
    let Some(active) = env(KEY_VAR).filter(|key| !key.trim().is_empty()) else {
        return Ok(None);
    };
    let id = env(KEY_ID_VAR)
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_KEY_ID));
    let mut keys = KeyRing::from_base64(&id, active.trim()).with_context(|| {
        format!(
            "{KEY_VAR} must be 32 random bytes in base64, and {KEY_ID_VAR} 1 to 32 of [A-Za-z0-9_-]"
        )
    })?;
    for entry in env(RETIRED_VAR)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((retired, key)) = entry.split_once('=') else {
            bail!("each entry of {RETIRED_VAR} is id=base64");
        };
        let retired = retired.trim();
        keys = keys
            .with_base64_key(retired, key.trim())
            .with_context(|| format!("the retired key `{retired}` of {RETIRED_VAR}"))?;
    }
    Ok(Some(keys))
}

/// The sealed file of one database.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// The store beside `db`.
    #[must_use]
    pub fn beside(db: &Path) -> Self {
        Self { path: path_for(db) }
    }

    /// The path of the file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file, or an empty one when it does not exist.
    fn read(&self) -> Result<Sealed> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Sealed {
                    version: VERSION,
                    keys: BTreeMap::new(),
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        let sealed: Sealed = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a house key file", self.path.display()))?;
        if sealed.version != VERSION {
            bail!(
                "{} has version {}, and this lg reads version {VERSION}",
                self.path.display(),
                sealed.version
            );
        }
        Ok(sealed)
    }

    /// Runs `change` on the file under the lock, then writes the file.
    fn update<T>(&self, change: impl FnOnce(&mut Sealed) -> Result<T>) -> Result<T> {
        let lock_path = self.path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        let mut sealed = self.read()?;
        let out = change(&mut sealed)?;
        self.write(&sealed)?;
        Ok(out)
    }

    /// Writes `sealed` to a temporary file and renames it over the file.
    fn write(&self, sealed: &Sealed) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(sealed)?;
        let mut file: File = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// Each name with the id of the key that sealed it, in order.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not read.
    pub fn list(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .read()?
            .keys
            .into_iter()
            .map(|(name, sealed)| {
                let id = key_id(&sealed).unwrap_or("?").to_owned();
                (name, id)
            })
            .collect())
    }

    /// Seals `value` as `name` under the active key of `ring`.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not sealable, `value` is empty, or
    /// the file does not write.
    pub fn set(&self, ring: &KeyRing, name: &str, value: &Secret) -> Result<()> {
        check_name(name)?;
        if value.expose().trim().is_empty() {
            bail!("the value of {name} is empty");
        }
        let sealed = ring
            .seal(value.expose().as_bytes(), &aad(name))
            .with_context(|| format!("sealing {name}"))?;
        self.update(|file| {
            file.keys.insert(name.to_owned(), sealed);
            Ok(())
        })
    }

    /// Removes `name`. Returns whether the file held it.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not write.
    pub fn remove(&self, name: &str) -> Result<bool> {
        self.update(|file| Ok(file.keys.remove(name).is_some()))
    }

    /// Seals each key again under the active key of `ring`. Returns the
    /// names that moved to that key. A key already under it stays as it is.
    ///
    /// # Errors
    ///
    /// Returns an error when a key does not open with `ring`. Then no key
    /// changes.
    pub fn reseal(&self, ring: &KeyRing) -> Result<Vec<String>> {
        self.update(|file| {
            let mut moved = Vec::new();
            for (name, sealed) in &mut file.keys {
                if key_id(sealed) == Some(ring.active_key_id()) {
                    continue;
                }
                let plain = open_one(ring, name, sealed)?;
                *sealed = ring
                    .seal(plain.expose().as_bytes(), &aad(name))
                    .with_context(|| format!("sealing {name}"))?;
                moved.push(name.clone());
            }
            Ok(moved)
        })
    }

    /// Opens each key with `ring`.
    ///
    /// # Errors
    ///
    /// Returns an error that names the first key that does not open, with
    /// the id of its key.
    pub fn open_all(&self, ring: &KeyRing) -> Result<BTreeMap<String, Secret>> {
        self.read()?
            .keys
            .iter()
            .map(|(name, sealed)| Ok((name.clone(), open_one(ring, name, sealed)?)))
            .collect()
    }
}

/// Fails unless the store takes `name`.
fn check_name(name: &str) -> Result<()> {
    if !is_sealable(name) {
        bail!(
            "`{name}` is not a house key name: use 1 to {} of [A-Z0-9_], no digit first, \
             and nothing that starts with {}",
            littleguy_core::house_keys::MAX_NAME,
            littleguy_core::house_keys::RESERVED_PREFIX
        );
    }
    Ok(())
}

/// Opens the value of `name`.
fn open_one(ring: &KeyRing, name: &str, sealed: &str) -> Result<Secret> {
    let id = key_id(sealed).unwrap_or("?");
    let bytes = ring.open(sealed, &aad(name)).map_err(|_| {
        anyhow::anyhow!(
            "{name} does not open: it is sealed with key `{id}`. The ring holds `{}` and \
             each id of {RETIRED_VAR}, and a changed byte does not open either",
            ring.active_key_id()
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{name} is not text"))?;
    Ok(Secret::new(text))
}
