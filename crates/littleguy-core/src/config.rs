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
//!
//! [[camel.route]]
//! id = "inbox"
//! from = "file:/home/mark/lg-inbox?delete=true"
//! prompt = "A file arrived. Summarize it."
//!
//! [san_mateo]
//! bind = "127.0.0.1:3003"
//!
//! [[san_mateo.integration]]
//! key = "google"
//! provider = "google"
//! client_id = "1234.apps.googleusercontent.com"
//! client_secret_env = "LG_GOOGLE_CLIENT_SECRET"
//! scopes = ["https://www.googleapis.com/auth/gmail.readonly"]
//! ```
//!
//! The `[discord]` section is optional. The bot token never goes in the
//! file. It comes from `DISCORD_BOT_TOKEN` only.
//!
//! Each `[[camel.route]]` is a camel route that feeds the assistant. The
//! owner writes it, so its messages need no pairing.
//!
//! The `[san_mateo]` section names the OAuth integrations that San Mateo
//! keeps tokens for. A client secret never goes in the file: the file names
//! the variable that holds it.

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
    /// A camel route id is not 1 to 32 lowercase letters, digits or `-`.
    #[error("`{0}` is not a camel route id: use 1 to 32 of a-z, 0-9 and -")]
    CamelRouteId(String),
    /// Two camel routes have one id.
    #[error("two camel routes have the id `{0}`")]
    CamelRouteTwice(String),
    /// The San Mateo section is not valid.
    #[error(transparent)]
    SanMateo(#[from] SanMateoError),
    /// A camel route reads from a component that littleguy does not run.
    #[error("camel route `{id}` reads from `{from}`: use one of {}", CAMEL_SCHEMES.join(", "))]
    CamelScheme {
        /// The route id.
        id: String,
        /// The `from` URI.
        from: String,
    },
}

/// A San Mateo setting is not valid.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SanMateoError {
    /// The bind address is not an IP address and port.
    #[error("san_mateo.bind `{0}` is not an address such as 127.0.0.1:3003")]
    Bind(String),
    /// The bind address is not a loopback address.
    #[error("san_mateo.bind `{0}` is not a loopback address: OAuth callbacks stay on this machine")]
    NotLoopback(String),
    /// An integration key is not 1 to 100 of `A-Z a-z 0-9 _ . -`.
    #[error("`{0}` is not an integration key: use 1 to 100 of A-Z, a-z, 0-9, _, . and -")]
    Key(String),
    /// Two integrations have one key.
    #[error("two san_mateo integrations have the key `{0}`")]
    KeyTwice(String),
    /// A variable name is not an environment variable name.
    #[error("`{0}` is not an environment variable name")]
    Variable(String),
}

/// The default San Mateo address.
pub const DEFAULT_SAN_MATEO_BIND: &str = "127.0.0.1:3003";

/// The camel components that a route may read from.
pub const CAMEL_SCHEMES: [&str; 2] = ["file", "timer"];

/// The longest camel route id, in bytes.
pub const MAX_CAMEL_ROUTE_ID: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    owner: String,
    system: Option<String>,
    window: Option<u32>,
    #[serde(default)]
    discord: RawDiscord,
    #[serde(default)]
    camel: RawCamel,
    san_mateo: Option<RawSanMateo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSanMateo {
    bind: Option<String>,
    #[serde(default)]
    integration: Vec<RawIntegration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntegration {
    key: String,
    provider: String,
    client_id: Option<String>,
    client_secret_env: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

/// One OAuth integration that San Mateo keeps tokens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    /// The integration key, such as `google`.
    pub key: String,
    /// The provider name in the San Mateo catalog.
    pub provider: String,
    /// The OAuth client id.
    pub client_id: Option<String>,
    /// The variable that holds the OAuth client secret.
    pub client_secret_env: Option<String>,
    /// The scopes to ask for.
    pub scopes: Vec<String>,
}

/// The San Mateo settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanMateoConfig {
    /// The loopback address of the connect pages and the OAuth callback.
    pub bind: std::net::SocketAddr,
    /// The integrations.
    pub integrations: Vec<Integration>,
}

