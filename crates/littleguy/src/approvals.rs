//! How a person sees and decides a call that waits.
//!
//! The daemon claims the record of each wait (`littleguy-stores`). It sends
//! the approver a Discord DM with the call and two buttons. The owner can
//! also decide with `lg approve` and `lg deny`. A decision answers the
//! wait with an engine signal.
//!
//! An approval sends the recorded arguments back as an edit. So the call
//! runs with exactly the arguments that the approver read, even when a
//! model reused a call id.

use std::fmt::Write as _;

use autumn_harvest_agent::Approval;
use littleguy_channels::{Button, ButtonStyle};
use littleguy_stores::ApprovalRecord;

/// The custom id prefix of the buttons.
pub const BUTTON_PREFIX: &str = "lg";

/// The requester of a call from `lg chat`.
pub const CLI_REQUESTER: &str = "cli";

/// The prefix of a Discord requester: the DM channel follows.
pub const DISCORD_REQUESTER: &str = "discord:";

/// The most characters of the code in a notice. A notice must fit one
/// Discord message with its frame.
pub const MAX_SHOWN_CHARS: usize = 1_500;

/// Who asked for the work that waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requester {
    /// The owner, through `lg chat`. The answer goes back to `lg approve`.
    Cli,
    /// A Discord user. The answer goes to this DM channel.
    Discord(String),
}

impl Requester {
    /// The text that the store keeps.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Cli => String::from(CLI_REQUESTER),
            Self::Discord(channel) => format!("{DISCORD_REQUESTER}{channel}"),
        }
    }

    /// The requester that `text` names. Unknown text names nobody.
    #[must_use]
    pub fn decode(text: Option<&str>) -> Option<Self> {
        match text? {
            CLI_REQUESTER => Some(Self::Cli),
            other => other
                .strip_prefix(DISCORD_REQUESTER)
                .filter(|channel| !channel.is_empty())
                .map(|channel| Self::Discord(channel.to_owned())),
        }
    }
}

/// A decision of a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Run the call.
    Approve,
    /// Do not run the call. The model reads the reason.
    Deny(String),
}

/// The reason that a denial without one gives the model.
pub const DEFAULT_DENY_REASON: &str = "the owner denied this call";

/// The signal payload of `choice` on `record`.
///
/// # Errors
///
/// Returns the text for the person when a call with unknown arguments is
/// approved. Such a call can only be denied.
pub fn payload(record: &ApprovalRecord, choice: &Choice) -> Result<Approval, String> {
    match choice {
        Choice::Approve if record.arguments.is_null() => Err(format!(
            "request #{} has no recorded arguments, so it can only be denied",
            record.id
        )),
        Choice::Approve => Ok(Approval::Edit {
            arguments: record.arguments.clone(),
        }),
        Choice::Deny(reason) => Ok(Approval::Reject {
            reason: reason.clone(),
        }),
    }
}

/// The custom id of a button: `lg:approve:<id>` or `lg:deny:<id>`.
#[must_use]
pub fn custom_id(choice: &Choice, id: i64) -> String {
    let verb = match choice {
        Choice::Approve => "approve",
        Choice::Deny(_) => "deny",
    };
    format!("{BUTTON_PREFIX}:{verb}:{id}")
}

/// The choice and the record number that a button names.
#[must_use]
pub fn parse_custom_id(custom_id: &str) -> Option<(Choice, i64)> {
    let mut parts = custom_id.splitn(3, ':');
    if parts.next()? != BUTTON_PREFIX {
        return None;
    }
    let choice = match parts.next()? {
        "approve" => Choice::Approve,
        "deny" => Choice::Deny(String::from(DEFAULT_DENY_REASON)),
        _ => return None,
    };
    let id = parts.next()?.parse().ok()?;
    Some((choice, id))
}

/// The buttons of the notice of `id`.
#[must_use]
pub fn buttons(id: i64) -> [Button; 2] {
    [
        Button {
            custom_id: custom_id(&Choice::Approve, id),
            label: String::from("Approve"),
            style: ButtonStyle::Success,
        },
        Button {
            custom_id: custom_id(&Choice::Deny(String::new()), id),
            label: String::from("Deny"),
            style: ButtonStyle::Danger,
        },
    ]
}

/// The first `max` characters of `text`, and whether it was cut.
fn cut(text: &str, max: usize) -> (String, bool) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    (head, chars.next().is_some())
}

/// A code block that no text inside it can close.
fn fence(text: &str) -> String {
    let text = text.replace("```", "`\u{200b}``");
    format!("```\n{text}\n```")
}

