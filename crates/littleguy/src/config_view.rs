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

use std::sync::Arc;

use autumn_harvest_agent::{FnTool, Tool, ToolEffect};
use littleguy_core::{Config, HeartbeatDelivery, McpTransport, Replica, SandboxBackend};
use serde_json::{Value, json};

/// A quiet window, as `22:00-08:00`.
fn hhmm(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Whether the environment variable `name` holds a value. Only that.
fn is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The summary of `config`. A section that is not in the config is `null`.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one allow list of fields reads best in one place"
)]
pub fn summary(config: &Config) -> Value {
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
                 not in the config. A part in `left_out_of_build` cannot run, whatever the config says.",
        "build": {"features": features, "left_out_of_build": left_out},
        "agent": {"window": config.window, "max_steps": config.max_steps},
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

/// `tools` and `config_summary` of `config`, when `[tools] runs` is on, as
/// for the other tools that let the assistant debug itself.
#[must_use]
pub fn with_summary(tools: crate::tools::Tools, config: &Config) -> crate::tools::Tools {
    if config.tools.runs {
        tools.with([config_tool(summary(config))])
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
         on. Use it before you guess whether something is off or broken. It shows no secret: \
         of a key it says only whether it is set.",
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

    const SECRET_URL: &str = "https://mcp.example/sse?token=sekret123";

    fn full() -> Config {
        Config::parse(&format!(
            "owner = \"mark\"\nsystem = \"Private prompt text.\"\n\
             [discord]\nallowed = [\"123456789012345678\"]\n\
             [approvals]\napprover = \"876543210987654321\"\n\
             [heartbeat]\nevery_minutes = 45\nquiet = \"22:00-08:00\"\n\
             [hooks]\ntoken_env = \"LG_TEST_HOOK_TOKEN_NAME\"\n\
             [[mcp.server]]\nid = \"docs\"\nurl = \"{SECRET_URL}\"\n\
             [[camel.route]]\nid = \"inbox\"\nfrom = \"file:/home/mark/private-inbox\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn the_summary_says_what_is_on_and_leaks_no_id_path_url_or_prompt() {
        let text = summary(&full()).to_string();
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
        let value = summary(&full());
        assert_eq!(value["heartbeat"]["every_minutes"], 45);
        assert_eq!(value["heartbeat"]["quiet"], "22:00-08:00");
        assert_eq!(value["discord"]["chat"]["allowed_senders"], 1);
        assert_eq!(value["approvals"]["approver_set"], true);
        assert_eq!(value["mcp_servers"][0]["id"], "docs");
        assert_eq!(value["mcp_servers"][0]["transport"], "http");
        assert_eq!(value["camel_routes"][0], "inbox");
        assert_eq!(value["hooks"]["token_set"], false);
    }

    #[test]
    fn an_absent_section_is_null_and_differs_from_off() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        let value = summary(&plain);
        assert_eq!(value["heartbeat"], Value::Null, "no [heartbeat] section");
        assert_eq!(value["refine"], Value::Null);
        assert_eq!(value["skills"]["off_without_refine"], true);
        let off = Config::parse("owner = \"mark\"\n[tools]\nfiles = false").unwrap();
        assert_eq!(summary(&off)["tools"]["files"], false);
        assert!(value["build"]["features"].as_array().is_some());
    }
}