/// Whether `name` is a plain environment variable name.
fn is_variable(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Checks the San Mateo section of a config.
fn san_mateo(raw: RawSanMateo) -> Result<SanMateoConfig, SanMateoError> {
    let text = raw
        .bind
        .unwrap_or_else(|| String::from(DEFAULT_SAN_MATEO_BIND));
    let bind: std::net::SocketAddr = text
        .parse()
        .map_err(|_| SanMateoError::Bind(text.clone()))?;
    if !bind.ip().is_loopback() {
        return Err(SanMateoError::NotLoopback(text));
    }
    let mut seen = std::collections::BTreeSet::new();
    let integrations = raw
        .integration
        .into_iter()
        .map(|i| {
            let plain = !i.key.is_empty()
                && i.key.len() <= 100
                && i.key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
            if !plain {
                return Err(SanMateoError::Key(i.key));
            }
            if !seen.insert(i.key.clone()) {
                return Err(SanMateoError::KeyTwice(i.key));
            }
            if let Some(var) = i.client_secret_env.as_deref().filter(|v| !is_variable(v)) {
                return Err(SanMateoError::Variable(var.to_owned()));
            }
            Ok(Integration {
                key: i.key,
                provider: i.provider,
                client_id: i.client_id.filter(|c| !c.is_empty()),
                client_secret_env: i.client_secret_env,
                scopes: i.scopes,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(SanMateoConfig { bind, integrations })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCamel {
    #[serde(default)]
    route: Vec<RawCamelRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCamelRoute {
    id: String,
    from: String,
    prompt: Option<String>,
}

/// One camel route that feeds the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CamelRoute {
    /// The route id. The sender of its messages is `camel:<id>`.
    pub id: String,
    /// The camel URI that the route reads from, such as `file:/inbox`.
    pub from: String,
    /// Text that goes before each message body.
    pub prompt: Option<String>,
}

impl CamelRoute {
    /// The sender id of the messages of this route.
    ///
    /// # Errors
    ///
    /// Never for a checked route: its id is short and plain.
    pub fn sender(&self) -> Result<SenderId, IdError> {
        SenderId::new(format!("camel:{}", self.id))
    }
}

/// Checks the camel routes of a config.
fn camel_routes(raw: Vec<RawCamelRoute>) -> Result<Vec<CamelRoute>, ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    raw.into_iter()
        .map(|route| {
            let plain = !route.id.is_empty()
                && route.id.len() <= MAX_CAMEL_ROUTE_ID
                && route
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !plain {
                return Err(ConfigError::CamelRouteId(route.id));
            }
            if !seen.insert(route.id.clone()) {
                return Err(ConfigError::CamelRouteTwice(route.id));
            }
            let scheme = route.from.split_once(':').map(|(scheme, _)| scheme);
            if !scheme.is_some_and(|scheme| CAMEL_SCHEMES.contains(&scheme)) {
                return Err(ConfigError::CamelScheme {
                    id: route.id,
                    from: route.from,
                });
            }
            Ok(CamelRoute {
                id: route.id,
                from: route.from,
                prompt: route.prompt.filter(|prompt| !prompt.trim().is_empty()),
            })
        })
        .collect()
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
    /// The camel routes that feed the assistant.
    pub camel: Vec<CamelRoute>,
    /// The San Mateo settings, when the config has the section.
    pub san_mateo: Option<SanMateoConfig>,
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
            camel: Vec::new(),
            san_mateo: None,
        }
    }

    /// Parses and checks a config text.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML, names an unknown
    /// key, holds a bad owner name or Discord user id, sets a zero window,
    /// or holds a bad camel route or San Mateo setting.
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
            camel: camel_routes(raw.camel.route)?,
            san_mateo: raw.san_mateo.map(san_mateo).transpose()?,
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
    fn the_camel_routes_are_read() {
        let config = Config::parse(
            "owner = \"m\"\n\
             [[camel.route]]\nid = \"inbox\"\nfrom = \"file:/in?delete=true\"\nprompt = \"Read it.\"\n\
             [[camel.route]]\nid = \"tick-1\"\nfrom = \"timer:t?period=60000\"\nprompt = \" \"\n",
        )
        .unwrap();
        assert_eq!(
            config.camel,
            vec![
                CamelRoute {
                    id: String::from("inbox"),
                    from: String::from("file:/in?delete=true"),
                    prompt: Some(String::from("Read it.")),
                },
                CamelRoute {
                    id: String::from("tick-1"),
                    from: String::from("timer:t?period=60000"),
                    prompt: None,
                },
            ]
        );
        assert_eq!(config.camel[0].sender().unwrap().as_str(), "camel:inbox");
    }

    #[test]
    fn a_bad_camel_route_is_refused() {
        let route = |id: &str, from: &str| {
            Config::parse(&format!(
                "owner = \"m\"\n[[camel.route]]\nid = \"{id}\"\nfrom = \"{from}\"\n"
            ))
        };
        for id in ["", "Inbox", "a b", &"x".repeat(33)] {
            assert!(
                matches!(route(id, "file:/in"), Err(ConfigError::CamelRouteId(_))),
                "{id}"
            );
        }
        for from in ["kafka:topic", "file", "/in"] {
            let error = route("a", from).unwrap_err();
            assert!(matches!(error, ConfigError::CamelScheme { .. }), "{from}");
            assert!(error.to_string().contains("file, timer"));
        }
        let twice = Config::parse(
            "owner = \"m\"\n[[camel.route]]\nid = \"a\"\nfrom = \"file:/x\"\n\
             [[camel.route]]\nid = \"a\"\nfrom = \"file:/y\"\n",
        );
        assert!(matches!(twice, Err(ConfigError::CamelRouteTwice(_))));
        assert!(Config::parse("owner = \"m\"\n[camel]\nother = 1\n").is_err());
    }

    #[test]
    fn the_san_mateo_section_is_read() {
        assert_eq!(Config::parse("owner = \"m\"").unwrap().san_mateo, None);
        let config = Config::parse("owner = \"m\"\n[san_mateo]\n").unwrap();
        let section = config.san_mateo.unwrap();
        assert_eq!(section.bind.to_string(), DEFAULT_SAN_MATEO_BIND);
        assert_eq!(section.integrations, Vec::new());
        let config = Config::parse(
            "owner = \"m\"\n[san_mateo]\nbind = \"[::1]:4000\"\n\
             [[san_mateo.integration]]\nkey = \"google\"\nprovider = \"google\"\n\
             client_id = \"c\"\nclient_secret_env = \"LG_G_SECRET\"\nscopes = [\"s1\"]\n\
             [[san_mateo.integration]]\nkey = \"gh.work\"\nprovider = \"github\"\nclient_id = \"\"\n",
        )
        .unwrap();
        let section = config.san_mateo.unwrap();
        assert_eq!(section.bind.port(), 4000);
        assert_eq!(
            section.integrations,
            vec![
                Integration {
                    key: String::from("google"),
                    provider: String::from("google"),
                    client_id: Some(String::from("c")),
                    client_secret_env: Some(String::from("LG_G_SECRET")),
                    scopes: vec![String::from("s1")],
                },
                Integration {
                    key: String::from("gh.work"),
                    provider: String::from("github"),
                    client_id: None,
                    client_secret_env: None,
                    scopes: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn a_bad_san_mateo_section_is_refused() {
        let error = |section: &str| match Config::parse(&format!(
            "owner = \"m\"\n[san_mateo]\n{section}"
        )) {
            Err(ConfigError::SanMateo(error)) => error,
            other => panic!("{other:?}"),
        };
        assert!(matches!(error("bind = \"nope\"\n"), SanMateoError::Bind(_)));
        assert!(matches!(
            error("bind = \"0.0.0.0:3003\"\n"),
            SanMateoError::NotLoopback(_)
        ));
        let one = |key: &str, extra: &str| {
            format!("[[san_mateo.integration]]\nkey = \"{key}\"\nprovider = \"p\"\n{extra}")
        };
        assert!(matches!(error(&one("a b", "")), SanMateoError::Key(_)));
        assert!(matches!(error(&one("", "")), SanMateoError::Key(_)));
        let twice = format!("{}{}", one("a", ""), one("a", ""));
        assert!(matches!(error(&twice), SanMateoError::KeyTwice(_)));
        for var in ["lower", "1ABC", "A-B", ""] {
            let extra = format!("client_secret_env = \"{var}\"\n");
            assert!(
                matches!(error(&one("a", &extra)), SanMateoError::Variable(_)),
                "{var}"
            );
        }
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let error = Config::load(Path::new("/nonexistent/lg.toml")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/lg.toml"));
    }
}
