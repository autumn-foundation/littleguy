//! The config file of the daemon.
//!
//! ```toml
//! owner = "mark"
//! system = "You are littleguy, a helpful assistant."
//! window = 40
//!
//! [discord]
//! pairing = true
//! allowed = ["123456789012345678"]
//! ```
//!
//! The `[discord]` section is optional. The bot token never goes in the
//! file. It comes from `DISCORD_BOT_TOKEN` only.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

use crate::ids::{IdError, OwnerName, SenderId};

/// The system prompt when the config names none.
pub const DEFAULT_SYSTEM: &str = "You are littleguy, a small and helpful personal assistant.";

/// The transcript bound when the config names none, in messages.
pub const DEFAULT_WINDOW: u32 = 40;

/// Why a config did not load.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file did not read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The path of the file.
        path: String,
        /// The read error.
        source: std::io::Error,
    },
    /// The text is not valid TOML for this config.
    #[error("the config is not valid: {0}")]
    Parse(#[from] toml::de::Error),
    /// An id in the config is not valid.
    #[error("the config holds a bad id: {0}")]
    Id(#[from] IdError),
    /// The window is zero.
    #[error("window must be at least 1")]
    Window,
    /// A Discord user id is not a snowflake.
    #[error("`{0}` is not a Discord user id")]
    DiscordUser(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    owner: String,
    system: Option<String>,
    window: Option<u32>,
    #[serde(default)]
    discord: RawDiscord,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiscord {
    pairing: Option<bool>,
    #[serde(default)]
    allowed: Vec<String>,
}

/// The Discord settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordConfig {
    /// A stranger who sends a direct message gets a pairing code.
    pub pairing: bool,
    /// The Discord user ids that may talk to the assistant from the start.
    pub allowed: Vec<SenderId>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            pairing: true,
            allowed: Vec::new(),
        }
    }
}

/// The sender id of the Discord user `user_id`.
///
/// # Errors
///
/// Returns an error when `user_id` is not a Discord snowflake: 1 to 20
/// digits.
pub fn discord_sender(user_id: &str) -> Result<SenderId, ConfigError> {
    let digits =
        !user_id.is_empty() && user_id.len() <= 20 && user_id.bytes().all(|b| b.is_ascii_digit());
    if !digits {
        return Err(ConfigError::DiscordUser(user_id.to_owned()));
    }
    Ok(SenderId::new(format!("discord:{user_id}"))?)
}

/// The checked config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The owner. The assistant is `assistant/<owner>`.
    pub owner: OwnerName,
    /// The system prompt of the assistant.
    pub system: String,
    /// The transcript bound of the assistant, in messages.
    pub window: u32,
    /// The Discord settings.
    pub discord: DiscordConfig,
}

impl Config {
    /// A config for `owner` with every default.
    #[must_use]
    pub fn for_owner(owner: OwnerName) -> Self {
        Self {
            owner,
            system: String::from(DEFAULT_SYSTEM),
            window: DEFAULT_WINDOW,
            discord: DiscordConfig::default(),
        }
    }

    /// Parses and checks a config text.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML, names an unknown
    /// key, holds a bad owner name or Discord user id, or sets a zero
    /// window.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: Raw = toml::from_str(text)?;
        let window = raw.window.unwrap_or(DEFAULT_WINDOW);
        if window == 0 {
            return Err(ConfigError::Window);
        }
        let allowed = raw
            .discord
            .allowed
            .iter()
            .map(|id| discord_sender(id))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            owner: OwnerName::new(raw.owner)?,
            system: raw.system.unwrap_or_else(|| String::from(DEFAULT_SYSTEM)),
            window,
            discord: DiscordConfig {
                pairing: raw.discord.pairing.unwrap_or(true),
                allowed,
            },
        })
    }

    /// Reads and checks the config file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not read or does not parse.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_takes_the_defaults() {
        let config = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(config, Config::for_owner(OwnerName::new("mark").unwrap()));
    }

    #[test]
    fn every_key_is_read() {
        let config = Config::parse("owner = \"mark\"\nsystem = \"Be brief.\"\nwindow = 8").unwrap();
        assert_eq!(config.system, "Be brief.");
        assert_eq!(config.window, 8);
    }

    #[test]
    fn a_bad_config_is_refused() {
        assert!(matches!(
            Config::parse("owner = \"a b\""),
            Err(ConfigError::Id(_))
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\nwindow = 0"),
            Err(ConfigError::Window)
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\nchannels = 1"),
            Err(ConfigError::Parse(_))
        ));
        assert!(matches!(Config::parse(""), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_discord_section_is_read() {
        let config = Config::parse(
            "owner = \"m\"\n[discord]\npairing = false\nallowed = [\"123\", \"456\"]",
        )
        .unwrap();
        assert!(!config.discord.pairing);
        let allowed: Vec<&str> = config
            .discord
            .allowed
            .iter()
            .map(SenderId::as_str)
            .collect();
        assert_eq!(allowed, ["discord:123", "discord:456"]);
        let plain = Config::parse("owner = \"m\"").unwrap();
        assert_eq!(plain.discord, DiscordConfig::default());
        assert!(plain.discord.pairing);
    }

    #[test]
    fn a_bad_discord_section_is_refused() {
        assert!(matches!(
            Config::parse("owner = \"m\"\n[discord]\nallowed = [\"me\"]"),
            Err(ConfigError::DiscordUser(_))
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\n[discord]\ntoken = \"x\""),
            Err(ConfigError::Parse(_))
        ));
        assert!(discord_sender("").is_err());
        assert!(discord_sender(&"1".repeat(21)).is_err());
        assert_eq!(discord_sender("42").unwrap().as_str(), "discord:42");
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let error = Config::load(Path::new("/nonexistent/lg.toml")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/lg.toml"));
    }
}
