//! The client side of the socket: `lg chat`, `lg pair`, `lg pairs` and
//! `lg unpair` send one request each and print the answer.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{
    ApprovalEntry, ConnectionEntry, GoalEntry, IdeaEntry, OverlayEntry, PairEntry, Request,
    Response, Speaker, Turn, WorkspaceEntry,
};
use crate::protocol::{ConversationEntry, PastTurn};
use crate::protocol::{InvalidSkill, PatchEntry, SkillDraftEntry, SkillEntry};
use crate::protocol::{ScriptDraftEntry, ScriptEntry, ScriptRunEntry};

/// The prefix of each message id of the web and the CLI. The history shows
/// only the turns whose key starts with it.
pub const MESSAGE_ID_PREFIX: &str = "cli-";

/// A fresh message id: the clock in nanoseconds and the process id.
#[must_use]
pub fn fresh_message_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{MESSAGE_ID_PREFIX}{nanos}-{}", std::process::id())
}

/// Sends `request` to the daemon on `socket` and returns its answer.
///
/// # Errors
///
/// Returns an error when no daemon answers, or the answer is not valid.
pub async fn send(socket: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "no daemon answers on {}. Start one with `lg serve`",
            socket.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write
        .write_all(&line)
        .await
        .context("sending the message")?;
    let mut answer = String::new();
    BufReader::new(read)
        .read_line(&mut answer)
        .await
        .context("reading the answer")?;
    if answer.is_empty() {
        bail!("the daemon closed the connection without an answer");
    }
    serde_json::from_str(&answer).context("the answer is not valid")
}

/// Writes `version` of an overlay for a person: `vN`, or `none`.
#[must_use]
pub fn show_version(version: Option<u64>) -> String {
    version.map_or_else(|| String::from("none"), |version| format!("v{version}"))
}

