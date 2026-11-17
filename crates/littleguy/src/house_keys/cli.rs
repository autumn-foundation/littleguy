//! `lg keys`: the owner seals, lists, removes and reseals house keys.
//!
//! Each command opens the sealed file directly, so it works while the
//! daemon is down. A value never comes from an argument, so it never shows
//! in a process list or a shell history. The daemon reads the file at
//! start, so a change applies at the next start of `lg serve`.

use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use littleguy_core::Config;
use littleguy_core::house_keys::is_sealable;
use san_mateo::{KeyRing, Secret};

use super::{Env, KEY_VAR, KNOWN, Store, config_names, ring};

/// The line that each command that changes the file ends with.
const RESTART: &str = "Restart lg serve to use the change.";

/// The `lg keys` commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum KeysCommand {
    /// Seal the value on standard input as NAME, such as
    /// `ANTHROPIC_API_KEY`. One trailing line break is dropped.
    Set {
        /// The variable name of the key.
        name: String,
    },
    /// Seal each named variable of the environment. With no name, each
    /// known house key and each variable that the config names.
    Import {
        /// The variable names. Empty means every known one that is set.
        names: Vec<String>,
    },
    /// Print each sealed name with the id of its key. Never a value.
    List,
    /// Remove one sealed key.
    Remove {
        /// The variable name of the key.
        name: String,
    },
    /// Seal each key again under the active key, after a rotation.
    Reseal,
}

impl KeysCommand {
    /// Runs the command on `store`. `config` names more variables for
    /// `import`. `env` is the environment, and `input` is standard input.
    /// Returns the text to print.
    ///
    /// # Errors
    ///
    /// Returns an error when the ring is missing for a command that seals,
    /// a name is not valid, or the file does not read or write.
    pub fn run(
        self,
        store: &Store,
        config: Option<&Config>,
        env: &Env,
        input: impl Read,
    ) -> Result<String> {
        match self {
            Self::Set { name } => {
                let ring = need_ring(env)?;
                let value = read_value(input)?;
                store.set(&ring, &name, &value)?;
                Ok(format!(
                    "sealed {name} with key `{}` in {}. {RESTART}",
                    ring.active_key_id(),
                    store.path().display()
                ))
            }
            Self::Import { names } => import(store, config, env, names),
            Self::List => {
                let keys = store.list()?;
                if keys.is_empty() {
                    return Ok(format!("no sealed keys in {}", store.path().display()));
                }
                Ok(keys
                    .iter()
                    .map(|(name, id)| format!("{name}\t{id}"))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            Self::Remove { name } => {
                if store.remove(&name)? {
                    Ok(format!("removed {name}. {RESTART}"))
                } else {
                    bail!("no sealed key {name}")
                }
            }
            Self::Reseal => {
                let ring = need_ring(env)?;
                let moved = store.reseal(&ring)?;
                let id = ring.active_key_id();
                if moved.is_empty() {
                    Ok(format!("each key is sealed with key `{id}` already"))
                } else {
                    Ok(format!(
                        "sealed {} again with key `{id}`. {RESTART}",
                        moved.join(", ")
                    ))
                }
            }
        }
    }
}

/// The ring of `env`, which a command that seals needs.
fn need_ring(env: &Env) -> Result<KeyRing> {
    ring(env.as_ref())?.with_context(|| {
        format!("set {KEY_VAR}, 32 random bytes in base64: it seals the house keys")
    })
}

/// The value on `input`, with one trailing line break dropped.
fn read_value(mut input: impl Read) -> Result<Secret> {
    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .context("reading the value from standard input")?;
    let value = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    if value.trim().is_empty() {
        bail!("standard input is empty: pipe the value in, such as `lg keys set NAME < file`");
    }
    Ok(Secret::new(value))
}

/// Seals each of `names` that `env` sets. With no name, each known house
/// key and each variable of `config`.
fn import(store: &Store, config: Option<&Config>, env: &Env, names: Vec<String>) -> Result<String> {
    let ring = need_ring(env)?;
    let asked = !names.is_empty();
    let names = if asked {
        names
    } else {
        let mut all: Vec<String> = KNOWN.iter().map(|name| (*name).to_owned()).collect();
        all.extend(config.map(config_names).unwrap_or_default());
        all.sort();
        all.dedup();
        all
    };
    let mut found = Vec::new();
    let mut unset = Vec::new();
    for name in names {
        if !is_sealable(&name) {
            if asked {
                bail!("`{name}` is not a house key name, so nothing was sealed");
            }
            continue;
        }
        match env(&name).filter(|value| !value.trim().is_empty()) {
            Some(value) => found.push((name, Secret::new(value))),
            None => unset.push(name),
        }
    }
    // A name that the owner asked for and that is unset is a mistake, so
    // nothing is sealed.
    if asked && !unset.is_empty() {
        bail!(
            "not set in the environment: {}. Nothing was sealed",
            unset.join(", ")
        );
    }
    let mut sealed = Vec::new();
    for (name, value) in found {
        store.set(&ring, &name, &value)?;
        sealed.push(name);
    }
    if sealed.is_empty() {
        return Ok(String::from(
            "no house key is set in the environment, so nothing was sealed",
        ));
    }
    Ok(format!(
        "sealed {} with key `{}` in {}.\nRemove each one from the environment, then restart lg serve.",
        sealed.join(", "),
        ring.active_key_id(),
        store.path().display()
    ))
}
