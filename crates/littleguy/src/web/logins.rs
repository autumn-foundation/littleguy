//! The sign-in links of the web pages.
//!
//! `lg ui` asks the daemon over the owner socket for a link. The link holds a
//! token that works once, within [`TOKEN_TTL`]. The page that redeems it puts
//! the owner into the session, so only a person who can use the socket can
//! sign a browser in.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a sign-in link works.
pub const TOKEN_TTL: Duration = Duration::from_secs(300);

/// The most links that work at one time. A new link pushes out the oldest.
pub const MAX_LIVE: usize = 8;

/// The sign-in tokens that work now, and the address of the pages.
#[derive(Debug, Clone)]
pub struct Logins {
    live: Arc<Mutex<Vec<(String, Instant)>>>,
    base: String,
}

impl Logins {
    /// Links that point at `base`, such as `http://127.0.0.1:3004`.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            live: Arc::new(Mutex::new(Vec::new())),
            base: base.into(),
        }
    }

    /// A fresh sign-in link.
    #[must_use]
    pub fn link(&self) -> String {
        self.link_at(Instant::now())
    }

    /// A fresh sign-in link, made at `now`. A test seam.
    #[must_use]
    pub fn link_at(&self, now: Instant) -> String {
        let token = fresh_token();
        {
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            live.retain(|(_, made)| now.saturating_duration_since(*made) < TOKEN_TTL);
            if live.len() >= MAX_LIVE {
                live.remove(0);
            }
            live.push((token.clone(), now));
        }
        format!("{}/login?token={token}", self.base)
    }

    /// Whether `token` works now. A token works once.
    #[must_use]
    pub fn redeem(&self, token: &str) -> bool {
        self.redeem_at(token, Instant::now())
    }

    /// Whether `token` works at `now`. A test seam.
    #[must_use]
    pub fn redeem_at(&self, token: &str, now: Instant) -> bool {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.retain(|(_, made)| now.saturating_duration_since(*made) < TOKEN_TTL);
        let found = live
            .iter()
            .position(|(known, _)| same(known.as_bytes(), token.as_bytes()));
        if let Some(index) = found {
            live.remove(index);
        }
        drop(live);
        found.is_some()
    }
}

/// Whether `a` and `b` are equal, in a time that does not depend on where
/// they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 32 random bytes as hex.
fn fresh_token() -> String {
    let mut bytes = [0_u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness: fall back to the clock and the process id. The
        // token still works once and only for minutes.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
        bytes[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    }
    bytes.iter().fold(String::with_capacity(64), |mut text, b| {
        use std::fmt::Write as _;
        let _ = write!(text, "{b:02x}");
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_of(link: &str) -> &str {
        link.split_once("token=").map(|(_, token)| token).unwrap()
    }

    #[test]
    fn a_link_works_once() {
        let logins = Logins::new("http://127.0.0.1:3004");
        let link = logins.link();
        assert!(link.starts_with("http://127.0.0.1:3004/login?token="));
        let token = token_of(&link);
        assert_eq!(token.len(), 64);
        assert!(logins.redeem(token));
        assert!(!logins.redeem(token), "a token works once");
        assert!(!logins.redeem("nothing"));
        assert!(!logins.redeem(""));
    }

    #[test]
    fn a_link_stops_working_after_its_ttl() {
        let logins = Logins::new("http://x");
        let start = Instant::now();
        let link = logins.link_at(start);
        let late = start + TOKEN_TTL;
        assert!(!logins.redeem_at(token_of(&link), late));
        let fresh = logins.link_at(start);
        let early = start + Duration::from_secs(TOKEN_TTL.as_secs() - 1);
        assert!(logins.redeem_at(token_of(&fresh), early));
    }

    #[test]
    fn a_new_link_pushes_out_the_oldest() {
        let logins = Logins::new("http://x");
        let now = Instant::now();
        let links: Vec<String> = (0..=MAX_LIVE).map(|_| logins.link_at(now)).collect();
        assert!(!logins.redeem_at(token_of(&links[0]), now));
        assert!(logins.redeem_at(token_of(&links[MAX_LIVE]), now));
        assert_ne!(token_of(&links[1]), token_of(&links[2]));
    }

    #[test]
    fn same_compares_bytes() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
