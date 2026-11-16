//! The pages: maud markup, htmx for each action.
//!
//! Every page and action needs the owner in the session. An action sends
//! one [`Request`] through the daemon handle, as `lg` does over the socket,
//! and answers with the section that it changed. So the pages hold no state
//! of their own, and they can do nothing that `lg` cannot. The one exception
//! is a chat answer that waits for its stream (see [`super::streams`]).

use std::convert::Infallible;

use autumn_plugin_markstream::sse::{Stream, StreamEvent, stream};
use autumn_plugin_markstream::{Markstream, Mode, markstream_head};
use autumn_web::AppState;
use autumn_web::app::AppBuilder;
use autumn_web::assets::PluginAssets;
use autumn_web::htmx::HTMX_JS_PATH;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::http::StatusCode;
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use autumn_web::security::CsrfToken;
use autumn_web::session::Session;
use autumn_web::sse::{Event, Sse};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::Web;
use super::decisions::Poll;
use crate::client::render;
use crate::daemon::live::Piece;
use crate::protocol::{
    ApprovalEntry, ConnectionEntry, OverlayEntry, PairEntry, Request, Response as Answer, Speaker,
    Turn,
};

/// The session key that marks the owner.
pub const OWNER_KEY: &str = "lg.owner";

/// The text for a browser with no owner session.
const SIGN_IN: &str = "Run `lg ui` on this machine and open the link that it prints.";

/// The script of the chat.
const CHAT_JS: &str = "chat.js";

/// The style of every page.
const STYLE_CSS: &str = "lg.css";

/// The script and the style of the pages. The binary holds them, and the app
/// serves them from memory with a hashed URL. The page CSP allows no inline
/// script, so the chat script is a file.
static ASSETS: PluginAssets = PluginAssets::from_files(
    "littleguy",
    &[
        (CHAT_JS, include_bytes!("assets/chat.js")),
        (STYLE_CSS, include_bytes!("assets/lg.css")),
    ],
);

/// Installs the assets of the pages: their own, and the markdown renderer.
#[derive(Debug, Clone, Copy)]
pub struct Assets;

impl autumn_web::plugin::Plugin for Assets {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("littleguy-pages")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.plugin_assets(&ASSETS)
            .plugin(autumn_plugin_markstream::MarkstreamPlugin::new())
    }
}

/// The routes of the pages.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![
        index,
        manage,
        workspace,
        decide_plan,
        decide_skill,
        login,
        logout,
        chat,
        chat_stream,
        new_conversation,
        approvals,
        approve,
        deny,
        decided,
        pairs,
        pair,
        unpair,
        overlays,
        overlay,
        propose,
        activate,
        rollback,
    ]
}

/// The web state of the app.
fn web(state: &AppState) -> Option<std::sync::Arc<Web>> {
    state.extension::<Web>()
}

/// Whether the session holds the owner.
async fn is_owner(session: &Session) -> bool {
    session.get(OWNER_KEY).await.as_deref() == Some("1")
}

/// The answer for a request with no owner session.
fn unsigned(fragment: bool) -> Response {
    let body = if fragment {
        html! { p.error { (SIGN_IN) } }
    } else {
        page(
            "Sign in",
            None,
            &html! { div.signin { h1 { "littleguy" } p { (SIGN_IN) } } },
        )
    };
    (StatusCode::UNAUTHORIZED, body).into_response()
}

/// The answer when the app has no web state.
fn broken() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        html! { p.error { "the daemon does not serve these pages" } },
    )
        .into_response()
}

/// A whole page with `body`. With a CSRF token, every htmx request sends it.
fn page(title: &str, csrf: Option<&str>, body: &Markup) -> Markup {
    let headers = csrf.map(|token| format!("{{\"X-CSRF-Token\": \"{token}\"}}"));
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - littleguy" }
                (ASSETS.stylesheet_tag(STYLE_CSS))
                (markstream_head())
                script src=(HTMX_JS_PATH) defer {}
                (ASSETS.deferred_script_tag(CHAT_JS))
            }
            body hx-headers=[headers] { (body) }
        }
    }
}

/// Asks the daemon, and turns a failure into an answer for the page.
async fn ask(web: &Web, request: Request) -> Answer {
    web.handle
        .ask(request)
        .await
        .unwrap_or_else(|error| Answer::Error {
            message: format!("{error:#}"),
        })
}

/// One line of text for an answer, with its class.
///
/// Three cases look different: an answer, something that is off in this
/// daemon (muted, with how to turn it on), and a failure (red).
fn note(answer: &Answer) -> Markup {
    if let Answer::Off { message } = answer {
        return html! { p.off { span.tag { "off" } " " (message) } };
    }
    let (text, ok) = render(answer);
    let class = if ok { "note" } else { "error" };
    html! { p class=(class) { (text) } }
}

/// The bar on top of each page of the owner, with `current` marked.
fn bar(csrf: &str, current: &str) -> Markup {
    let mark = |path: &str| (path == current).then_some("page");
    html! {
        header.bar {
            h1 { a href="/" { "littleguy" } }
            nav {
                a href="/" aria-current=[mark("/")] { "Chat" }
                a href="/workspace" aria-current=[mark("/workspace")] { "Workspace" }
                a href="/manage" aria-current=[mark("/manage")] { "Manage" }
                form method="post" action="/logout" {
                    input type="hidden" name="_csrf" value=(csrf);
                    button type="submit" { "Sign out" }
                }
            }
        }
    }
}

