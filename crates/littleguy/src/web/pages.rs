//! The pages: maud markup, htmx for each action.
//!
//! Every page and action needs the owner in the session. An action sends
//! one [`Request`] through the daemon handle, as `lg` does over the socket,
//! and answers with the section that it changed. So the pages hold no state
//! of their own, and they can do nothing that `lg` cannot.

use autumn_web::AppState;
use autumn_web::htmx::HTMX_JS_PATH;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::http::StatusCode;
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use autumn_web::security::CsrfToken;
use autumn_web::session::Session;
use serde::Deserialize;

use super::Web;
use crate::client::render;
use crate::protocol::{
    ApprovalEntry, ConnectionEntry, OverlayEntry, PairEntry, Request, Response as Answer,
};

/// The session key that marks the owner.
pub const OWNER_KEY: &str = "lg.owner";

/// The text for a browser with no owner session.
const SIGN_IN: &str = "Run `lg ui` on this machine and open the link that it prints.";

/// The style of every page. It is small, so it sits in the page.
const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:52rem;margin:2rem auto;\
padding:0 1rem;line-height:1.5}section{border-top:1px solid #8884;padding:.5rem 0 1rem}\
ul{padding-left:1.2rem}pre{white-space:pre-wrap;background:#8881;padding:.5rem}\
textarea{width:100%}.note{color:#666}.error{color:#b00}button{margin-left:.3rem}";

/// The routes of the pages.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![
        index, login, logout, chat, approvals, approve, deny, pairs, pair, unpair, overlays,
        overlay, propose, activate, rollback,
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
            &html! { h1 { "littleguy" } p { (SIGN_IN) } },
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
                style { (STYLE) }
                script src=(HTMX_JS_PATH) defer {}
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

/// The page of the owner: chat, approvals, pairing, refine and connections.
#[get("/")]
async fn index(State(state): State<AppState>, session: Session, csrf: CsrfToken) -> Response {
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
        header {
            h1 { "littleguy" }
            form method="post" action="/logout" {
                input type="hidden" name="_csrf" value=(csrf.token());
                button type="submit" { "Sign out" }
            }
        }
        section {
            h2 { "Chat" }
            form hx-post="/chat" hx-target="#answer" hx-disabled-elt="button" {
                textarea name="text" rows="3" required aria-label="Message" {}
                button type="submit" { "Send" }
            }
            div #answer {}
        }
        (approvals)
        (pairs)
        (overlays)
        (connections)
    };
    page("Home", Some(csrf.token()), &body).into_response()
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

/// Sends one message to the assistant and shows the answer.
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
    let request = Request::Chat {
        message_id: crate::client::fresh_message_id(),
        text: form.text,
    };
    let answer = ask(&web, request).await;
    let (text, ok) = render(&answer);
    let class = if ok { "reply" } else { "error" };
    html! { pre class=(class) { (text) } }.into_response()
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
