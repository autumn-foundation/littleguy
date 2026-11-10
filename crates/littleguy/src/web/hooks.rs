//! The inbound hooks: routes that wake a sleeping host (M5.3, ADR 0015).
//!
//! A host such as a Fly Sprite pauses about a minute after its last inbound
//! request. A paused host fires no timer, and an outbound socket does not
//! keep it awake. So something outside calls a hook, and the hook holds the
//! request open until the work it starts is done.
//!
//! Each hook needs the bearer token from the variable that `[hooks]`
//! names. The token is compared in constant time and never logged.

use autumn_web::AppState;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::http::{HeaderMap, StatusCode, header};
use autumn_web::reexports::axum::response::{IntoResponse, Response};

use super::Web;
use super::logins::same;

/// The shortest token that a hook accepts.
pub const MIN_TOKEN_LEN: usize = 32;

/// The bearer token of the hooks. Its `Debug` output hides the value.
#[derive(Clone)]
pub struct HookToken(String);

impl std::fmt::Debug for HookToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HookToken(..)")
    }
}

/// Why a hook token is not usable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    /// The variable is not set.
    #[error("`{0}` is not set: the hooks need a bearer token")]
    Missing(String),
    /// The value is too short to guess safely.
    #[error("`{0}` is shorter than {MIN_TOKEN_LEN} characters")]
    Short(String),
}

impl HookToken {
    /// The token in `value`, read from the variable `name`.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is missing or shorter than
    /// [`MIN_TOKEN_LEN`].
    pub fn new(name: &str, value: Option<String>) -> Result<Self, TokenError> {
        let value = value
            .filter(|v| !v.is_empty())
            .ok_or_else(|| TokenError::Missing(name.to_owned()))?;
        if value.len() < MIN_TOKEN_LEN {
            return Err(TokenError::Short(name.to_owned()));
        }
        Ok(Self(value))
    }

    /// Whether `headers` carry this token as a bearer token.
    fn admits(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|given| same(given.as_bytes(), self.0.as_bytes()))
    }
}

/// The routes of the hooks.
pub fn routes() -> Vec<autumn_web::Route> {
    routes![tick]
}

/// Ticks the daemon and answers once the runs block.
#[post("/hooks/tick")]
async fn tick(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(web) = state.extension::<Web>() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(token) = web.hooks.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !token.admits(&headers) {
        return (StatusCode::UNAUTHORIZED, "a hook needs its bearer token\n").into_response();
    }
    match web.handle.tick().await {
        Ok(true) => (StatusCode::OK, "ticked\n").into_response(),
        Ok(false) => (StatusCode::INTERNAL_SERVER_ERROR, "the tick failed\n").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_token_must_be_set_and_long() {
        assert_eq!(
            HookToken::new("T", None).unwrap_err(),
            TokenError::Missing(String::from("T"))
        );
        assert_eq!(
            HookToken::new("T", Some(String::new())).unwrap_err(),
            TokenError::Missing(String::from("T"))
        );
        assert_eq!(
            HookToken::new("T", Some(String::from("short"))).unwrap_err(),
            TokenError::Short(String::from("T"))
        );
        let token = HookToken::new("T", Some(GOOD.to_owned())).unwrap();
        assert_eq!(format!("{token:?}"), "HookToken(..)");
    }

    #[test]
    fn only_the_bearer_token_is_admitted() {
        let token = HookToken::new("T", Some(GOOD.to_owned())).unwrap();
        let mut headers = HeaderMap::new();
        assert!(!token.admits(&headers));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {GOOD}x").parse().unwrap(),
        );
        assert!(!token.admits(&headers));
        headers.insert(header::AUTHORIZATION, GOOD.parse().unwrap());
        assert!(!token.admits(&headers));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {GOOD}").parse().unwrap(),
        );
        assert!(token.admits(&headers));
    }
}