/// One turn of the thread. The assistant writes markdown, so its turn
/// renders as markdown, and raw HTML in it shows as text. A person's turn
/// shows as it was typed.
fn turn(turn: &Turn) -> Markup {
    match turn.from {
        Speaker::Person => html! { div.msg.you { div.body { (turn.text) } } },
        Speaker::Assistant => answer_turn(&turn.text, true),
        // A heartbeat or a brief: the same answer, with a label.
        Speaker::Unprompted => html! {
            p.unprompted { "littleguy, on its own" }
            (answer_turn(&turn.text, true))
        },
    }
}

/// The style of an answer: markdown in a chat bubble.
fn answer_markdown(markdown: Markstream) -> Markup {
    let markdown = markdown.mode(Mode::Chat).class("markstream body");
    html! { div.msg.lg { (markdown) } }
}

/// One answer of the assistant, or an error in its place.
fn answer_turn(text: &str, ok: bool) -> Markup {
    if ok {
        answer_markdown(Markstream::new(text))
    } else {
        html! { div.msg.lg.error { div.body { (text) } } }
    }
}

/// The page of the owner: the conversation, and the box to write in.
#[get("/")]
async fn index(State(state): State<AppState>, session: Session, csrf: CsrfToken) -> Response {
    if !is_owner(&session).await {
        return unsigned(false);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let history = ask(&web, Request::History).await;
    let body = html! {
        (bar(csrf.token(), "/"))
        main.chat {
            div #scroller {
                div #thread aria-live="polite" {
                    @match &history {
                        Answer::History { turns } if turns.is_empty() => p.empty { "Say hi to littleguy." },
                        Answer::History { turns } => @for each in turns { (turn(each)) },
                        other => (note(other)),
                    }
                }
                div #thinking aria-hidden="true" {
                    span.dot {} span.dot {} span.dot {}
                    span.label { "littleguy is thinking…" }
                }
            }
            div.composer-wrap {
                // The chat script shows the dots and locks the button until
                // the answer settles.
                form #composer hx-post="/chat" hx-target="#thread" hx-swap="beforeend" {
                    textarea name="text" rows="1" required aria-label="Message"
                        placeholder="Message littleguy" {}
                    button type="submit" { "Send" }
                }
                div.hint {
                    "Enter sends · Shift+Enter adds a line · "
                    // The thread starts over in place. Memory, notes and the
                    // workspace stay.
                    button.quiet type="button" hx-post="/new" hx-target="#thread"
                        hx-swap="innerHTML" { "New conversation" }
                }
            }
        }
    };
    page("Chat", Some(csrf.token()), &body).into_response()
}

/// The page of approvals, pairing, refine and connections.
#[get("/manage")]
async fn manage(State(state): State<AppState>, session: Session, csrf: CsrfToken) -> Response {
    if !is_owner(&session).await {
        return unsigned(false);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let approvals = approvals_section(&web, None).await;
    let pairs = pairs_section(&web, None).await;
    let overlays = overlays_section(&web, None).await;
    let connections = connections_section(&web).await;
    let body = html! {
        (bar(csrf.token(), "/manage"))
        main.manage {
            (approvals)
            (pairs)
            (overlays)
            (connections)
        }
    };
    page("Manage", Some(csrf.token()), &body).into_response()
}

/// What the workspace page opens: a tab, and a file or an item in it.
#[derive(Debug, Deserialize)]
struct Open {
    tab: Option<String>,
    path: Option<String>,
    id: Option<i64>,
    /// A skill, by name.
    name: Option<String>,
}

/// A tab of the workspace page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// The files of the workspace.
    Files,
    /// The goals, proposed and active.
    Goals,
    /// The open ideas.
    Ideas,
    /// The notes of the assistant.
    Notes,
    /// The skills, and the drafts that wait.
    Skills,
}

impl Tab {
    /// The tab that `name` names. Anything else is the files.
    fn parse(name: Option<&str>) -> Self {
        match name {
            Some("goals") => Self::Goals,
            Some("ideas") => Self::Ideas,
            Some("notes") => Self::Notes,
            Some("skills") => Self::Skills,
            _ => Self::Files,
        }
    }

    /// The URL of the tab, with item `id` open.
    fn url(self, id: Option<i64>) -> String {
        let tab = match self {
            Self::Files => return String::from("/workspace"),
            Self::Goals => "goals",
            Self::Ideas => "ideas",
            Self::Notes => "notes",
            Self::Skills => "skills",
        };
        id.map_or_else(
            || format!("/workspace?tab={tab}"),
            |id| format!("/workspace?tab={tab}&id={id}"),
        )
    }
}

/// The URL of the workspace page with `path` open.
fn workspace_url(path: &str) -> String {
    let mut url = String::from("/workspace?path=");
    url.extend(form_urlencoded::byte_serialize(path.as_bytes()));
    url
}

/// The URL of the notes tab with the note `name` open.
fn note_url(name: &str) -> String {
    let mut url = String::from("/workspace?tab=notes&name=");
    url.extend(form_urlencoded::byte_serialize(name.as_bytes()));
    url
}

/// The URL of the skills tab with the skill `name` open.
fn skill_url(name: &str) -> String {
    let mut url = String::from("/workspace?tab=skills&name=");
    url.extend(form_urlencoded::byte_serialize(name.as_bytes()));
    url
}

