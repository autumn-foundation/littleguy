//! Which sections of a new config a running daemon takes (ADR 0018).
//!
//! A daemon reads `lg.toml` again when the file changes, or at
//! `lg reload`. Most sections only shape the assistant, its tools and its
//! routes, so the daemon applies them at once. A few sections start a
//! server, a socket or a background process at start. Those keep their
//! old value until the next start, and the reload names them.
//!
//! The kernel is pure: [`merge`] compares two configs and returns the one
//! to run, with the names of the sections that changed.

use crate::Config;

/// The result of a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// The config to run: the new one, with each restart section kept from
    /// the old one.
    pub config: Config,
    /// The sections that changed and apply now, in file order.
    pub applied: Vec<&'static str>,
    /// The sections that changed and wait for a restart, in file order.
    pub restart: Vec<&'static str>,
    /// The sections that changed and shape the agent profile, in file
    /// order. The daemon takes them, but the running assistant keeps the
    /// profile of its run. autumn-harvest reads the profile once per run,
    /// and a run on SQLite never ends (autumn-harvest #2658).
    pub profile: Vec<&'static str>,
}

impl Merge {
    /// Whether the merge changes nothing that runs.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.applied.is_empty() && self.profile.is_empty()
    }
}

/// Merges `next` into `current`.
///
/// Each live section takes its new value. Each restart section keeps its current value, and its name goes into
/// [`Merge::restart`] when it changed. Each profile section takes its new
/// value, and its name goes into [`Merge::profile`] when it changed.
///
/// The restart sections start something at start. The owner names the
/// agent and its rows. `san_mateo`, `ui` and `hooks` bind a server.
/// `google` needs the San Mateo connections. `sandbox` starts a backend,
/// `mcp` starts the servers, and `fleet` opens the tenant database.
#[must_use]
pub fn merge(current: &Config, mut next: Config) -> Merge {
    let mut applied = Vec::new();
    let mut restart = Vec::new();
    let mut profile = Vec::new();
    macro_rules! profile {
        ($($field:ident),*) => {
            $(if current.$field != next.$field {
                profile.push(stringify!($field));
            })*
        };
    }
    macro_rules! live {
        ($($field:ident),*) => {
            $(if current.$field != next.$field {
                applied.push(stringify!($field));
            })*
        };
    }
    macro_rules! keep {
        ($($field:ident),*) => {
            $(if current.$field != next.$field {
                restart.push(stringify!($field));
                next.$field = current.$field.clone();
            })*
        };
    }
    keep!(owner);
    profile!(system, window);
    live!(discord, slack, camel);
    keep!(san_mateo, google);
    live!(memory, brief);
    profile!(tools);
    keep!(sandbox);
    live!(approvals, flow);
    keep!(mcp);
    live!(refine);
    keep!(ui, hooks, fleet);
    Merge {
        config: next,
        applied,
        restart,
        profile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(&format!("owner = \"mark\"\n{text}")).unwrap()
    }

    #[test]
    fn the_same_config_changes_nothing() {
        let config = parse("system = \"hi\"");
        let merge = merge(&config, config.clone());
        assert!(merge.is_empty());
        assert_eq!(merge.restart, [] as [&str; 0]);
        assert_eq!(merge.config, config);
    }

    #[test]
    fn live_sections_apply_and_restart_sections_wait() {
        let current = parse("system = \"old\"\n[hooks]");
        let next = parse(
            "system = \"new\"\nwindow = 12\n[[camel.route]]\nid = \"inbox\"\nfrom = \"timer:t?period=1000\"\n[ui]\nbind = \"127.0.0.1:9999\"",
        );
        let merge = merge(&current, next);
        assert_eq!(merge.applied, ["camel"]);
        assert_eq!(merge.profile, ["system", "window"]);
        assert_eq!(merge.restart, ["ui", "hooks"]);
        assert_eq!(merge.config.system, "new");
        assert_eq!(merge.config.window, 12);
        assert_eq!(merge.config.camel.len(), 1);
        // The restart sections keep the running values.
        assert_eq!(merge.config.ui, current.ui);
        assert_eq!(merge.config.hooks, current.hooks);
    }

    #[test]
    fn a_new_owner_waits_for_a_restart() {
        let current = parse("");
        let next = Config::parse("owner = \"other\"").unwrap();
        let merge = merge(&current, next);
        assert!(merge.is_empty());
        assert_eq!(merge.restart, ["owner"]);
        assert_eq!(merge.config.owner, current.owner);
    }
}
