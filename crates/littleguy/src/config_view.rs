//! `config_summary`: what the daemon loaded, as the assistant may see it.
//!
//! The assistant could not tell a part that is off from a part that is
//! broken. This tool answers from the config that the daemon runs, not from
//! the file, so it holds after a reload too. It also names the features of
//! this build, so "off in the config" and "not in this build" differ.
//!
//! What it prints goes into a prompt, and maybe into a chat. So each field
//! here is chosen by hand: an allow list, never a dump of the struct. It
//! leaves out every user id, the approver, the owner, the prompts, paths,
//! the transport of an MCP server (a URL can hold a token), and the names
//! of environment variables. Of a secret it says at most whether it is set.
//!
//! It also says which boot and which build it is. Durable replay hides a
//! restart from the assistant: a run keeps its start time across one. So
//! the summary dates the process with `boot` and the binary with
//! `build.version`. A reload rebuilds this tool (ADR 0018), and a reload is
//! not a restart. So the boot is captured once per process and passed in,
//! never read from the clock here.

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, SecondsFormat, Utc};

use autumn_harvest_agent::{FnTool, Tool, ToolEffect};
use littleguy_core::{Config, HeartbeatDelivery, McpTransport, Replica, SandboxBackend};
use serde_json::{Value, json};

/// The version of this binary, from its crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The id of one boot of the daemon process: 16 hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootId(String);