/// The tabs of the workspace page, with `current` marked. The goals and
/// skills tabs count what waits for the owner. With `oob`, an htmx answer
/// swaps the tabs too, so the counts stay right after a decision.
fn tabs(current: Tab, plans: &Answer, skills: &Answer, oob: bool) -> Markup {
    let waiting = match plans {
        Answer::Plans { goals, .. } => goals.iter().filter(|g| g.status == "proposed").count(),
        _ => 0,
    };
    let drafts = match skills {
        Answer::Skills { drafts, .. } => drafts.len(),
        _ => 0,
    };
    let mark = |tab: Tab| (tab == current).then_some("page");
    html! {
        nav #tabs.tabs aria-label="Workspace" hx-swap-oob=[oob.then_some("true")] {
            a href=(Tab::Files.url(None)) aria-current=[mark(Tab::Files)] { "Files" }
            a href=(Tab::Goals.url(None)) aria-current=[mark(Tab::Goals)] {
                "Goals"
                @if waiting > 0 { span.count title="waiting for you" { (waiting) " waiting" } }
            }
            a href=(Tab::Ideas.url(None)) aria-current=[mark(Tab::Ideas)] { "Ideas" }
            a href=(Tab::Notes.url(None)) aria-current=[mark(Tab::Notes)] { "Notes" }
            a href=(Tab::Skills.url(None)) aria-current=[mark(Tab::Skills)] {
                "Skills"
                @if drafts > 0 { span.count title="drafts wait for you" { (drafts) " waiting" } }
            }
        }
    }
}

/// The files of the assistant workspace, its goals and its ideas, one tab
/// at a time. Each tab is a list and the item that is open. A link loads
/// the whole page, so the page works without a script. With htmx, a click
/// swaps only the open item.
#[get("/workspace")]
async fn workspace(
    State(state): State<AppState>,
    session: Session,
    csrf: CsrfToken,
    Query(open): Query<Open>,
) -> Response {
    if !is_owner(&session).await {
        return unsigned(false);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let tab = Tab::parse(open.tab.as_deref());
    let plans = ask(&web, Request::Plans).await;
    let skills = ask(&web, Request::Skills).await;
    let view = match tab {
        Tab::Files => files_view(&web, open.path.as_deref()).await,
        Tab::Goals | Tab::Ideas => plans_view(tab, &plans, open.id, None),
        Tab::Notes => notes_view(&ask(&web, Request::Notes).await, open.name.as_deref()),
        Tab::Skills => skills_view(&skills, open.id, open.name.as_deref(), None),
    };
    let body = html! {
        (bar(csrf.token(), "/workspace"))
        main.workspace {
            (tabs(tab, &plans, &skills, false))
            (view)
        }
    };
    page("Workspace", Some(csrf.token()), &body).into_response()
}

/// The files tab: the files, and the one at `path`.
async fn files_view(web: &Web, path: Option<&str>) -> Markup {
    let files = ask(web, Request::Workspace).await;
    let file = match path {
        Some(path) => Some(
            ask(
                web,
                Request::WorkspaceFile {
                    path: path.to_owned(),
                },
            )
            .await,
        ),
        None => None,
    };
    html! {
        div.split {
            nav.files aria-label="Files" {
                @match &files {
                    Answer::Workspace { files } if files.is_empty() => {
                        p.note { "The workspace is empty. littleguy keeps files here with its file tools." }
                    }
                    Answer::Workspace { files } => ul {
                        @for file in files {
                            @let current = (path == Some(file.path.as_str())).then_some("page");
                            li {
                                a href=(workspace_url(&file.path)) aria-current=[current]
                                    hx-get=(workspace_url(&file.path)) hx-select="#file"
                                    hx-target="#file" hx-swap="outerHTML" hx-push-url="true" {
                                    span.path { (file.path) }
                                    span.size { (file.bytes) " B" }
                                }
                            }
                        }
                    },
                    other => (note(other)),
                }
            }
            (open_file(file.as_ref()))
        }
    }
}

/// An epoch millisecond as a short UTC time.
fn when(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map_or_else(String::new, |at| at.format("%b %-d, %H:%M UTC").to_string())
}

/// Text that the assistant wrote, rendered as markdown. Raw HTML in it
/// shows as text.
fn markdown(text: &str) -> Markup {
    html! { (Markstream::new(text).mode(Mode::Docs).class("markstream doc")) }
}

/// The notes of a goal or an idea, oldest first, each as markdown.
fn plan_notes(heading: &str, notes: &[crate::protocol::PlanNote]) -> Markup {
    html! {
        @if !notes.is_empty() {
            h3 { (heading) }
            ol.notes {
                @for note in notes {
                    li { p.at { (when(note.at_ms)) } (markdown(&note.text)) }
                }
            }
        }
    }
}

/// A button that sends the owner's `decision` on `id`, and swaps the open
/// view with the answer.
fn decide_button(decision: &str, id: i64, label: &str) -> Markup {
    html! {
        button hx-post=(format!("/plans/{decision}/{id}")) hx-target="#plan-view" hx-swap="outerHTML" {
            (label)
        }
    }
}

/// One goal, as a document. A proposed goal waits for the owner, as a
/// refine overlay does.
fn goal_doc(goal: &crate::protocol::GoalEntry) -> Markup {
    let proposed = goal.status == "proposed";
    html! {
        article.doc {
            header {
                h2 { span.id { "#" (goal.id) } " " (goal.title) }
                div.actions {
                    @if proposed {
                        (decide_button("accept", goal.id, "Accept"))
                        (decide_button("decline", goal.id, "Decline"))
                    } @else {
                        (decide_button("drop-goal", goal.id, "Drop"))
                    }
                }
            }
            p.meta {
                @if proposed { span.tag { "waiting for you" } } @else { span.tag.active { "active" } }
                @if let Some(idea) = goal.idea {
                    " · from " a href=(Tab::Ideas.url(Some(idea))) { "idea #" (idea) }
                }
                " · "
                @match (goal.check_every_minutes, goal.next_check_ms) {
                    (Some(every), Some(next)) => { "checks every " (every) " min, next " (when(next)) }
                    (Some(every), None) => { "checks every " (every) " min once accepted" }
                    _ => { "no checks" }
                }
            }
            h3 { "Why" }
            (markdown(&goal.why))
            (plan_notes("Progress", &goal.notes))
        }
    }
}

/// One idea, as a document.
fn idea_doc(idea: &crate::protocol::IdeaEntry) -> Markup {
    html! {
        article.doc {
            header {
                h2 { span.id { "#" (idea.id) } " " (idea.title) }
                div.actions { (decide_button("drop-idea", idea.id, "Drop")) }
            }
            p.meta { "filed " (when(idea.created_ms)) }
            (markdown(&idea.body))
            (plan_notes("Research", &idea.notes))
        }
    }
}

/// The notes tab: the notes by name, and the note `name`, or the first.
/// The owner reads them here; littleguy writes them.
fn notes_view(notes: &Answer, name: Option<&str>) -> Markup {
    html! {
        div #plan-view.split {
            @match notes {
                Answer::Notes { notes } => {
                    @let open = name.and_then(|name| notes.iter().find(|n| n.name == name)).or_else(|| notes.first());
                    nav.files aria-label="Notes" {
                        @if notes.is_empty() {
                            p.note { "No notes yet. littleguy writes them as it learns, also on a heartbeat." }
                        }
                        ul {
                            @for note in notes {
                                @let current = open.is_some_and(|o| o.name == note.name);
                                @let url = note_url(&note.name);
                                li {
                                    a href=(url) aria-current=[current.then_some("page")]
                                        hx-get=(url) hx-select="#plan-view" hx-target="#plan-view"
                                        hx-swap="outerHTML" hx-push-url="true" {
                                        span.path { (note.name) }
                                        span.size { (note.text.chars().count()) " chars" }
                                    }
                                }
                            }
                        }
                    }
                    section #file {
                        @match open {
                            Some(note) => article.doc {
                                header { h2 { (note.name) } }
                                p.meta { "changed " (when(note.updated_ms)) }
                                (markdown(&note.text))
                            },
                            None => p.note { "Nothing to open." },
                        }
                    }
                }
                other => section #file.wide { (note(other)) },
            }
        }
    }
}

