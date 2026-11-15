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
        login,
        logout,
        chat,
        chat_stream,
        approvals,
        approve,
        deny,
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
fn note(answer: &Answer) -> Markup {
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
                p.hint { "Enter sends · Shift+Enter adds a line" }
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

/// The file that the workspace page opens.
#[derive(Debug, Deserialize)]
struct Open {
    path: Option<String>,
}

/// The URL of the workspace page with `path` open.
fn workspace_url(path: &str) -> String {
    let mut url = String::from("/workspace?path=");
    url.extend(form_urlencoded::byte_serialize(path.as_bytes()));
    url
}

/// The files of the assistant workspace, and the one that is open. A link
/// loads the whole page, so the page works without a script. With htmx, a
/// click swaps only the open file.
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
    let files = ask(&web, Request::Workspace).await;
    let file = match &open.path {
        Some(path) => Some(ask(&web, Request::WorkspaceFile { path: path.clone() }).await),
        None => None,
    };
    let body = html! {
        (bar(csrf.token(), "/workspace"))
        main.workspace {
            nav.files aria-label="Files" {
                @match &files {
                    Answer::Workspace { files } if files.is_empty() => {
                        p.note { "The workspace is empty. littleguy keeps files here with its file tools." }
                    }
                    Answer::Workspace { files } => ul {
                        @for file in files {
                            @let current = (open.path.as_deref() == Some(file.path.as_str())).then_some("page");
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
    };
    page("Workspace", Some(csrf.token()), &body).into_response()
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

/// Approves call `id`.
#[post("/approvals/{id}/approve")]
async fn approve(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    act(
        &state,
        &session,
        Request::Approve { id },
        Section::Approvals,
    )
    .await
}

/// Denies call `id`.
#[post("/approvals/{id}/deny")]
async fn deny(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    let request = Request::Deny { id, reason: None };
    act(&state, &session, request, Section::Approvals).await
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