/// What the call does, for a person: the runtime and the code of a
/// `run_code` call, or the arguments of any other call.
#[must_use]
pub fn describe(record: &ApprovalRecord) -> String {
    let arguments = &record.arguments;
    let (body, header) = match (arguments["runtime"].as_str(), arguments["code"].as_str()) {
        (Some(runtime), Some(code)) => (code.to_owned(), format!("Runtime: {runtime}")),
        (Some(runtime), None) => (String::new(), format!("Runtime: {runtime}, no code")),
        _ if arguments.is_null() => (
            String::new(),
            String::from("The arguments of this call were not recorded."),
        ),
        _ => (
            serde_json::to_string_pretty(arguments).unwrap_or_default(),
            String::from("Arguments:"),
        ),
    };
    let mut text = header;
    if !body.is_empty() {
        let (shown, cut) = cut(&body, MAX_SHOWN_CHARS);
        text.push('\n');
        text.push_str(&fence(&shown));
        if cut {
            text.push_str("\n(cut: the call holds more)");
        }
    }
    let extra: Vec<&str> = ["args", "stdin", "files", "env"]
        .into_iter()
        .filter(|key| arguments.get(*key).is_some_and(|v| !v.is_null()))
        .collect();
    if record.tool == crate::sandbox::TOOL && !extra.is_empty() {
        let _ = write!(text, "\nIt also sets: {}.", extra.join(", "));
    }
    text
}

/// The notice that asks the approver.
#[must_use]
pub fn notice(record: &ApprovalRecord) -> String {
    format!(
        "Request #{}: {} (`{}`).\n{}\nApprove it here, or run `lg approve {}` or `lg deny {}`.",
        record.id,
        record.reason,
        record.tool,
        describe(record),
        record.id,
        record.id
    )
}

/// The text that tells a requester to wait.
#[must_use]
pub fn waiting_text(id: i64) -> String {
    format!(
        "I need approval before I run code (request #{id}). I will answer here once it is decided."
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn record(arguments: Value) -> ApprovalRecord {
        ApprovalRecord {
            id: 7,
            call_id: String::from("call-1"),
            tool: String::from("run_code"),
            arguments,
            reason: String::from("the assistant wants to run code"),
            signal: Some(String::from("tool_approval:1:0:call-1")),
            requester: None,
            verdict: None,
            closed: false,
        }
    }

    #[test]
    fn a_requester_round_trips() {
        for requester in [Requester::Cli, Requester::Discord(String::from("900"))] {
            assert_eq!(
                Requester::decode(Some(&requester.encode())),
                Some(requester)
            );
        }
        assert_eq!(Requester::decode(None), None);
        assert_eq!(Requester::decode(Some("discord:")), None);
        assert_eq!(Requester::decode(Some("mail:x")), None);
    }

    #[test]
    fn a_button_id_round_trips_and_a_foreign_one_is_refused() {
        let approve = custom_id(&Choice::Approve, 12);
        assert_eq!(approve, "lg:approve:12");
        assert_eq!(parse_custom_id(&approve), Some((Choice::Approve, 12)));
        let (deny, id) = parse_custom_id("lg:deny:3").unwrap();
        assert_eq!(id, 3);
        assert_eq!(deny, Choice::Deny(String::from(DEFAULT_DENY_REASON)));
        for bad in [
            "",
            "lg",
            "lg:approve",
            "lg:run:1",
            "xx:approve:1",
            "lg:approve:x",
        ] {
            assert_eq!(parse_custom_id(bad), None, "{bad}");
        }
        let [yes, no] = buttons(4);
        assert_eq!(yes.custom_id, "lg:approve:4");
        assert_eq!(no.custom_id, "lg:deny:4");
    }

    #[test]
    fn an_approval_sends_the_recorded_arguments() {
        let args = json!({"runtime": "sh", "code": "echo hi"});
        let approved = payload(&record(args.clone()), &Choice::Approve).unwrap();
        assert_eq!(approved, Approval::Edit { arguments: args });
        let denied = payload(&record(json!({})), &Choice::Deny(String::from("no"))).unwrap();
        assert_eq!(
            denied,
            Approval::Reject {
                reason: String::from("no")
            }
        );
        let unknown = payload(&record(Value::Null), &Choice::Approve).unwrap_err();
        assert!(unknown.contains("only be denied"), "{unknown}");
        assert!(payload(&record(Value::Null), &Choice::Deny(String::new())).is_ok());
    }

    #[test]
    fn a_notice_shows_the_code_in_a_fence_that_holds() {
        let text = notice(&record(json!({
            "runtime": "python3",
            "code": "print('```')\nprint(2)",
            "stdin": "x",
        })));
        assert!(text.starts_with("Request #7:"), "{text}");
        assert!(text.contains("Runtime: python3"), "{text}");
        assert!(text.contains("print(2)"), "{text}");
        // Only the frame opens and closes a block.
        assert_eq!(text.matches("```").count(), 2, "{text}");
        assert!(text.contains("It also sets: stdin."), "{text}");
        assert!(text.contains("lg approve 7"), "{text}");
    }

    #[test]
    fn a_long_call_is_cut_and_every_shape_is_described() {
        let long = "x".repeat(MAX_SHOWN_CHARS + 10);
        let text = describe(&record(json!({"runtime": "sh", "code": long})));
        assert!(text.contains("(cut: the call holds more)"));
        assert!(text.chars().count() < MAX_SHOWN_CHARS + 100);
        assert!(describe(&record(json!({"runtime": "sh"}))).contains("no code"));
        assert!(describe(&record(Value::Null)).contains("not recorded"));
        let mut other = record(json!({"path": "a.txt"}));
        other.tool = String::from("write_file");
        let text = describe(&other);
        assert!(
            text.contains("Arguments:") && text.contains("a.txt"),
            "{text}"
        );
    }

    #[test]
    fn the_waiting_text_names_the_request() {
        assert!(waiting_text(7).contains("#7"));
    }
}