/// The goals that wait for the owner, then the active ones, each in the
/// order of the store.
fn waiting_first(goals: &[crate::protocol::GoalEntry]) -> Vec<&crate::protocol::GoalEntry> {
    let mut sorted: Vec<_> = goals.iter().collect();
    sorted.sort_by_key(|goal| goal.status != "proposed");
    sorted
}

/// The goals or the ideas tab: the list, and the item `id`, or the first.
/// `notice` goes on top of the open item.
fn plans_view(tab: Tab, plans: &Answer, id: Option<i64>, notice: Option<Markup>) -> Markup {
    html! {
        div #plan-view.split {
            @match (tab, plans) {
                (Tab::Goals, Answer::Plans { goals, .. }) => {
                    // What waits for the owner comes first.
                    @let goals = waiting_first(goals);
                    @let open = id.and_then(|id| goals.iter().copied().find(|g| g.id == id)).or_else(|| goals.first().copied());
                    nav.files aria-label="Goals" {
                        @if goals.is_empty() {
                            p.note { "No goals yet. littleguy proposes them, and you accept them here." }
                        }
                        ul {
                            @for goal in &goals {
                                @let current = open.is_some_and(|o| o.id == goal.id);
                                @let url = tab.url(Some(goal.id));
                                li {
                                    a href=(url) aria-current=[current.then_some("page")]
                                        hx-get=(url) hx-select="#plan-view" hx-target="#plan-view"
                                        hx-swap="outerHTML" hx-push-url="true" {
                                        span.path { "#" (goal.id) " " (goal.title) }
                                        @if goal.status == "proposed" { span.dot title="waiting for you" {} }
                                    }
                                }
                            }
                        }
                    }
                    section #file {
                        @if let Some(notice) = notice { (notice) }
                        @match open {
                            Some(goal) => (goal_doc(goal)),
                            None => p.note { "Nothing to open." },
                        }
                    }
                }
                (Tab::Ideas, Answer::Plans { ideas, .. }) => {
                    @let open = id.and_then(|id| ideas.iter().find(|i| i.id == id)).or_else(|| ideas.first());
                    nav.files aria-label="Ideas" {
                        @if ideas.is_empty() {
                            p.note { "No ideas yet. littleguy files them from your chats and its own research." }
                        }
                        ul {
                            @for idea in ideas {
                                @let current = open.is_some_and(|o| o.id == idea.id);
                                @let url = tab.url(Some(idea.id));
                                li {
                                    a href=(url) aria-current=[current.then_some("page")]
                                        hx-get=(url) hx-select="#plan-view" hx-target="#plan-view"
                                        hx-swap="outerHTML" hx-push-url="true" {
                                        span.path { "#" (idea.id) " " (idea.title) }
                                        span.size { (idea.notes.len()) " notes" }
                                    }
                                }
                            }
                        }
                    }
                    section #file {
                        @if let Some(notice) = notice { (notice) }
                        @match open {
                            Some(idea) => (idea_doc(idea)),
                            None => p.note { "Nothing to open." },
                        }
                    }
                }
                (_, other) => section #file.wide { @if let Some(notice) = notice { (notice) } (note(other)) },
            }
        }
    }
}

