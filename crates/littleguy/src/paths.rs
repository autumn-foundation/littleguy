//! The default paths of the config, the database and the socket.
//!
//! They follow the XDG base directories, with `$HOME` as the fallback.

use std::path::PathBuf;

use anyhow::{Result, bail};

/// The environment that the defaults read.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `$HOME`.
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`.
    pub config_home: Option<PathBuf>,
    /// `$XDG_DATA_HOME`.
    pub data_home: Option<PathBuf>,
}

impl Env {
    /// Reads the process environment. An empty variable counts as unset.
    #[must_use]
    pub fn from_process() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: var("HOME"),
            config_home: var("XDG_CONFIG_HOME"),
            data_home: var("XDG_DATA_HOME"),
        }
    }

    fn base(&self, xdg: Option<&PathBuf>, fallback: &str) -> Result<PathBuf> {
        if let Some(dir) = xdg {
            return Ok(dir.join("littleguy"));
        }
        let Some(home) = &self.home else {
            bail!("set HOME, or pass the path explicitly");
        };
        Ok(home.join(fallback).join("littleguy"))
    }

    /// `<config home>/littleguy/lg.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `XDG_CONFIG_HOME` nor `HOME` is set.
    pub fn config(&self) -> Result<PathBuf> {
        Ok(self
            .base(self.config_home.as_ref(), ".config")?
            .join("lg.toml"))
    }

    /// `<data home>/littleguy/lg.db`.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `XDG_DATA_HOME` nor `HOME` is set.
    pub fn db(&self) -> Result<PathBuf> {
        Ok(self
            .base(self.data_home.as_ref(), ".local/share")?
            .join("lg.db"))
    }

    /// `<data home>/littleguy/lg.sock`.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `XDG_DATA_HOME` nor `HOME` is set.
    pub fn socket(&self) -> Result<PathBuf> {
        Ok(self
            .base(self.data_home.as_ref(), ".local/share")?
            .join("lg.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_wins_over_home() {
        let env = Env {
            home: Some(PathBuf::from("/h")),
            config_home: Some(PathBuf::from("/c")),
            data_home: Some(PathBuf::from("/d")),
        };
        assert_eq!(env.config().unwrap(), PathBuf::from("/c/littleguy/lg.toml"));
        assert_eq!(env.db().unwrap(), PathBuf::from("/d/littleguy/lg.db"));
        assert_eq!(env.socket().unwrap(), PathBuf::from("/d/littleguy/lg.sock"));
    }

    #[test]
    fn home_is_the_fallback() {
        let env = Env {
            home: Some(PathBuf::from("/h")),
            ..Env::default()
        };
        assert_eq!(
            env.config().unwrap(),
            PathBuf::from("/h/.config/littleguy/lg.toml")
        );
        assert_eq!(
            env.db().unwrap(),
            PathBuf::from("/h/.local/share/littleguy/lg.db")
        );
    }

    #[test]
    fn no_home_is_an_error() {
        assert!(Env::default().db().is_err());
    }
}