impl BootId {
    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A fresh id from the operating system's randomness. Without
    /// randomness the clock fills it, so two boots still differ.
    fn fresh(at: DateTime<Utc>) -> Self {
        let mut bytes = [0_u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            bytes = at.timestamp_nanos_opt().unwrap_or_default().to_le_bytes();
        }
        Self(bytes.iter().fold(String::new(), |mut hex, byte| {
            // Writing to a String cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
    }
}

/// When this process started, and the id of that start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The instant that the process captured at its start.
    pub started_at: DateTime<Utc>,
    /// The id of this boot.
    pub id: BootId,
}

impl Boot {
    /// The boot of this process. The first call captures it, and every
    /// later call returns the same value. So a reload, which rebuilds the
    /// tools, cannot stamp it again.
    #[must_use]
    pub fn this_process() -> &'static Self {
        static BOOT: OnceLock<Boot> = OnceLock::new();
        BOOT.get_or_init(|| {
            let started_at = Utc::now();
            Self {
                started_at,
                id: BootId::fresh(started_at),
            }
        })
    }
}

/// A quiet window, as `22:00-08:00`.
fn hhmm(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Whether the environment variable `name` holds a value. Only that.
fn is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The summary of `config` in the process of `boot`. A section that is not
/// in the config is `null`.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one allow list of fields reads best in one place"
)]
pub fn summary(config: &Config, boot: &Boot) -> Value {
    let features: Vec<&str> = crate::features::ALL
        .iter()
        .filter(|feature| feature.on)
        .map(|feature| feature.name)
        .collect();
    let left_out: Vec<&str> = crate::features::ALL
        .iter()
        .filter(|feature| !feature.on)
        .map(|feature| feature.name)
        .collect();
    let chat = |chat: &littleguy_core::ChatConfig| json!({"pairing": chat.pairing, "allowed_senders": chat.allowed.len()});
    json!({
        "note": "The config that this daemon loaded, not the file. null is a section that is \
                 not in the config. A part in `left_out_of_build` cannot run, whatever the config says. \
                 `boot` is this process: a restart changes its id and start, a reload does not. \
                 `build.version` is the version of this binary.",
        "boot": {
            "started_at": boot.started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "id": boot.id.as_str(),
        },
        "build": {"version": VERSION, "features": features, "left_out_of_build": left_out},
        "agent": {
            "window": config.window,
            "max_steps": config.max_steps,
            // null is the engine cap.
            "max_request_bytes": config.max_request_bytes,
        },
        "tools": {
            "web_fetch": config.tools.web_fetch,
            "web_search": config.tools.web_search,
            "web_search_key_set": is_set("LG_BRAVE_API_KEY"),
            "files": config.tools.files,
            "runs": config.tools.runs,
            "plans": config.tools.plans,
            "fetch_max_chars": config.tools.fetch_max_chars,
        },
        "memory": {
            "enabled": config.memory.enabled,
            "consolidate": config.memory.consolidate,
            "timezone": config.memory.timezone,
            "reflect": config.memory.reflect,
            "memory_chars": config.memory.memory_chars,
            "user_chars": config.memory.user_chars,
        },
        "heartbeat": config.heartbeat.as_ref().map(|beat| json!({
            "every_minutes": beat.every_minutes,
            "quiet": beat.quiet.map(|quiet| format!("{}-{}", hhmm(quiet.start), hhmm(quiet.end))),
            "timezone": beat.timezone,
            "research": beat.research,
            "deliver": match beat.deliver {
                HeartbeatDelivery::Web => "web",
            },
        })),
        "brief": config.brief.as_ref().map(|brief| json!({
            "schedule": brief.schedule,
            "timezone": brief.timezone,
            "sends_to_a_chat": brief.to.is_some(),
        })),
        // A null max_resets is no bound.
        "compaction": config.compaction.map(|section| json!({
            "at_percent": section.at_percent,
            "max_resets": section.max_resets,
            "notes_bytes": section.notes_bytes,
        })),
        "refine": config.refine.as_ref().map(|refine| json!({"messages": refine.messages})),
        "skills": {
            // Skills reach the assistant through the refine overlay, so
            // without [refine] none loads.
            "folder_set_in_config": config.skills.dir.is_some(),
            "off_without_refine": config.refine.is_none(),
        },
        "council": config.council.as_ref().map(|council| json!({
            "accept": council.accept.iter().map(|kind| format!("{kind:?}").to_lowercase()).collect::<Vec<_>>(),
            "members": council.members,
            "has_charter": council.charter.is_some(),
        })),
        "flow": {
            "rule_of_two": config.flow.rule_of_two,
            "recheck": format!("{:?}", config.flow.recheck),
        },
        "approvals": {
            "approver_set": config.approvals.approver.is_some(),
            "timeout_secs": config.approvals.timeout_secs,
        },
        "sandbox": config.sandbox.as_ref().map(|sandbox| json!({
            "backend": match sandbox.backend {
                SandboxBackend::Process => "process",
                SandboxBackend::Firecracker(_) => "firecracker",
            },
            "network": sandbox.network,
        })),
        "mcp_servers": config.mcp.iter().map(|server| json!({
            "id": server.id,
            "transport": match server.transport {
                McpTransport::Stdio { .. } => "stdio",
                McpTransport::Http { .. } => "http",
            },
            "labels": server.labels,
            "effect": format!("{:?}", server.effect),
            "tool_allow_list": server.tools.as_ref().map(Vec::len),
            "timeout_secs": server.timeout_secs,
        })).collect::<Vec<_>>(),
        "camel_routes": config.camel.iter().map(|route| route.id.as_str()).collect::<Vec<_>>(),
        "discord": {
            "token_set": is_set("DISCORD_BOT_TOKEN"),
            "chat": chat(&config.discord),
        },
        "slack": {
            "token_set": is_set("SLACK_BOT_TOKEN"),
            "socket_mode_token_set": is_set("SLACK_APP_TOKEN"),
            "chat": chat(&config.slack),
        },
        "google": config.google.as_ref().map(|google| json!({
            "mail": google.mail,
            "calendar": google.calendar,
        })),
        "san_mateo": config.san_mateo.as_ref().map(|san_mateo| json!({
            "integrations": san_mateo.integrations.len(),
            "own_catalog": san_mateo.catalog.is_some(),
        })),
        "api": config.api.as_ref().map(|api| json!({
            "integrations": api.integrations,
            "max_chars": api.max_chars,
        })),
        "hooks": config.hooks.as_ref().map(|hooks| json!({
            "public": !hooks.bind.ip().is_loopback(),
            "token_set": is_set(&hooks.token_env),
        })),
        "fleet": config.fleet.as_ref().map(|fleet| json!({
            "replica": match fleet.replica {
                Replica::Path(_) => "folder",
                Replica::S3(_) => "s3",
            },
            "rpo_secs": fleet.rpo_secs,
        })),
        "pages_serve": config.ui.bind.ip().is_loopback(),
    })
}

/// `tools` and `config_summary` of `config` in the process of `boot`, when
/// `[tools] runs` is on, as for the other tools that let the assistant
/// debug itself.
#[must_use]
pub fn with_summary(
    tools: crate::tools::Tools,
    config: &Config,
    boot: &Boot,
) -> crate::tools::Tools {
    if config.tools.runs {
        tools.with([config_tool(summary(config, boot))])
    } else {
        tools
    }
}