/// The text that `lg chat` prints, and whether the command succeeded.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one arm per response, and the arms read best as one match"
)]
pub fn render(response: &Response) -> (String, bool) {
    match response {
        Response::Reply { text, .. } => (text.clone(), true),
        Response::Duplicate { last_answer } => (
            format!(
                "(already delivered) {}",
                last_answer.as_deref().unwrap_or("no answer yet")
            ),
            true,
        ),
        Response::NoAnswer { reason } => (format!("no answer: {reason}"), false),
        Response::Pairing { .. } => (String::from("this sender is not paired"), false),
        Response::Dropped => (String::from("the daemon dropped the message"), false),
        Response::Paired { sender } => (format!("paired {sender}"), true),
        Response::Pairs { entries } => (render_pairs(entries), true),
        Response::Unpaired { removed: true } => (String::from("unpaired"), true),
        Response::ConnectLink { link } => (
            format!("Open this link in a browser on this machine:\n{link}"),
            true,
        ),
        Response::Connections { entries } => (render_connections(entries), true),
        Response::Disconnected { removed: true } => (String::from("disconnected"), true),
        Response::Disconnected { removed: false } => {
            (String::from("no connection has that id"), false)
        }
        Response::Unpaired { removed: false } => (
            String::from("no paired sender has that id. A sender from the config file stays"),
            false,
        ),
        Response::Waiting { approval } => (
            format!(
                "waiting for approval of request #{approval}. Run `lg approvals` to read it, \
                 then `lg approve {approval}` or `lg deny {approval}`"
            ),
            true,
        ),
        Response::Busy { approval } => (
            format!(
                "the assistant waits for a decision on request #{approval}. Decide it first \
                 with `lg approve {approval}` or `lg deny {approval}`"
            ),
            false,
        ),
        Response::Approvals { entries } => (render_approvals(entries), true),
        Response::Cleared => (
            String::from("a new conversation starts. Memory, notes and the workspace stay"),
            true,
        ),
        Response::Decided { approval } => (
            format!("decided request #{approval}. The answer went to the requester"),
            true,
        ),
        Response::Proposed { version, overlay } => (
            format!(
                "proposed overlay version {version}. It is not active.\n{overlay}\n\
                 Run `lg refine activate {version}` to use it"
            ),
            true,
        ),
        Response::Kept => (
            String::from("the proposer kept the active overlay. Nothing was stored"),
            true,
        ),
        Response::Overlays {
            active, entries, ..
        } => (render_overlays(*active, entries), true),
        Response::Overlay { entry, overlay } => {
            (format!("{}\n{overlay}", render_overlay_line(entry)), true)
        }
        Response::OverlayStored { version } => (
            format!(
                "stored overlay version {version}. It is not active. \
                 Run `lg refine activate {version}` to use it"
            ),
            true,
        ),
        Response::OverlayActive {
            version, previous, ..
        } => (
            match previous {
                Some(previous) if previous != version => format!(
                    "the assistant runs on overlay version {version} from its next message \
                     (was {previous})"
                ),
                _ => {
                    format!("the assistant runs on overlay version {version} from its next message")
                }
            },
            true,
        ),
        Response::OverlayMoved {
            version, active, ..
        } => (
            format!(
                "the overlay pointer moved after you read it, and {} is active now, \
                 so version {version} is not active. Nothing changed. \
                 Run `lg refine list` and decide again",
                show_version(*active),
            ),
            false,
        ),
        Response::Reloaded {
            applied,
            restart,
            profile,
        } => (render_reload(applied, restart, profile), true),
        Response::History { turns } => (render_history(turns), true),
        Response::Conversations {
            entries,
            keep_days,
            partial,
        } => (render_conversations(entries, *keep_days, *partial), true),
        Response::Conversation { turns, .. } => (render_past(turns), true),
        Response::Workspace { files } => (render_workspace(files), true),
        Response::Plans {
            goals,
            ideas,
            answered,
        } => (render_plans(goals, ideas, answered), true),
        Response::Notes { notes } => (
            if notes.is_empty() {
                String::from("no notes")
            } else {
                notes
                    .iter()
                    .map(|note| format!("{} ({} chars)", note.name, note.text.chars().count()))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            true,
        ),
        Response::Skills {
            skills,
            invalid,
            drafts,
        } => (render_skills(skills, invalid, drafts), true),
        Response::Patches { repo, patches } => (render_patches(repo, patches), true),
        Response::PatchOpened { id, pr, url } => {
            (format!("patch {id} is draft PR #{pr}: {url}"), true)
        }
        Response::Scripts {
            dir,
            drafts,
            scripts,
            runs,
            ..
        } => (render_scripts(dir, drafts, scripts, runs), true),
        Response::WorkspaceFile { path, bytes, text } => (
            text.clone()
                .unwrap_or_else(|| format!("{path}: {bytes} bytes, not text or too large to show")),
            true,
        ),
        // A listing of something that is off has nothing to list. That is
        // not a failure.
        Response::Off { message } => (message.clone(), true),
        Response::Error { message } => (format!("error: {message}"), false),
    }
}

/// One line per skill, per file that does not load, and per draft.
fn render_skills(
    skills: &[SkillEntry],
    invalid: &[InvalidSkill],
    drafts: &[SkillDraftEntry],
) -> String {
    let mut lines: Vec<String> = skills
        .iter()
        .map(|skill| format!("{}: {}", skill.name, skill.description))
        .collect();
    lines.extend(
        invalid
            .iter()
            .map(|file| format!("does not load: {} ({})", file.path, file.reason)),
    );
    lines.extend(
        drafts
            .iter()
            .map(|draft| format!("draft {}: {} waits for you", draft.id, draft.name)),
    );
    if lines.is_empty() {
        return String::from("no skills yet");
    }
    lines.join("\n")
}

/// One line per accepted script, per draft and per recent run.
fn render_scripts(
    dir: &str,
    drafts: &[ScriptDraftEntry],
    scripts: &[ScriptEntry],
    runs: &[ScriptRunEntry],
) -> String {
    let mut lines: Vec<String> = scripts
        .iter()
        .map(|script| {
            let drift = if script.drifted {
                " (its file changed: it does not run)"
            } else {
                ""
            };
            format!(
                "{} [{}]: {}{drift}",
                script.name, script.runtime, script.description
            )
        })
        .collect();
    lines.extend(drafts.iter().map(|draft| {
        let revises = if draft.revises {
            ", revises the script"
        } else {
            ""
        };
        format!("draft {}: {} waits for you{revises}", draft.id, draft.name)
    }));
    if lines.is_empty() {
        lines.push(format!("no scripts in {dir} yet"));
    }
    if !runs.is_empty() {
        lines.push(String::from("recent runs:"));
        lines.extend(runs.iter().map(|run| {
            let mut line = format!(
                "  {} {} {:?}",
                crate::stamp::from_millis(run.at_ms)
                    .map_or_else(String::new, crate::stamp::rfc3339),
                run.name,
                run.args
            );
            if let Some(stdin) = &run.stdin {
                let _ = write!(line, " stdin {} chars", stdin.chars().count());
            }
            let _ = write!(line, ": {}", run.outcome);
            line
        }));
    }
    lines.join("\n")
}

/// One draft whole, for `lg script show` and `lg script accept`: the
/// name, the runtime, the description and the code.
#[must_use]
pub fn render_script_draft(draft: &ScriptDraftEntry) -> String {
    let revises = if draft.revises {
        "\nreplaces the accepted script of that name"
    } else {
        ""
    };
    format!(
        "draft {}: {} [{}]{revises}\n{}\n\n{}",
        draft.id,
        draft.name,
        draft.runtime,
        draft.description.trim(),
        draft.code
    )
}

/// One line per patch, with its status and PR.
fn render_patches(repo: &str, patches: &[PatchEntry]) -> String {
    if patches.is_empty() {
        return format!("no patches for {repo}");
    }
    patches
        .iter()
        .map(|patch| {
            let status = match (&patch.status[..], &patch.pr_url) {
                ("waiting", _) => String::from("waits for you"),
                (status, Some(url)) => format!("{status} {url}"),
                (status, None) => status.to_owned(),
            };
            let revises = patch
                .revises
                .map(|id| format!(" (revises patch {id})"))
                .unwrap_or_default();
            format!("patch {}: {}{revises}, {status}", patch.id, patch.title)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One patch whole, for `lg patch show` and `lg patch open`: the title,
/// the status, the warnings, the body and the diff.
#[must_use]
pub fn render_patch(patch: &PatchEntry) -> String {
    let mut text = format!(
        "patch {}: {}\nstatus: {}",
        patch.id, patch.title, patch.status
    );
    if let Some(url) = &patch.pr_url {
        let _ = write!(text, "\nPR: {url}");
    }
    if let Some(revised) = patch.revises {
        let _ = write!(text, "\nrevises patch {revised}");
    }
    let _ = write!(text, "\nbase: {}", patch.base);
    for warning in &patch.warnings {
        let _ = write!(text, "\nread with care: {warning}");
    }
    let _ = write!(text, "\n\n{}\n\n{}", patch.body.trim(), patch.diff);
    text
}

/// One line per goal and per idea, with its number.
fn render_plans(goals: &[GoalEntry], ideas: &[IdeaEntry], answered: &[IdeaEntry]) -> String {
    let mut lines = Vec::new();
    for goal in goals {
        lines.push(format!(
            "goal {} ({}): {}",
            goal.id, goal.status, goal.title
        ));
    }
    for idea in ideas {
        lines.push(format!("idea {}: {}", idea.id, idea.title));
    }
    for idea in answered {
        lines.push(format!("idea {} (answered): {}", idea.id, idea.title));
    }
    if lines.is_empty() {
        return String::from("no goals and no ideas yet");
    }
    lines.join("\n")
}

/// One line per file: its size and its path.
fn render_workspace(files: &[WorkspaceEntry]) -> String {
    if files.is_empty() {
        return String::from("the workspace is empty");
    }
    files
        .iter()
        .map(|file| format!("{:>9}  {}", file.bytes, file.path))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One block per turn, oldest first.
fn render_history(turns: &[Turn]) -> String {
    if turns.is_empty() {
        return String::from("no conversation yet");
    }
    turns
        .iter()
        .map(|turn| {
            let who = match turn.from {
                Speaker::Person => "you",
                Speaker::Assistant => "littleguy",
                Speaker::Unprompted => "littleguy (on its own)",
            };
            format!("{who}: {}", turn.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The past conversations, one line each, under the retention window.
fn render_conversations(entries: &[ConversationEntry], keep_days: u32, partial: bool) -> String {
    let mut lines = vec![keep_line(keep_days)];
    if entries.is_empty() {
        lines.push(String::from("no conversation yet"));
    }
    let oldest = entries.last().map(|entry| entry.id);
    for entry in entries {
        let title: String = entry
            .first_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let title: String = title.chars().take(80).collect();
        let mut line = format!("#{} {} turns: {title}", entry.id, entry.turns);
        if entry.live {
            line.push_str(" (now)");
        }
        if partial && Some(entry.id) == oldest {
            line.push_str(" (began earlier; its first turns are gone)");
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// The sentence that says how long a past conversation stays.
#[must_use]
pub fn keep_line(keep_days: u32) -> String {
    match keep_days {
        0 => String::from(
            "littleguy keeps no past conversations: [runs] keep_days is 0. Older ones are gone.",
        ),
        1 => String::from(
            "littleguy keeps past conversations for 1 day ([runs] keep_days). Older ones are gone.",
        ),
        days => format!(
            "littleguy keeps past conversations for {days} days ([runs] keep_days). Older ones are gone."
        ),
    }
}

/// The turns of a past conversation, with each tool call on its own line.
fn render_past(turns: &[PastTurn]) -> String {
    turns
        .iter()
        .map(|turn| {
            let who = match turn.from {
                Speaker::Person => "you",
                Speaker::Assistant | Speaker::Unprompted => "littleguy",
            };
            let mut text = format!("{who}: {}", turn.text);
            for call in &turn.calls {
                let _ = std::fmt::Write::write_fmt(
                    &mut text,
                    format_args!("\n  (called {})", call.tool),
                );
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The sections a reload applied, and the sections that wait.
fn render_reload(applied: &[String], restart: &[String], profile: &[String]) -> String {
    let mut lines = vec![if applied.is_empty() && profile.is_empty() {
        String::from("the config changed nothing that runs")
    } else {
        format!("applied: {}", applied.join(", "))
    }];
    if !profile.is_empty() {
        lines.push(format!("from the next message: {}", profile.join(", ")));
    }
    if !restart.is_empty() {
        lines.push(format!(
            "at the next start of `lg serve`: {}",
            restart.join(", ")
        ));
    }
    lines.join("\n")
}

/// One line per overlay version, oldest first, and the active one.
fn render_overlays(active: Option<u64>, entries: &[OverlayEntry]) -> String {
    if entries.is_empty() {
        return String::from("no overlay versions. Run `lg refine propose`");
    }
    let mut lines: Vec<String> = entries.iter().map(render_overlay_line).collect();
    if active.is_none() {
        lines.push(String::from(
            "no version is active: the assistant runs on its config",
        ));
    }
    lines.join("\n")
}

/// One overlay version: a mark for the active one, the version, the
/// author, the date and the parts that it sets.
fn render_overlay_line(entry: &OverlayEntry) -> String {
    let mark = if entry.active { "*" } else { " " };
    let parts = if entry.parts.is_empty() {
        String::from("(no change)")
    } else {
        entry.parts.join(", ")
    };
    format!(
        "{mark} v{}\t{}\t{}\t{parts}",
        entry.version, entry.author, entry.created_at
    )
}

/// Each call that waits, with what it does.
fn render_approvals(entries: &[ApprovalEntry]) -> String {
    if entries.is_empty() {
        return String::from("no calls wait for approval");
    }
    entries
        .iter()
        .map(|e| format!("#{} {}\n{}", e.id, e.tool, e.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One line per connection: integration, connection id, provider, date and
/// whether it needs a new connect.
fn render_connections(entries: &[ConnectionEntry]) -> String {
    if entries.is_empty() {
        return String::from("no connections. Run `lg connect <integration>`");
    }
    entries
        .iter()
        .map(|e| {
            let state = if e.refresh_exhausted {
                "connect again"
            } else {
                "ok"
            };
            format!(
                "{}\t{}\t{}\t{}\t{state}",
                e.integration, e.connection, e.provider, e.created_at
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per allowed sender: channel, sender and source.
fn render_pairs(entries: &[PairEntry]) -> String {
    if entries.is_empty() {
        return String::from("no allowed senders");
    }
    entries
        .iter()
        .map(|e| format!("{}\t{}\t{}", e.channel, e.sender, e.source))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scripts_listing_names_drafts_drift_and_runs() {
        let draft = ScriptDraftEntry {
            id: 2,
            name: String::from("walk-repo"),
            runtime: String::from("python3"),
            description: String::from("Walk a repository."),
            code: String::from("print(1)\n"),
            digest: String::from("ab"),
            revises: true,
            created_ms: 0,
        };
        let script = ScriptEntry {
            name: String::from("walk-repo"),
            runtime: String::from("python3"),
            description: String::from("Walk a repository."),
            path: String::from("/s/walk-repo.py"),
            sha256: String::from("cd"),
            code: None,
            drifted: true,
            accepted_ms: 0,
        };
        let run = ScriptRunEntry {
            name: String::from("walk-repo"),
            args: vec![String::from("a b")],
            stdin: Some(String::from("xyz")),
            outcome: String::from("exited 0"),
            duration_ms: 4,
            at_ms: 0,
        };
        let (text, ok) = render(&Response::Scripts {
            dir: String::from("/s"),
            network: false,
            drafts: vec![draft.clone()],
            scripts: vec![script],
            runs: vec![run],
        });
        assert!(ok);
        assert!(
            text.contains("walk-repo [python3]: Walk a repository. (its file changed"),
            "{text}"
        );
        assert!(
            text.contains("draft 2: walk-repo waits for you, revises the script"),
            "{text}"
        );
        assert!(
            text.contains(r#"walk-repo ["a b"] stdin 3 chars: exited 0"#),
            "{text}"
        );
        let empty = render(&Response::Scripts {
            dir: String::from("/s"),
            network: false,
            drafts: Vec::new(),
            scripts: Vec::new(),
            runs: Vec::new(),
        });
        assert_eq!(empty.0, "no scripts in /s yet");
        let shown = render_script_draft(&draft);
        assert!(
            shown.starts_with("draft 2: walk-repo [python3]\nreplaces"),
            "{shown}"
        );
        assert!(shown.ends_with("print(1)\n"), "{shown}");
    }

    #[test]
    fn fresh_ids_differ_and_are_valid() {
        let a = fresh_message_id();
        let b = fresh_message_id();
        assert_ne!(a, b);
        assert!(littleguy_core::MessageId::new(a).is_ok());
    }

    #[test]
    fn only_an_answer_or_a_duplicate_succeeds() {
        let reply = Response::Reply {
            text: String::from("hi"),
            usage: None,
        };
        assert_eq!(render(&reply), (String::from("hi"), true));
        assert!(render(&Response::Duplicate { last_answer: None }).1);
        assert!(!render(&Response::Dropped).1);
        assert!(!render(&Response::Pairing { code: None }).1);
        assert_eq!(
            render(&Response::Paired {
                sender: String::from("discord:1")
            }),
            (String::from("paired discord:1"), true)
        );
        assert!(render(&Response::Unpaired { removed: true }).1);
        assert!(!render(&Response::Unpaired { removed: false }).1);
        assert!(
            !render(&Response::NoAnswer {
                reason: String::from("x")
            })
            .1
        );
        assert!(
            !render(&Response::Error {
                message: String::from("x")
            })
            .1
        );
    }

    fn overlay(version: u64, active: bool, parts: &[&str]) -> OverlayEntry {
        OverlayEntry {
            version,
            author: String::from("operator:lg-refine"),
            created_at: String::from("2026-11-10T00:00:00+00:00"),
            active,
            parts: parts.iter().map(|&p| p.to_owned()).collect(),
        }
    }

    #[test]
    fn overlays_print_one_line_each_and_mark_the_active_one() {
        let (text, ok) = render(&Response::Overlays {
            active: Some(2),
            head: None,
            entries: vec![overlay(1, false, &["system"]), overlay(2, true, &[])],
        });
        assert!(ok);
        assert_eq!(
            text,
            "  v1\toperator:lg-refine\t2026-11-10T00:00:00+00:00\tsystem\n\
             * v2\toperator:lg-refine\t2026-11-10T00:00:00+00:00\t(no change)"
        );
        let (none, _) = render(&Response::Overlays {
            active: None,
            head: None,
            entries: vec![overlay(1, false, &["system"])],
        });
        assert!(none.ends_with("the assistant runs on its config"), "{none}");
        let (empty, _) = render(&Response::Overlays {
            active: None,
            head: None,
            entries: Vec::new(),
        });
        assert!(empty.contains("lg refine propose"), "{empty}");
    }

    #[test]
    fn refine_answers_name_the_next_step() {
        let (text, ok) = render(&Response::Proposed {
            version: 3,
            overlay: String::from("{}"),
        });
        assert!(ok && text.contains("lg refine activate 3"), "{text}");
        let (text, _) = render(&Response::OverlayStored { version: 4 });
        assert!(text.contains("lg refine activate 4"), "{text}");
        let (text, _) = render(&Response::Kept);
        assert!(text.contains("Nothing was stored"), "{text}");
        let (text, _) = render(&Response::OverlayActive {
            version: 1,
            previous: Some(2),
            guarded: false,
        });
        assert!(text.contains("(was 2)"), "{text}");
        let (text, _) = render(&Response::OverlayActive {
            version: 1,
            previous: None,
            guarded: false,
        });
        assert!(!text.contains("was"), "{text}");
        let (text, ok) = render(&Response::OverlayMoved {
            version: 3,
            active: Some(2),
            head: Some(9),
        });
        assert!(!ok, "a refused activation fails the command");
        assert!(
            text.contains("v2 is active now") && text.contains("version 3 is not active"),
            "{text}"
        );
        let (text, _) = render(&Response::OverlayMoved {
            version: 1,
            active: None,
            head: Some(4),
        });
        assert!(text.contains("none is active now"), "{text}");
        let (text, _) = render(&Response::Overlay {
            entry: overlay(1, true, &["guardrails"]),
            overlay: String::from("{\"guardrails\": \"x\"}"),
        });
        assert!(text.starts_with("* v1"), "{text}");
        assert!(text.ends_with("{\"guardrails\": \"x\"}"), "{text}");
    }

    #[test]
    fn pairs_print_one_line_each() {
        assert_eq!(render_pairs(&[]), "no allowed senders");
        let entries = vec![
            PairEntry {
                channel: String::from("cli"),
                sender: String::from("uid:1"),
                source: String::from("owner"),
            },
            PairEntry {
                channel: String::from("discord"),
                sender: String::from("discord:2"),
                source: String::from("paired"),
            },
        ];
        assert_eq!(
            render(&Response::Pairs { entries }).0,
            "cli\tuid:1\towner\ndiscord\tdiscord:2\tpaired"
        );
    }

    #[test]
    fn connections_print_one_line_each() {
        assert!(render_connections(&[]).contains("lg connect"));
        let entry = |exhausted| ConnectionEntry {
            integration: String::from("google"),
            connection: String::from("c-1"),
            provider: String::from("google"),
            created_at: String::from("2026-11-08T00:00:00+00:00"),
            refresh_exhausted: exhausted,
        };
        let (text, ok) = render(&Response::Connections {
            entries: vec![entry(false), entry(true)],
        });
        assert!(ok);
        assert_eq!(
            text,
            "google\tc-1\tgoogle\t2026-11-08T00:00:00+00:00\tok\n\
             google\tc-1\tgoogle\t2026-11-08T00:00:00+00:00\tconnect again"
        );
        let (text, ok) = render(&Response::ConnectLink {
            link: String::from("http://x"),
        });
        assert!(ok && text.ends_with("http://x"));
        assert!(render(&Response::Disconnected { removed: true }).1);
        assert!(!render(&Response::Disconnected { removed: false }).1);
    }

    #[tokio::test]
    async fn no_daemon_names_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("none.sock");
        let request = Request::Chat {
            message_id: String::from("m"),
            text: String::from("t"),
        };
        let error = send(&socket, &request).await.unwrap_err().to_string();
        assert!(error.contains("none.sock"), "{error}");
    }
}
