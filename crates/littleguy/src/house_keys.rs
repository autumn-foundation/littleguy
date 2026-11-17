//! House keys sealed with the key ring of `LG_SAN_MATEO_KEY` (ADR 0038).
//!
//! A house key is a secret that littleguy sends itself: a model key, the
//! search key, a chat token, or a variable that the config names. Each one
//! used to live in the environment. Now the ring of [`KEY_VAR`] seals each
//! one into [`FILE`] beside the database, so the environment holds one
//! secret instead of many.
//!
//! [`HouseKeys`] is the one lookup of the daemon. It reads a secret by its
//! variable name. A sealed value wins over the environment
//! (`littleguy_core::house_keys::source`). A name that the file does not
//! hold comes from the environment, so a setup with no file works as
//! before.
//!
//! The owner writes the file with `lg keys` ([`cli`]). The daemon reads it
//! at start only.

#[cfg(feature = "sealed-keys")]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "sealed-keys")]
use anyhow::Context;
use anyhow::Result;
use littleguy_core::house_keys::{Source, source};

#[cfg(feature = "sealed-keys")]
pub mod cli;
#[cfg(feature = "sealed-keys")]
mod store;

#[cfg(feature = "sealed-keys")]
pub use store::{Store, ring};

/// The variable that holds the active key of the ring: 32 random bytes in
/// base64. It is the one bootstrap secret.
#[cfg(feature = "sealed-keys")]
pub const KEY_VAR: &str = "LG_SAN_MATEO_KEY";

/// The variable that holds the id of [`KEY_VAR`]. A rotation changes it.
#[cfg(feature = "sealed-keys")]
pub const KEY_ID_VAR: &str = "LG_SAN_MATEO_KEY_ID";

/// The variable that holds the retired keys that still open data:
/// `id=base64,id=base64`. It is set only while a rotation runs.
#[cfg(feature = "sealed-keys")]
pub const RETIRED_VAR: &str = "LG_SAN_MATEO_KEY_RETIRED";

/// The id of [`KEY_VAR`] when [`KEY_ID_VAR`] is unset. San Mateo sealed its
/// first tokens under this id.
#[cfg(feature = "sealed-keys")]
pub const DEFAULT_KEY_ID: &str = "k1";

/// The sealed file, beside `lg.db`.
pub const FILE: &str = "house-keys.json";

/// The house keys that each build knows by name. `lg keys import` seals
/// these, and the variables that the config names.
#[cfg(feature = "sealed-keys")]
pub const KNOWN: [&str; 8] = [
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    crate::tools::SEARCH_KEY_ENV,
    "DISCORD_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "SLACK_APP_TOKEN",
    "SLACK_SIGNING_SECRET",
];

/// Reads one environment variable.
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The environment of this process.
#[must_use]
pub fn process_env() -> Env {
    Arc::new(|name: &str| std::env::var(name).ok())
}

/// The sealed file beside `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent().unwrap_or_else(|| Path::new(".")).join(FILE)
}

/// The secrets of the daemon: the opened house keys, then the environment.
#[derive(Clone)]
pub struct HouseKeys {
    /// Each opened house key by name. A `Secret` never shows in a `Debug`
    /// line.
    #[cfg(feature = "sealed-keys")]
    sealed: BTreeMap<String, san_mateo::Secret>,
    env: Env,
}

impl std::fmt::Debug for HouseKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("HouseKeys");
        #[cfg(feature = "sealed-keys")]
        out.field("sealed", &self.sealed_names());
        out.finish_non_exhaustive()
    }
}

impl Default for HouseKeys {
    /// The environment of this process, with no sealed key.
    fn default() -> Self {
        Self::env_only(process_env())
    }
}

impl HouseKeys {
    /// A lookup with no sealed key: each secret comes from `env`.
    #[must_use]
    pub fn env_only(env: Env) -> Self {
        Self {
            #[cfg(feature = "sealed-keys")]
            sealed: BTreeMap::new(),
            env,
        }
    }