/// The `config_summary` tool over `summary`, which the daemon builds from
/// the config of each load.
#[must_use]
pub fn config_tool(summary: Value) -> Arc<dyn Tool> {
    let summary = Arc::new(summary);
    FnTool::new(
        "config_summary",
        "Show the setup that you run with: which parts are on, off, or not in this build, and \
         their settings, such as the heartbeat interval, the memory sizes and which tools are \
         on. Use it before you guess whether something is off or broken. It also says when this \
         process started, its boot id and the version of this build, so you can tell whether \
         something happened before or after a restart or an upgrade. A run of yours survives a \
         restart, so only this tells you of one. It shows no secret: of a key it says only \
         whether it is set.",
        json!({"type": "object", "properties": {}}),
        move |_input| {
            let summary = Arc::clone(&summary);
            async move { Ok((*summary).clone()) }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> Boot {
        Boot {
            started_at: DateTime::parse_from_rfc3339("2026-11-17T03:00:00.250Z")
                .unwrap()
                .with_timezone(&Utc),
            id: BootId(String::from("00112233aabbccdd")),
        }
    }

    const SECRET_URL: &str = "https://mcp.example/sse?token=sekret123";

    fn full() -> Config {
        Config::parse(&format!(
            "owner = \"mark\"\nsystem = \"Private prompt text.\"\n\
             [discord]\nallowed = [\"123456789012345678\"]\n\
             [approvals]\napprover = \"876543210987654321\"\n\
             max_request_bytes = 600000\n\
             [compaction]\nmax_resets = 4\n\
             [heartbeat]\nevery_minutes = 45\nquiet = \"22:00-08:00\"\n\
             [hooks]\ntoken_env = \"LG_TEST_HOOK_TOKEN_NAME\"\n\
             [[mcp.server]]\nid = \"docs\"\nurl = \"{SECRET_URL}\"\n\
             [[camel.route]]\nid = \"inbox\"\nfrom = \"file:/home/mark/private-inbox\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn the_summary_says_what_is_on_and_leaks_no_id_path_url_or_prompt() {
        let text = summary(&full(), &boot()).to_string();
        for private in [
            "123456789012345678",
            "876543210987654321",
            "sekret123",
            "mcp.example",
            "private-inbox",
            "Private prompt text",
            "LG_TEST_HOOK_TOKEN_NAME",
            "\"mark\"",
        ] {
            assert!(!text.contains(private), "the summary holds {private}");
        }
        let value = summary(&full(), &boot());
        assert_eq!(value["heartbeat"]["every_minutes"], 45);
        assert_eq!(value["heartbeat"]["quiet"], "22:00-08:00");
        assert_eq!(value["discord"]["chat"]["allowed_senders"], 1);
        assert_eq!(value["approvals"]["approver_set"], true);
        assert_eq!(value["mcp_servers"][0]["id"], "docs");
        assert_eq!(value["mcp_servers"][0]["transport"], "http");
        assert_eq!(value["camel_routes"][0], "inbox");
        assert_eq!(value["hooks"]["token_set"], false);
        assert_eq!(value["agent"]["max_request_bytes"], 600_000);
        assert_eq!(value["compaction"]["at_percent"], 80);
        assert_eq!(value["compaction"]["max_resets"], 4);
        assert_eq!(value["compaction"]["notes_bytes"], 8_192);
    }

    #[test]
    fn an_absent_section_is_null_and_differs_from_off() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        let value = summary(&plain, &boot());
        assert_eq!(value["heartbeat"], Value::Null, "no [heartbeat] section");
        assert_eq!(value["refine"], Value::Null);
        assert_eq!(value["compaction"], Value::Null, "no [compaction] section");
        assert_eq!(value["agent"]["max_request_bytes"], Value::Null);
        assert_eq!(value["skills"]["off_without_refine"], true);
        let off = Config::parse("owner = \"mark\"\n[tools]\nfiles = false").unwrap();
        assert_eq!(summary(&off, &boot())["tools"]["files"], false);
        assert!(value["build"]["features"].as_array().is_some());
    }

    #[test]
    fn the_summary_says_which_boot_and_which_build_it_is() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        let value = summary(&plain, &boot());
        assert_eq!(value["boot"]["started_at"], "2026-11-17T03:00:00.250Z");
        assert_eq!(value["boot"]["id"], "00112233aabbccdd");
        assert_eq!(value["build"]["version"], VERSION);
    }

    #[test]
    fn the_boot_of_a_process_is_captured_once() {
        let first = Boot::this_process().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(Boot::this_process(), &first, "a second read stamps nothing");
        assert_eq!(first.id.as_str().len(), 16);
        assert!(first.id.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_fresh_boot_ids_differ() {
        let now = Utc::now();
        assert_ne!(BootId::fresh(now), BootId::fresh(now));
    }
}
