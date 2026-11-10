//! The Slack hooks of the Events API (M5.4, ADR 0016). Feature `slack`.
//!
//! Slack calls these routes, so a sleeping host wakes for each message. Each
//! request carries a signature from the signing secret of the app. A route
//! checks it before it parses the body, and refuses a request without it.
//!
//! Slack waits three seconds for the answer. A run takes longer, so each
//! route answers first and runs the work in a task. The message id is the
//! key of its op, so a retry from Slack runs nothing new.

use std::time::{SystemTime, UNIX_EPOCH};

use autumn_plugin_slack::{SIGNATURE_HEADER, TIMESTAMP_HEADER, Verifier};
use autumn_web::AppState;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::body::Bytes;
use autumn_web::reexports::axum::http::{HeaderMap, StatusCode};
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use littleguy_channels::SigningSecret;
use littleguy_channels::slack::{parse_dm, parse_press};
use serde_json::{Value, json};

use super::Web;
use crate::daemon::SlackInbox;

/// How far the clock of a request may stray from ours, in seconds. Slack
/// asks for five minutes, which stops a replay of an old request.
const TOLERANCE_SECS: u64 = 300;

/// The Slack side of the hooks: the signature check and the way in.
#[derive(Debug, Clone)]
pub struct SlackHook {
    verifier: Verifier,
    inbox: SlackInbox,
}

impl SlackHook {
    /// The hooks that check each request with `secret`, and hand it to
    /// `inbox`.
    #[must_use]
    pub fn new(secret: &SigningSecret, inbox: SlackInbox) -> Self {
        Self {
            verifier: Verifier::new(secret.expose(), &[], TOLERANCE_SECS),
            inbox,
        }
    }

    /// Whether Slack signed `body` with the secret, at about this time.
    fn admits(&self, headers: &HeaderMap, body: &[u8]) -> bool {
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.verifier
            .verify(
                header(TIMESTAMP_HEADER),
                header(SIGNATURE_HEADER),
                body,
                now,
            )
            .is_ok()
    }
}

/// The routes of the Slack hooks.
pub fn routes() -> Vec<autumn_web::Route> {
    routes![events, interactions]
}

/// The hook of `state` for a request with `headers` and `body`, or the
/// answer that refuses it.
fn admitted(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<SlackHook, (StatusCode, &'static str)> {
    let Some(web) = state.extension::<Web>() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the app is starting\n"));
    };
    let Some(hook) = web.slack.as_ref() else {
        return Err((StatusCode::NOT_FOUND, "Slack does not run over the hooks\n"));
    };
    if !hook.admits(headers, body) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "a Slack hook needs its signature\n",
        ));
    }
    Ok(hook.clone())
}

/// Takes one event from the Events API.
#[post("/hooks/slack/events")]
async fn events(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let hook = match admitted(&state, &headers, &body) {
        Ok(hook) => hook,
        Err(refused) => return refused.into_response(),
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match payload["type"].as_str() {
        Some("url_verification") => payload["challenge"].as_str().map_or_else(
            || StatusCode::BAD_REQUEST.into_response(),
            |challenge| Json(json!({ "challenge": challenge })).into_response(),
        ),
        Some("event_callback") => {
            // Each other event, such as a message of the bot itself, ends
            // here with a plain answer, so Slack does not send it again.
            if let Some(message) = parse_dm(&payload["event"]) {
                tokio::spawn(async move { hook.inbox.message(message).await });
            }
            StatusCode::OK.into_response()
        }
        _ => StatusCode::OK.into_response(),
    }
}

/// Takes one button press.
#[post("/hooks/slack/interactions")]
async fn interactions(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let hook = match admitted(&state, &headers, &body) {
        Ok(hook) => hook,
        Err(refused) => return refused.into_response(),
    };
    let payload = form_urlencoded::parse(&body)
        .find(|(key, _)| key == "payload")
        .and_then(|(_, value)| serde_json::from_str::<Value>(&value).ok());
    let Some(payload) = payload else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Some(press) = parse_press(&payload) {
        tokio::spawn(async move { hook.inbox.press(press).await });
    }
    StatusCode::OK.into_response()
}
