//! The cargo features of `lg`, and the check that a config fits the build.
//!
//! Each integration is a cargo feature, so a build without it carries none
//! of its crates. The config still parses every section in any build. A
//! section that needs a feature of this build fails `lg serve` at start,
//! and the error names the feature. A quiet start would hide a missing
//! integration until the owner asks for it.
//!
//! A key that is on by default, such as `tools.web_fetch`, cannot tell an
//! owner's choice from the default. So it only logs.

use anyhow::{Result, bail};
use littleguy_core::Config;

/// One cargo feature of `lg`, with whether this build has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// The feature name in `Cargo.toml`.
    pub name: &'static str,
    /// Whether this build has the feature.
    pub on: bool,
}

/// Every feature of `lg` that turns an integration on, in the order of
/// `Cargo.toml`.
pub const ALL: [Feature; 10] = [
    Feature {
        name: "discord",
        on: cfg!(feature = "discord"),
    },
    Feature {
        name: "slack",
        on: cfg!(feature = "slack"),
    },
    Feature {
        name: "claude",
        on: cfg!(feature = "claude"),
    },
    Feature {
        name: "openai",
        on: cfg!(feature = "openai"),
    },
    Feature {
        name: "web",
        on: cfg!(feature = "web"),
    },
    Feature {
        name: "mcp",
        on: cfg!(feature = "mcp"),
    },
    Feature {
        name: "sandbox",
        on: cfg!(feature = "sandbox"),
    },
    Feature {
        name: "camel",
        on: cfg!(feature = "camel"),
    },
    Feature {
        name: "oauth",
        on: cfg!(feature = "oauth"),
    },
    Feature {
        name: "google",
        on: cfg!(feature = "google"),
    },
];

/// Whether this build has the feature `name`.
#[must_use]
pub fn has(name: &str) -> bool {
    ALL.iter().any(|feature| feature.name == name && feature.on)
}

/// The error text for a part of `lg` that this build leaves out.
#[must_use]
pub fn missing(what: &str, feature: &str) -> String {
    format!("{what} needs the `{feature}` feature, and this lg is built without it")
}

/// Checks that this build has each feature that `config` needs.
///
/// # Errors
///
/// Returns an error that names each section and its missing feature.
pub fn check(config: &Config) -> Result<()> {
    let needs = [
        (!config.camel.is_empty(), "[[camel.route]]", "camel"),
        (config.san_mateo.is_some(), "[san_mateo]", "oauth"),
        (config.google.is_some(), "[google]", "google"),
        (!config.mcp.is_empty(), "[[mcp.server]]", "mcp"),
        (config.sandbox.is_some(), "[sandbox]", "sandbox"),
    ];
    let problems: Vec<String> = needs
        .into_iter()
        .filter(|(used, _, feature)| *used && !has(feature))
        .map(|(_, section, feature)| missing(section, feature))
        .collect();
    if !problems.is_empty() {
        bail!(
            "{}. Build lg with those features, or remove the sections",
            problems.join("; ")
        );
    }
    if !has("web") && (config.tools.web_fetch || config.tools.web_search) {
        tracing::info!("the web tools are off: this lg is built without the `web` feature");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use littleguy_core::OwnerName;

    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    #[test]
    fn the_list_matches_the_build() {
        assert_eq!(has("discord"), cfg!(feature = "discord"));
        assert_eq!(has("google"), cfg!(feature = "google"));
        assert!(!has("nothing"));
        let names: Vec<&str> = ALL.iter().map(|feature| feature.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "each feature is listed once");
    }

    #[test]
    fn the_list_names_each_feature_of_the_manifest() {
        let manifest = include_str!("../Cargo.toml");
        let section = manifest
            .split("[features]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .unwrap();
        let mut declared: Vec<&str> = section
            .lines()
            .filter_map(|line| line.split_once(" = "))
            .map(|(name, _)| name.trim())
            .filter(|name| !name.starts_with('#') && !["default", "full"].contains(name))
            .collect();
        declared.sort_unstable();
        let mut listed: Vec<&str> = ALL.iter().map(|feature| feature.name).collect();
        listed.sort_unstable();
        assert_eq!(declared, listed);
    }

    #[test]
    fn a_config_without_sections_fits_every_build() {
        let config = Config::for_owner(OwnerName::new("mark").unwrap());
        check(&config).unwrap();
    }

    #[test]
    fn each_section_needs_its_feature() {
        let cases = [
            (
                "owner = \"mark\"\n[sandbox]\nbackend = \"process\"",
                "[sandbox]",
                "sandbox",
            ),
            (
                "owner = \"mark\"\n[[mcp.server]]\nid = \"m\"\ncommand = \"m\"",
                "[[mcp.server]]",
                "mcp",
            ),
            (
                "owner = \"mark\"\n[[camel.route]]\nid = \"inbox\"\nfrom = \"file:/tmp/in\"",
                "[[camel.route]]",
                "camel",
            ),
        ];
        for (text, section, feature) in cases {
            let result = check(&parse(text));
            if has(feature) {
                result.unwrap();
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(section), "{error}");
                assert!(error.contains(&format!("`{feature}`")), "{error}");
            }
        }
    }

    #[test]
    fn the_message_names_the_feature() {
        assert_eq!(
            missing("[google]", "google"),
            "[google] needs the `google` feature, and this lg is built without it"
        );
    }
}
