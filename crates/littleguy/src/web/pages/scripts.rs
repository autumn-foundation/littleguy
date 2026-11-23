//! The Scripts tab of the workspace page (ADR 0046).
//!
//! Accepting code to run is a different promise from accepting prose, so
//! the scripts have their own tab. It lists the drafts that wait, then the
//! accepted scripts. The open item shows its code. A draft has Accept and
//! Decline, and Accept carries the digest of the draft on the page. A
//! script has Retire. The run log under both shows what ran and with what
//! arguments, so the one way a tick changes something stays visible.

use maud::{Markup, html};

use super::{Answer, Tab, note, when};
use crate::protocol::{ScriptDraftEntry, ScriptEntry, ScriptRunEntry};

/// One item of the scripts tab.
enum Item<'a> {
    /// A draft that waits for the owner.
    Draft(&'a ScriptDraftEntry),
    /// An accepted script.
    Script(&'a ScriptEntry),
}

/// The URL of the scripts tab with the script `name` open.
pub(super) fn script_url(name: &str) -> String {
    let mut url = String::from("/workspace?tab=scripts&name=");
    url.extend(form_urlencoded::byte_serialize(name.as_bytes()));
    url
}

/// A button that sends the owner's script `action` on `target`, and swaps
/// the open view with the answer.
fn button(action: &str, target: &str, label: &str) -> Markup {
    let mut url = format!("/scripts/{action}/");
    url.extend(form_urlencoded::byte_serialize(target.as_bytes()));
    posts(&url, label)
}

/// A button that posts to `url`, and swaps the open view with the answer.
fn posts(url: &str, label: &str) -> Markup {
    html! {
        button hx-post=(url) hx-target="#plan-view" hx-swap="outerHTML" { (label) }
    }
}

/// The document of one item.
fn doc(item: &Item<'_>, network: bool) -> Markup {
    let reach = if network {
        "runs with network access"
    } else {
        "runs with no network"
    };
    match item {
        Item::Draft(draft) => html! {
            article.doc {
                header {
                    h2 { (draft.name) }
                    div.actions {
                        (posts(&format!("/scripts/accept/{}/{}", draft.id, draft.digest), "Accept"))
                        (button("decline", &draft.id.to_string(), "Decline"))
                    }
                }
                p.meta {
                    span.tag { "waiting for you" }
                    " · proposed by littleguy " (when(draft.created_ms))
                    " · " code { (draft.runtime) }
                    @if draft.revises { " · " strong { "replaces the accepted script" } }
                }
                p.error {
                    "Once accepted, a heartbeat may run this code with any arguments, and what it "
                    "changes stays changed. It " (reach) "."
                }
                h3 { "Description" }
                p { (draft.description) }
                h3 { "Code" }
                pre.diff { code { (draft.code) } }
            }
        },
        Item::Script(script) => html! {
            article.doc {
                header {
                    h2 { (script.name) }
                    div.actions { (button("retire", &script.name, "Retire")) }
                }
                p.meta {
                    span.tag.active { "accepted " (when(script.accepted_ms)) }
                    " · " code { (script.runtime) }
                    " · " code { (script.path) }
                    " · " (reach)
                }
                @if script.drifted {
                    p.error {
                        "Its file no longer matches the hash that you accepted, so it does not "
                        "run. Retire it, or let littleguy propose it again."
                    }
                }
                h3 { "Description" }
                p { (script.description) }
                h3 { "SHA-256" }
                p { code { (script.sha256) } }
                h3 { "Code" }
                @match &script.code {
                    Some(code) => pre.diff { code { (code) } },
                    None => p.note { "The file does not read." },
                }
            }
        },
    }
}

/// The run log: what ran, when, with what arguments, and how it ended.
fn run_log(runs: &[ScriptRunEntry]) -> Markup {
    html! {
        section.runs aria-label="Script runs" {
            h3 { "Runs" }
            @if runs.is_empty() {
                p.note { "No script has run yet." }
            } @else {
                table {
                    thead { tr { th { "When" } th { "Script" } th { "Arguments" } th { "Stdin" } th { "Outcome" } } }
                    tbody {
                        @for run in runs {
                            tr {
                                td { (when(run.at_ms)) }
                                td { a href=(script_url(&run.name)) { (run.name) } }
                                td {
                                    @if run.args.is_empty() { span.note { "none" } }
                                    @for arg in &run.args { code { (arg) } " " }
                                }
                                td {
                                    @match &run.stdin {
                                        Some(stdin) => details { summary { (stdin.chars().count()) " chars" } pre { (stdin) } },
                                        None => span.note { "none" },
                                    }
                                }
                                td { (run.outcome) @if run.duration_ms > 0 { " · " (run.duration_ms) " ms" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The scripts tab: the drafts that wait first, then the scripts, with
/// the draft `id` or the script `name` open, or the first item. The run
/// log goes under both. `notice` goes on top of the open item.
pub(super) fn view(
    answer: &Answer,
    id: Option<i64>,
    name: Option<&str>,
    notice: Option<Markup>,
) -> Markup {
    let Answer::Scripts {
        dir,
        network,
        drafts,
        scripts,
        runs,
    } = answer
    else {
        return html! {
            div #plan-view.split {
                section #file.wide { @if let Some(notice) = notice { (notice) } (note(answer)) }
            }
        };
    };
    let items: Vec<(String, Item<'_>)> = drafts
        .iter()
        .map(|draft| (Tab::Scripts.url(Some(draft.id)), Item::Draft(draft)))
        .chain(
            scripts
                .iter()
                .map(|script| (script_url(&script.name), Item::Script(script))),
        )
        .collect();
    let chosen = |item: &Item<'_>| match item {
        Item::Draft(draft) => id == Some(draft.id),
        Item::Script(script) => name == Some(script.name.as_str()),
    };
    let open = items
        .iter()
        .position(|(_, item)| chosen(item))
        .or_else(|| (!items.is_empty()).then_some(0));
    html! {
        div #plan-view.split {
            nav.files aria-label="Scripts" {
                @if items.is_empty() {
                    p.note { "No scripts in " code { (dir) } " yet. littleguy proposes one with propose_script." }
                }
                ul {
                    @for (index, (url, item)) in items.iter().enumerate() {
                        @let current = (open == Some(index)).then_some("page");
                        li {
                            a href=(url) aria-current=[current]
                                hx-get=(url) hx-select="#plan-view" hx-target="#plan-view"
                                hx-swap="outerHTML" hx-push-url="true" {
                                @match item {
                                    Item::Draft(draft) => {
                                        span.path { (draft.name) }
                                        span.dot title="waiting for you" {}
                                    }
                                    Item::Script(script) => {
                                        span.path { (script.name) }
                                        @if script.drifted { span.size { "changed" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            section #file {
                @if let Some(notice) = notice { (notice) }
                @match open.and_then(|index| items.get(index)) {
                    Some((_, item)) => (doc(item, *network)),
                    None => p.note { "Nothing to open." },
                }
            }
            (run_log(runs))
        }
    }
}
