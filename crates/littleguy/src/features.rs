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
pub const ALL: [Feature; 31] = [
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
        name: "scripts",
        on: cfg!(feature = "scripts"),
    },
    Feature {
        name: "auto-approve",
        on: cfg!(feature = "auto-approve"),
    },
    Feature {
        name: "history",
        on: cfg!(feature = "history"),
    },
    Feature {
        name: "aletheia",
        on: cfg!(feature = "aletheia"),
    },
    Feature {
        name: "refine",
        on: cfg!(feature = "refine"),
    },
    Feature {
        name: "plans",
        on: cfg!(feature = "plans"),
    },
    Feature {
        name: "heartbeat",
        on: cfg!(feature = "heartbeat"),
    },
    Feature {
        name: "followups",
        on: cfg!(feature = "followups"),
    },
    Feature {
        name: "reflection",
        on: cfg!(feature = "reflection"),
    },
    Feature {
        name: "skills",
        on: cfg!(feature = "skills"),
    },
    Feature {
        name: "self-debug",
        on: cfg!(feature = "self-debug"),
    },
    Feature {
        name: "council",
        on: cfg!(feature = "council"),
    },
    Feature {
        name: "compaction",
        on: cfg!(feature = "compaction"),
    },
    Feature {
        name: "research",
        on: cfg!(feature = "research"),
    },
    Feature {
        name: "search-notes",
        on: cfg!(feature = "search-notes"),
    },
    Feature {
        name: "grep-files",
        on: cfg!(feature = "grep-files"),
    },
    Feature {
        name: "notes",
        on: cfg!(feature = "notes"),
    },
    Feature {
        name: "camel",
        on: cfg!(feature = "camel"),
    },
    Feature {
        name: "sealed-keys",
        on: cfg!(feature = "sealed-keys"),
    },
    Feature {
        name: "oauth",
        on: cfg!(feature = "oauth"),
    },
    Feature {
        name: "google",
        on: cfg!(feature = "google"),
    },
    Feature {
        name: "api",
        on: cfg!(feature = "api"),
    },
    Feature {
        name: "patches",
        on: cfg!(feature = "patches"),
    },
    Feature {
        name: "fleet",
        on: cfg!(feature = "fleet"),
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
    // `[fleet]` replicates one SQLite file. The AletheiaDB directory waits
    // for the database fleet of F2 (ADR 0047).
    if config.fleet.is_some()
        && config.memory.backend == Some(littleguy_core::MemoryBackend::Aletheia)
    {
        bail!(
            "memory.backend = \"aletheia\" does not run with [fleet] yet: the fleet replicates \
             one SQLite file. Set memory.backend = \"sqlite\", or remove [fleet]"
        );
    }
    let needs = [
        (!config.camel.is_empty(), "[[camel.route]]", "camel"),
        (config.san_mateo.is_some(), "[san_mateo]", "oauth"),
        (config.google.is_some(), "[google]", "google"),
        (config.api.is_some(), "[api]", "api"),
        (config.patches.is_some(), "[patches]", "patches"),
        (!config.mcp.is_empty(), "[[mcp.server]]", "mcp"),
        (config.sandbox.is_some(), "[sandbox]", "sandbox"),
        (config.scripts.is_some(), "[scripts]", "scripts"),
        (
            config
                .sandbox
                .as_ref()
                .is_some_and(|sandbox| sandbox.approve == littleguy_core::SandboxApprove::Auto),
            "approve = \"auto\"",
            "auto-approve",
        ),
        (config.refine.is_some(), "[refine]", "refine"),
        (config.heartbeat.is_some(), "[heartbeat]", "heartbeat"),
        (config.skills.dir.is_some(), "[skills]", "skills"),
        (config.council.is_some(), "[council]", "council"),
        (config.compaction.is_some(), "[compaction]", "compaction"),
        (config.research.is_some(), "[research]", "research"),
        (config.fleet.is_some(), "[fleet]", "fleet"),
        (
            config.memory.backend == Some(littleguy_core::MemoryBackend::Aletheia),
            "memory.backend = \"aletheia\"",
            "aletheia",
        ),
        (
            config.memory.embeddings.is_some(),
            "[memory.embeddings]",
            "aletheia",
        ),
        (
            config.memory.embeddings.is_some(),
            "[memory.embeddings]",
            "openai",
        ),
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
    if !has("reflection") && config.memory.enabled && config.memory.reflect {
        tracing::info!(
            "the reflection after consolidation is off: this lg is built without the `reflection` feature"
        );
    }
    if !has("self-debug") && config.tools.runs {
        tracing::info!(
            "the self-debug tools are off: this lg is built without the `self-debug` feature"
        );
    }
    if !has("plans") && config.tools.plans {
        tracing::info!("goals and ideas are off: this lg is built without the `plans` feature");
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
            ("owner = \"mark\"\n[refine]", "[refine]", "refine"),
            ("owner = \"mark\"\n[heartbeat]", "[heartbeat]", "heartbeat"),
            (
                "owner = \"mark\"\n[skills]\ndir = \"/tmp/skills\"",
                "[skills]",
                "skills",
            ),
            ("owner = \"mark\"\n[council]", "[council]", "council"),
            (
                "owner = \"mark\"\n[compaction]",
                "[compaction]",
                "compaction",
            ),
            (
                "owner = \"mark\"\n[research]\nmax_tokens_per_call = 1000",
                "[research]",
                "research",
            ),
            (
                "owner = \"mark\"\n[[camel.route]]\nid = \"inbox\"\nfrom = \"file:/tmp/in\"",
                "[[camel.route]]",
                "camel",
            ),
            (
                "owner = \"mark\"\n[memory]\nbackend = \"aletheia\"",
                "memory.backend",
                "aletheia",
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
    fn embeddings_need_aletheia_and_openai() {
        let config = parse(
            "owner = \"mark\"\n[memory.embeddings]\nurl = \"https://e.com/v1\"\nmodel = \"m\"\ndimensions = 8",
        );
        let result = check(&config);
        if has("aletheia") && has("openai") {
            result.unwrap();
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("[memory.embeddings]"), "{error}");
        }
    }

    #[test]
    fn aletheia_memory_does_not_run_on_a_fleet_yet() {
        let config = parse(
            "owner = \"mark\"\n[memory]\nbackend = \"aletheia\"\n[fleet.replica]\npath = \"/tmp/replica\"",
        );
        let error = check(&config).unwrap_err().to_string();
        assert!(error.contains("memory.backend"), "{error}");
        // A fleet with no backend keeps the memory in its tenant database.
        let plain = parse("owner = \"mark\"\n[fleet.replica]\npath = \"/tmp/replica\"");
        assert_eq!(
            crate::memory::backend(&plain),
            littleguy_core::MemoryBackend::Sqlite
        );
    }

    #[test]
    fn the_default_backend_follows_the_build() {
        let config = Config::for_owner(OwnerName::new("mark").unwrap());
        let expected = if has("aletheia") {
            littleguy_core::MemoryBackend::Aletheia
        } else {
            littleguy_core::MemoryBackend::Sqlite
        };
        assert_eq!(crate::memory::backend(&config), expected);
    }

    #[test]
    fn the_message_names_the_feature() {
        assert_eq!(
            missing("[google]", "google"),
            "[google] needs the `google` feature, and this lg is built without it"
        );
    }
}