/// The owner's decision on a goal or an idea. Answers with the open view of
/// its tab, and the tabs, so the waiting count stays right.
#[post("/plans/{decision}/{id}")]
async fn decide_plan(
    State(state): State<AppState>,
    session: Session,
    Path((decision, id)): Path<(String, i64)>,
) -> Response {
    use crate::protocol::PlanDecision;
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let (decision, tab) = match decision.as_str() {
        "accept" => (PlanDecision::Accept, Tab::Goals),
        "decline" => (PlanDecision::Decline, Tab::Goals),
        "drop-goal" => (PlanDecision::DropGoal, Tab::Goals),
        "drop-idea" => (PlanDecision::DropIdea, Tab::Ideas),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let answer = ask(&web, Request::Decide { decision, id }).await;
    let (plans, notice) = if matches!(answer, Answer::Plans { .. }) {
        (answer, None)
    } else {
        (ask(&web, Request::Plans).await, Some(note(&answer)))
    };
    // An accepted goal stays open. A gone item opens the first one.
    let view = plans_view(tab, &plans, Some(id), notice);
    let skills = ask(&web, Request::Skills).await;
    html! { (view) (tabs(tab, &plans, &skills, true)) }.into_response()
}

/// One skill, a draft or a file that does not load, as a document.
enum SkillItem<'a> {
    Draft(&'a crate::protocol::SkillDraftEntry),
    Skill(&'a crate::protocol::SkillEntry),
    Invalid(&'a crate::protocol::InvalidSkill),
}

/// A button that sends the owner's skill `action` on `target`, and swaps
/// the open view with the answer.
fn skill_button(action: &str, target: &str, label: &str) -> Markup {
    let mut url = format!("/skills/{action}/");
    url.extend(form_urlencoded::byte_serialize(target.as_bytes()));
    html! {
        button hx-post=(url) hx-target="#plan-view" hx-swap="outerHTML" { (label) }
    }
}

/// The document of one skill item.
fn skill_doc(item: &SkillItem<'_>) -> Markup {
    match item {
        SkillItem::Draft(draft) => html! {
            article.doc {
                header {
                    h2 { (draft.name) }
                    div.actions {
                        (skill_button("accept", &draft.id.to_string(), "Accept"))
                        (skill_button("decline", &draft.id.to_string(), "Decline"))
                    }
                }
                p.meta {
                    span.tag { "waiting for you" }
                    " · drafted by littleguy " (when(draft.created_ms))
                    @if draft.revises { " · " strong { "replaces the active skill" } }
                }
                h3 { "Description" }
                p { (draft.description) }
                h3 { "SKILL.md" }
                (markdown(&draft.body))
            }
        },
        SkillItem::Skill(skill) => html! {
            article.doc {
                header {
                    h2 { (skill.name) }
                    div.actions { (skill_button("retire", &skill.name, "Retire")) }
                }
                p.meta {
                    span.tag.active { "active" }
                    @if let Some(author) = &skill.author { " · by " (author) }
                    " · " code { (skill.path) }
                }
                h3 { "Description" }
                p { (skill.description) }
                h3 { "SKILL.md" }
                (markdown(&skill.body))
            }
        },
        SkillItem::Invalid(file) => html! {
            article.doc {
                header { h2 { "Does not load" } }
                p.meta { code { (file.path) } }
                p.error { (file.reason) }
            }
        },
    }
}

/// The skills tab: drafts that wait first, then the skills, then the files
/// that do not load. The draft `id` or the skill `name` is open, or the
/// first item. `notice` goes on top of the open item.
fn skills_view(
    skills: &Answer,
    id: Option<i64>,
    name: Option<&str>,
    notice: Option<Markup>,
) -> Markup {
    let Answer::Skills {
        skills,
        invalid,
        drafts,
    } = skills
    else {
        return html! {
            div #plan-view.split {
                section #file.wide { @if let Some(notice) = notice { (notice) } (note(skills)) }
            }
        };
    };
    let items: Vec<(String, SkillItem<'_>)> = drafts
        .iter()
        .map(|draft| {
            (
                format!("{}&id={}", Tab::Skills.url(None), draft.id),
                SkillItem::Draft(draft),
            )
        })
        .chain(
            skills
                .iter()
                .map(|skill| (skill_url(&skill.name), SkillItem::Skill(skill))),
        )
        .chain(
            invalid
                .iter()
                .map(|file| (Tab::Skills.url(None), SkillItem::Invalid(file))),
        )
        .collect();
    let chosen = |item: &SkillItem<'_>| match item {
        SkillItem::Draft(draft) => id == Some(draft.id),
        SkillItem::Skill(skill) => name == Some(skill.name.as_str()),
        SkillItem::Invalid(_) => false,
    };
    let open = items
        .iter()
        .position(|(_, item)| chosen(item))
        .or_else(|| (!items.is_empty()).then_some(0));
    html! {
        div #plan-view.split {
            nav.files aria-label="Skills" {
                @if items.is_empty() {
                    p.note { "No skills yet. Put a SKILL.md in the skills folder, or let littleguy draft one." }
                }
                ul {
                    @for (index, (url, item)) in items.iter().enumerate() {
                        @let current = (open == Some(index)).then_some("page");
                        li {
                            a href=(url) aria-current=[current]
                                hx-get=(url) hx-select="#plan-view" hx-target="#plan-view"
                                hx-swap="outerHTML" hx-push-url="true" {
                                @match item {
                                    SkillItem::Draft(draft) => {
                                        span.path { (draft.name) }
                                        span.dot title="waiting for you" {}
                                    }
                                    SkillItem::Skill(skill) => span.path { (skill.name) },
                                    SkillItem::Invalid(file) => {
                                        span.path { (file.path) }
                                        span.size { "does not load" }
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
                    Some((_, item)) => (skill_doc(item)),
                    None => p.note { "Nothing to open." },
                }
            }
        }
    }
}

/// The owner's decision on a skill: accept or decline a draft, or retire a
/// skill. Answers with the skills tab, and the tabs out of band.
#[post("/skills/{action}/{target}")]
async fn decide_skill(
    State(state): State<AppState>,
    session: Session,
    Path((action, target)): Path<(String, String)>,
) -> Response {
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let draft = target.parse::<i64>();
    let request = match (action.as_str(), draft) {
        ("accept", Ok(draft)) => Request::AcceptSkill { draft },
        ("decline", Ok(draft)) => Request::DeclineSkill { draft },
        ("retire", _) => Request::RetireSkill {
            name: target.clone(),
        },
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // An accepted draft opens as the skill it became.
    let accepted = match (&request, ask(&web, Request::Skills).await) {
        (Request::AcceptSkill { draft }, Answer::Skills { drafts, .. }) => drafts
            .into_iter()
            .find(|entry| entry.id == *draft)
            .map(|entry| entry.name),
        _ => None,
    };
    let answer = ask(&web, request).await;
    let (skills, notice) = if matches!(answer, Answer::Skills { .. }) {
        (answer, None)
    } else {
        (ask(&web, Request::Skills).await, Some(note(&answer)))
    };
    let plans = ask(&web, Request::Plans).await;
    let view = skills_view(&skills, None, accepted.as_deref(), notice);
    html! { (view) (tabs(Tab::Skills, &plans, &skills, true)) }.into_response()
}

/// Whether `path` names a Markdown file, in any case.
pub(super) fn is_markdown(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
}

/// The pane of the open file. Markdown renders. Other text shows as it is.
fn open_file(file: Option<&Answer>) -> Markup {
    html! {
        section #file {
            @match file {
                None => p.note { "Pick a file." },
                Some(Answer::WorkspaceFile { path, bytes, text }) => {
                    h2 { (path) }
                    @match text {
                        Some(text) if is_markdown(path) => {
                            (Markstream::new(text.as_str()).mode(Mode::Docs).class("markstream doc"))
                        }
                        Some(text) => pre { (text) },
                        None => p.note { (bytes) " bytes. The file is not text, or it is too large to show." },
                    }
                }
                Some(other) => (note(other)),
            }
        }
    }
}

/// The form of a sign-in link.
#[derive(Debug, Deserialize)]
struct Token {
    token: String,
}

/// Redeems a sign-in link, then sends the browser to the home page.
#[get("/login")]
async fn login(
    State(state): State<AppState>,
    session: Session,
    Query(form): Query<Token>,
) -> Response {
    let Some(web) = web(&state) else {
        return broken();
    };
    if !web.logins.redeem(&form.token) {
        let body = page(
            "Sign in",
            None,
            &html! { h1 { "littleguy" } p { "This link was used or is too old. " (SIGN_IN) } },
        );
        return (StatusCode::FORBIDDEN, body).into_response();
    }
    // A new id, so a session id that was set before sign-in is worth nothing.
    session.rotate_id().await;
    session.insert(OWNER_KEY, "1").await;
    Redirect::to("/").into_response()
}

/// Ends the owner session.
#[post("/logout")]
async fn logout(session: Session) -> Response {
    session.destroy().await;
    Redirect::to("/").into_response()
}

/// The form of a chat message.
#[derive(Debug, Deserialize)]
struct Message {
    text: String,
}

/// Sends one message to the assistant, and shows its answer as the model
/// writes it. The answer streams from [`chat_stream`].
/// Starts a new conversation. The answer replaces the thread.
#[post("/new")]
async fn new_conversation(State(state): State<AppState>, session: Session) -> Response {
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    match ask(&web, Request::NewConversation).await {
        Answer::Cleared => html! { p.empty { "Say hi to littleguy." } }.into_response(),
        other => note(&other).into_response(),
    }
}

#[post("/chat")]
async fn chat(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<Message>,
) -> Response {
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let id = web.streams.put(web.handle.chat_live(form.text));
    answer_markdown(Markstream::stream(format!("/chat/stream/{id}"))).into_response()
}

/// Streams one answer that [`chat`] started, once.
#[get("/chat/stream/{id}")]
async fn chat_stream(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<String>,
) -> Response {
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    Sse::new(answer_events(web.streams.take(&id)))
        .keep_alive(autumn_web::sse::keep_alive())
        .into_response()
}

/// Where the events of an answer are.
enum Feed {
    /// The pieces come from the daemon.
    Open(mpsc::Receiver<Piece>),
    /// One event is left.
    Last(StreamEvent),
    /// The stream is over.
    Closed,
}

/// The markstream events of one answer: the text while the model writes
/// it, then the whole answer in its place. The answer is the text that
/// counts, so a piece that a slow reader lost does no harm. With no pieces,
/// the stream fails at once.
fn answer_events(
    pieces: Option<mpsc::Receiver<Piece>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let start = pieces.map_or(Feed::Last(StreamEvent::Error), Feed::Open);
    stream::unfold(start, |feed| async move {
        let (event, next) = match feed {
            Feed::Closed => return None,
            Feed::Last(event) => (event, Feed::Closed),
            Feed::Open(mut pieces) => match pieces.recv().await {
                Some(Piece::Restart) => (StreamEvent::replace(""), Feed::Open(pieces)),
                Some(Piece::Text(text)) => (StreamEvent::chunk(text), Feed::Open(pieces)),
                Some(Piece::Answer(answer)) => {
                    let (text, ok) = render(&answer);
                    let last = if ok {
                        StreamEvent::Done
                    } else {
                        StreamEvent::Error
                    };
                    (StreamEvent::replace(text), Feed::Last(last))
                }
                None => (StreamEvent::Error, Feed::Closed),
            },
        };
        Some((Ok(Event::from(event)), next))
    })
}

/// The approvals section, with `notice` on top.
async fn approvals_section(web: &Web, notice: Option<Markup>) -> Markup {
    let answer = ask(web, Request::Approvals).await;
    html! {
        section #approvals {
            h2 { "Approvals" }
            @if let Some(notice) = notice { (notice) }
            @match &answer {
                Answer::Approvals { entries } if entries.is_empty() => p.note { "No call waits for approval." },
                Answer::Approvals { entries } => ul { @for entry in entries { (approval(entry)) } },
                other => (note(other)),
            }
        }
    }
}

/// One call that waits, with its buttons.
fn approval(entry: &ApprovalEntry) -> Markup {
    html! {
        li {
            strong { "#" (entry.id) " " (entry.tool) }
            pre { (entry.text) }
            button hx-post={ "/approvals/" (entry.id) "/approve" } hx-target="#approvals" hx-swap="outerHTML" { "Approve" }
            button hx-post={ "/approvals/" (entry.id) "/deny" } hx-target="#approvals" hx-swap="outerHTML" { "Deny" }
        }
    }
}

/// The approvals section.
#[get("/approvals")]
async fn approvals(State(state): State<AppState>, session: Session) -> Response {
    show(&state, &session, Section::Approvals).await
}

/// Approves call `id`. The page does not wait for the run.
#[post("/approvals/{id}/approve")]
async fn approve(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    decide(&state, &session, id, Request::Approve { id }, "Approved").await
}

/// Denies call `id`. The page does not wait for the run.
#[post("/approvals/{id}/deny")]
async fn deny(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    let request = Request::Deny { id, reason: None };
    decide(&state, &session, id, request, "Denied").await
}

/// How often a decided call asks for its result.
const RESULT_EVERY: &str = "every 1s";

/// Starts the decision `request` on call `id` and answers at once. The
/// decision drives the run on, which takes a model turn, so it goes on
/// while the section asks for its result.
async fn decide(
    state: &AppState,
    session: &Session,
    id: i64,
    request: Request,
    done: &str,
) -> Response {
    if !is_owner(session).await {
        return unsigned(true);
    }
    let Some(web) = web(state) else {
        return broken();
    };
    let asked = web.clone();
    web.decisions
        .start(id, async move { ask(&asked, request).await });
    let result = format!("/approvals/{id}/result");
    html! {
        section #approvals hx-get=(result) hx-trigger=(RESULT_EVERY) hx-swap="outerHTML" {
            h2 { "Approvals" }
            p.note { (done) " request #" (id) ". It runs now, and its answer shows here." }
        }
    }
    .into_response()
}

/// The result of the decision on call `id`. While the run goes on, it
/// answers with no content, so the section keeps its notice and asks again.
#[get("/approvals/{id}/result")]
async fn decided(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    if !is_owner(&session).await {
        return unsigned(true);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    match web.decisions.take(id) {
        Poll::Running => StatusCode::NO_CONTENT.into_response(),
        Poll::Done(answer) => render_section(&web, Section::Approvals, Some(note(&answer)))
            .await
            .into_response(),
        Poll::Unknown => render_section(&web, Section::Approvals, None)
            .await
            .into_response(),
    }
}

/// The pairing section, with `notice` on top.
async fn pairs_section(web: &Web, notice: Option<Markup>) -> Markup {
    let answer = ask(web, Request::Pairs).await;
    html! {
        section #pairs {
            h2 { "Who can talk to it" }
            @if let Some(notice) = notice { (notice) }
            @match &answer {
                Answer::Pairs { entries } if entries.is_empty() => p.note { "No allowed senders." },
                Answer::Pairs { entries } => ul { @for entry in entries { (pair_line(entry)) } },
                other => (note(other)),
            }
            form hx-post="/pairs/pair" hx-target="#pairs" hx-swap="outerHTML" {
                input name="code" placeholder="ABCD-1234" required aria-label="Pairing code";
                button type="submit" { "Pair" }
            }
        }
    }
}

/// One allowed sender. A paired sender has an unpair button.
fn pair_line(entry: &PairEntry) -> Markup {
    let vals = serde_json::json!({ "sender": entry.sender }).to_string();
    html! {
        li {
            (entry.channel) " " code { (entry.sender) } " (" (entry.source) ")"
            @if entry.source == "paired" {
                button hx-post="/pairs/unpair" hx-vals=(vals) hx-target="#pairs" hx-swap="outerHTML" { "Unpair" }
            }
        }
    }
}

/// The pairing section.
#[get("/pairs")]
async fn pairs(State(state): State<AppState>, session: Session) -> Response {
    show(&state, &session, Section::Pairs).await
}

/// The form of a pairing code.
#[derive(Debug, Deserialize)]
struct Code {
    code: String,
}

/// Redeems a pairing code.
#[post("/pairs/pair")]
async fn pair(State(state): State<AppState>, session: Session, Form(form): Form<Code>) -> Response {
    let request = Request::Pair { code: form.code };
    act(&state, &session, request, Section::Pairs).await
}

/// The form of an unpair button.
#[derive(Debug, Deserialize)]
struct Sender {
    sender: String,
}

/// Takes a paired sender off its channel.
#[post("/pairs/unpair")]
async fn unpair(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<Sender>,
) -> Response {
    let request = Request::Unpair {
        sender: form.sender,
    };
    act(&state, &session, request, Section::Pairs).await
}

/// The refine section, with `notice` on top.
async fn overlays_section(web: &Web, notice: Option<Markup>) -> Markup {
    let answer = ask(web, Request::Overlays).await;
    html! {
        section #refine {
            h2 { "Refine" }
            @if let Some(notice) = notice { (notice) }
            @match &answer {
                Answer::Overlays { active, entries } => {
                    @if entries.is_empty() { p.note { "No overlay versions yet." } }
                    @else { ul { @for entry in entries { (overlay_line(entry)) } } }
                    @if active.is_none() { p.note { "No version is active: the assistant runs on its config." } }
                    button hx-post="/refine/propose" hx-target="#refine" hx-swap="outerHTML" hx-disabled-elt="this" { "Propose" }
                    @if active.is_some() {
                        button hx-post="/refine/rollback" hx-target="#refine" hx-swap="outerHTML" { "Roll back" }
                    }
                }
                other => (note(other)),
            }
        }
    }
}

/// One overlay version, with its activate button.
fn overlay_line(entry: &OverlayEntry) -> Markup {
    let parts = if entry.parts.is_empty() {
        String::from("no change")
    } else {
        entry.parts.join(", ")
    };
    html! {
        li {
            a href={ "/refine/" (entry.version) } { "v" (entry.version) }
            " " (entry.author) " " (entry.created_at) " (" (parts) ")"
            @if entry.active { " " strong { "active" } }
            @else {
                button hx-post={ "/refine/" (entry.version) "/activate" } hx-target="#refine" hx-swap="outerHTML" { "Activate" }
            }
        }
    }
}

/// The refine section.
#[get("/refine")]
async fn overlays(State(state): State<AppState>, session: Session) -> Response {
    show(&state, &session, Section::Refine).await
}

/// One overlay version as JSON.
#[get("/refine/{version}")]
async fn overlay(
    State(state): State<AppState>,
    session: Session,
    Path(version): Path<u64>,
) -> Response {
    if !is_owner(&session).await {
        return unsigned(false);
    }
    let Some(web) = web(&state) else {
        return broken();
    };
    let answer = ask(&web, Request::OverlayShow { version }).await;
    let body = match &answer {
        Answer::Overlay { entry, overlay } => html! {
            h1 { "Overlay v" (entry.version) }
            p { (entry.author) " " (entry.created_at) @if entry.active { " (active)" } }
            pre { (overlay) }
            a href="/" { "Back" }
        },
        other => html! { h1 { "Overlay v" (version) } (note(other)) a href="/" { "Back" } },
    };
    page("Overlay", None, &body).into_response()
}

/// Asks the proposer for an overlay.
#[post("/refine/propose")]
async fn propose(State(state): State<AppState>, session: Session) -> Response {
    act(&state, &session, Request::RefinePropose, Section::Refine).await
}

/// Activates overlay `version`.
#[post("/refine/{version}/activate")]
async fn activate(
    State(state): State<AppState>,
    session: Session,
    Path(version): Path<u64>,
) -> Response {
    let request = Request::OverlayActivate { version };
    act(&state, &session, request, Section::Refine).await
}

/// Activates the version before.
#[post("/refine/rollback")]
async fn rollback(State(state): State<AppState>, session: Session) -> Response {
    act(&state, &session, Request::OverlayRollback, Section::Refine).await
}

/// The OAuth connections, when the build has them.
async fn connections_section(web: &Web) -> Markup {
    let answer = ask(web, Request::Connections).await;
    html! {
        section #connections {
            h2 { "Connections" }
            @match &answer {
                Answer::Connections { entries } if entries.is_empty() => p.note { "No connections. Run `lg connect <integration>`." },
                Answer::Connections { entries } => ul { @for entry in entries { (connection_line(entry)) } },
                other => (note(other)),
            }
        }
    }
}

/// One OAuth connection.
fn connection_line(entry: &ConnectionEntry) -> Markup {
    html! {
        li {
            (entry.integration) " " code { (entry.connection) } " " (entry.provider) " " (entry.created_at)
            @if entry.refresh_exhausted { " " strong { "connect again" } }
        }
    }
}

/// The section that an action changes.
#[derive(Debug, Clone, Copy)]
enum Section {
    Approvals,
    Pairs,
    Refine,
}

/// The markup of `section` for `web`, with `notice` on top.
async fn render_section(web: &Web, section: Section, notice: Option<Markup>) -> Markup {
    match section {
        Section::Approvals => approvals_section(web, notice).await,
        Section::Pairs => pairs_section(web, notice).await,
        Section::Refine => overlays_section(web, notice).await,
    }
}

/// Renders one section for the owner.
async fn show(state: &AppState, session: &Session, section: Section) -> Response {
    if !is_owner(session).await {
        return unsigned(true);
    }
    let Some(web) = web(state) else {
        return broken();
    };
    render_section(&web, section, None).await.into_response()
}

/// Sends `request`, then renders `section` again with the answer on top.
async fn act(state: &AppState, session: &Session, request: Request, section: Section) -> Response {
    if !is_owner(session).await {
        return unsigned(true);
    }
    let Some(web) = web(state) else {
        return broken();
    };
    let answer = ask(&web, request).await;
    render_section(&web, section, Some(note(&answer)))
        .await
        .into_response()
}