    /// Opens the sealed file beside `db` with the ring of `env`. With no
    /// file, each secret comes from `env`. The log names each variable
    /// that is sealed and also set in `env`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file holds keys and `env` has no
    /// [`KEY_VAR`], when a key does not open with the ring, or when the
    /// file does not read.
    #[cfg(feature = "sealed-keys")]
    pub fn load(db: &Path, env: Env) -> Result<Self> {
        let store = Store::beside(db);
        let names = store.list()?;
        if names.is_empty() {
            return Ok(Self::env_only(env));
        }
        let ring = ring(env.as_ref())?.with_context(|| {
            format!(
                "{} holds sealed keys: set {KEY_VAR} to open them",
                store.path().display()
            )
        })?;
        let sealed = store.open_all(&ring)?;
        let keys = Self { sealed, env };
        for name in keys.shadowed() {
            tracing::warn!(
                "{name} is sealed and also set in the environment. The sealed value runs: unset {name}"
            );
        }
        tracing::info!(sealed = ?keys.sealed_names(), "the house keys are open");
        Ok(keys)
    }

    /// Takes `env` for each secret. This build has no ring, so a sealed
    /// file beside `db` fails the start: the build would ignore its keys.
    ///
    /// # Errors
    ///
    /// Returns an error when the sealed file exists.
    #[cfg(not(feature = "sealed-keys"))]
    pub fn load(db: &Path, env: Env) -> Result<Self> {
        let path = path_for(db);
        if path.exists() {
            anyhow::bail!(crate::features::missing(
                &format!("{}", path.display()),
                "sealed-keys"
            ));
        }
        Ok(Self::env_only(env))
    }

    /// The secret named `name`: its sealed value, else the environment.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let env = (self.env)(name);
        match source(self.sealed_value(name).is_some(), env.is_some()) {
            Source::Sealed => self.sealed_value(name).map(str::to_owned),
            Source::Env => env,
            Source::Missing => None,
        }
    }

    /// A lookup function for the parts that take one.
    #[must_use]
    pub fn lookup(self: &Arc<Self>) -> Env {
        let keys = Arc::clone(self);
        Arc::new(move |name: &str| keys.get(name))
    }

    /// The names of the sealed keys, in order.
    #[cfg(feature = "sealed-keys")]
    #[must_use]
    pub fn sealed_names(&self) -> Vec<&str> {
        self.sealed.keys().map(String::as_str).collect()
    }

    /// The sealed names that the environment sets too.
    #[cfg(feature = "sealed-keys")]
    #[must_use]
    pub fn shadowed(&self) -> Vec<&str> {
        self.sealed_names()
            .into_iter()
            .filter(|name| (self.env)(name).is_some())
            .collect()
    }

    #[cfg(feature = "sealed-keys")]
    fn sealed_value(&self, name: &str) -> Option<&str> {
        self.sealed.get(name).map(san_mateo::Secret::expose)
    }

    #[cfg(not(feature = "sealed-keys"))]
    #[expect(
        clippy::unused_self,
        reason = "the sealed build reads self, and both builds share one signature"
    )]
    #[expect(
        clippy::missing_const_for_fn,
        reason = "the sealed build reads a map, so the shared signature is not const"
    )]
    fn sealed_value(&self, _name: &str) -> Option<&str> {
        None
    }
}

/// The variables of `config` that hold a secret: the hook token, each
/// client secret, and each MCP variable. `lg keys import` seals them.
#[must_use]
#[cfg(feature = "sealed-keys")]
pub fn config_names(config: &littleguy_core::Config) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(hooks) = &config.hooks {
        names.push(hooks.token_env.clone());
    }
    if let Some(section) = &config.san_mateo {
        names.extend(
            section
                .integrations
                .iter()
                .filter_map(|i| i.client_secret_env.clone()),
        );
    }
    for server in &config.mcp {
        match &server.transport {
            littleguy_core::McpTransport::Stdio { env, .. } => names.extend(env.iter().cloned()),
            littleguy_core::McpTransport::Http { bearer_env, .. } => {
                names.extend(bearer_env.iter().cloned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
#[cfg(feature = "sealed-keys")]
mod tests;

#[cfg(test)]
#[cfg(not(feature = "sealed-keys"))]
mod plain_tests {
    use super::*;

    #[test]
    fn a_build_without_the_ring_reads_the_environment_and_refuses_a_sealed_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let env: Env = Arc::new(|name: &str| (name == "X").then(|| String::from("v")));
        let keys = HouseKeys::load(&db, Arc::clone(&env)).unwrap();
        assert_eq!(keys.get("X").as_deref(), Some("v"));
        assert_eq!(keys.get("Y"), None);
        std::fs::write(path_for(&db), "{}").unwrap();
        let error = HouseKeys::load(&db, env).unwrap_err().to_string();
        assert!(error.contains("sealed-keys"), "{error}");
    }
}
