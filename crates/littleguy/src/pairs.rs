//! The durable record of each channel sender: pairings and delivery cursors.
//!
//! The file `pairings.json` sits next to the database. The harness runtime
//! owns the database, so littleguy keeps its own state in its own file. The
//! daemon is its one writer, because the runtime lock keeps a second daemon
//! out.
//!
//! Each entry names a sender, where its replies go, and the last message
//! whose reply went out. That cursor makes a reply go out once. A message
//! at or before the cursor is skipped. A message after it runs, or answers
//! from the recorded op when it ran before.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use littleguy_channels::chat::is_after;
use littleguy_core::{ChannelId, SenderId};
use serde::{Deserialize, Serialize};

/// The file name, next to the database.
pub const FILE_NAME: &str = "pairings.json";

/// The format version of the file.
const VERSION: u32 = 1;

/// One sender that the daemon has answered or paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The channel.
    pub channel: ChannelId,
    /// The sender.
    pub sender: SenderId,
    /// Where replies go, such as a Discord or Slack DM channel id.
    pub reply_to: String,
    /// The last message whose reply went out. Empty before the first one.
    pub delivered: String,
    /// A pairing code added this sender. The config file did not.
    pub paired: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    entries: Vec<Entry>,
}

/// The entries, and the file that keeps them.
#[derive(Debug)]
pub struct PairStore {
    path: PathBuf,
    entries: BTreeMap<(ChannelId, SenderId), Entry>,
}

impl PairStore {
    /// Reads the store at `path`. A missing file is an empty store.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not read or does not parse.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let entries = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: File = serde_json::from_slice(&bytes)
                    .with_context(|| format!("{} is not valid", path.display()))?;
                file.entries
                    .into_iter()
                    .map(|e| ((e.channel.clone(), e.sender.clone()), e))
                    .collect()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        Ok(Self { path, entries })
    }

    /// The entry of `sender` on `channel`.
    #[must_use]
    pub fn get(&self, channel: &ChannelId, sender: &SenderId) -> Option<&Entry> {
        self.entries.get(&(channel.clone(), sender.clone()))
    }

    /// Every entry, in order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// Adds or replaces the entry of its sender, and saves the file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not save.
    pub fn put(&mut self, entry: Entry) -> Result<()> {
        self.entries
            .insert((entry.channel.clone(), entry.sender.clone()), entry);
        self.save()
    }

    /// Removes the paired entry of `sender`. An entry that the config file
    /// allows stays. Returns whether a paired entry went.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not save.
    pub fn unpair(&mut self, channel: &ChannelId, sender: &SenderId) -> Result<bool> {
        let key = (channel.clone(), sender.clone());
        if !self.entries.get(&key).is_some_and(|e| e.paired) {
            return Ok(false);
        }
        self.entries.remove(&key);
        self.save()?;
        Ok(true)
    }

    /// Records that the reply to `message_id` went out to `reply_to`. The
    /// cursor only moves forward.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not save.
    pub fn delivered(
        &mut self,
        channel: &ChannelId,
        sender: &SenderId,
        reply_to: &str,
        message_id: &str,
    ) -> Result<()> {
        let entry = self
            .entries
            .entry((channel.clone(), sender.clone()))
            .or_insert_with(|| Entry {
                channel: channel.clone(),
                sender: sender.clone(),
                reply_to: reply_to.to_owned(),
                delivered: String::new(),
                paired: false,
            });
        reply_to.clone_into(&mut entry.reply_to);
        if entry.delivered.is_empty() || is_after(message_id, &entry.delivered) {
            message_id.clone_into(&mut entry.delivered);
        }
        self.save()
    }

    /// Whether the reply to `message_id` of `sender` went out already.
    #[must_use]
    pub fn is_delivered(&self, channel: &ChannelId, sender: &SenderId, message_id: &str) -> bool {
        self.get(channel, sender)
            .is_some_and(|e| !e.delivered.is_empty() && !is_after(message_id, &e.delivered))
    }

    fn save(&self) -> Result<()> {
        let file = File {
            version: VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_atomic(&self.path, &bytes)
    }
}

/// The store path next to `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE_NAME)
}

/// Writes `bytes` to `path` through a scratch file, a sync and a rename, so
/// a crash leaves the old file or the new one, never half of one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let scratch = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&scratch)
        .with_context(|| format!("creating {}", scratch.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&scratch, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discord() -> ChannelId {
        ChannelId::new("discord").unwrap()
    }

    fn sender(id: &str) -> SenderId {
        SenderId::new(id).unwrap()
    }

    fn paired(id: &str) -> Entry {
        Entry {
            channel: discord(),
            sender: sender(id),
            reply_to: String::from("dm"),
            delivered: String::from("100"),
            paired: true,
        }
    }

    #[test]
    fn an_entry_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut store = PairStore::open(&path).unwrap();
        assert_eq!(store.entries().count(), 0);
        store.put(paired("discord:1")).unwrap();
        let again = PairStore::open(&path).unwrap();
        assert_eq!(
            again.get(&discord(), &sender("discord:1")),
            Some(&paired("discord:1"))
        );
    }

    #[test]
    fn the_cursor_moves_forward_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PairStore::open(dir.path().join(FILE_NAME)).unwrap();
        let who = sender("discord:2");
        assert!(!store.is_delivered(&discord(), &who, "5"));
        store.delivered(&discord(), &who, "dm2", "10").unwrap();
        store.delivered(&discord(), &who, "dm2", "7").unwrap();
        let entry = store.get(&discord(), &who).unwrap();
        assert_eq!(entry.delivered, "10");
        assert!(!entry.paired);
        assert!(store.is_delivered(&discord(), &who, "10"));
        assert!(store.is_delivered(&discord(), &who, "9"));
        assert!(!store.is_delivered(&discord(), &who, "11"));
        let slack = ChannelId::new("slack").unwrap();
        let user = sender("slack:U1");
        store.delivered(&slack, &user, "D1", "5.000020").unwrap();
        assert!(store.is_delivered(&slack, &user, "5.000003"));
        assert!(!store.is_delivered(&slack, &user, "5.000021"));
    }

    #[test]
    fn only_a_paired_entry_unpairs() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PairStore::open(dir.path().join(FILE_NAME)).unwrap();
        store.put(paired("discord:1")).unwrap();
        store
            .delivered(&discord(), &sender("discord:2"), "dm", "1")
            .unwrap();
        assert!(!store.unpair(&discord(), &sender("discord:2")).unwrap());
        assert!(store.unpair(&discord(), &sender("discord:1")).unwrap());
        assert!(!store.unpair(&discord(), &sender("discord:1")).unwrap());
        assert_eq!(store.entries().count(), 1);
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{not json").unwrap();
        let error = PairStore::open(&path).unwrap_err().to_string();
        assert!(error.contains("not valid"), "{error}");
        assert!(PairStore::open(dir.path()).is_err());
    }

    #[test]
    fn the_store_sits_next_to_the_database() {
        assert_eq!(
            path_for(Path::new("/d/lg.db")),
            Path::new("/d/pairings.json")
        );
        assert_eq!(path_for(Path::new("lg.db")), Path::new("pairings.json"));
    }
}
