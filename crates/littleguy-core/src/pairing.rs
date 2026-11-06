//! Pairing: how a stranger becomes an allowed sender.
//!
//! A stranger on a pairing channel gets a code. The owner reads the code out
//! of band and redeems it on the local socket. Only then does the sender
//! join the allowed set. So a stranger never adds itself, and the owner
//! approves one named sender at a time.
//!
//! [`PendingPairings`] holds the live codes in memory. A code lives for
//! [`PAIRING_TTL_SECS`], and at most [`MAX_PENDING`] codes live at once, so
//! a flood of strangers cannot grow the daemon without bound. A restart
//! drops every code. A stranger then asks again and gets a new one.
//!
//! [`issue_decision`] and [`is_live`] are the kernel.
//! `formal/verus/src/pairing.rs` proves both for every input.

use std::collections::BTreeMap;
use std::fmt;

use crate::ids::{ChannelId, MessageId, SenderId};

/// How long a pairing code lives, in seconds.
pub const PAIRING_TTL_SECS: u64 = 10 * 60;

/// The most live codes at once.
pub const MAX_PENDING: usize = 32;

/// The letters of a code: Crockford base32, with no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The number of letters in a code. 8 letters hold 40 random bits.
const CODE_LEN: usize = 8;

/// A pairing code, such as `7K2D-9QXM`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PairingCode(String);

impl PairingCode {
    /// The code of 5 random bytes. The caller supplies the randomness, so
    /// this crate holds no I/O.
    #[must_use]
    pub fn from_random(bytes: [u8; 5]) -> Self {
        let bits = bytes
            .iter()
            .fold(0_u64, |bits, byte| (bits << 8) | u64::from(*byte));
        let code = (0..CODE_LEN)
            .map(|i| {
                let shift = 5 * (CODE_LEN - 1 - i);
                // The mask keeps 5 bits, so the index is below 32.
                let index = usize::try_from((bits >> shift) & 0x1f).unwrap_or(0);
                char::from(ALPHABET[index])
            })
            .collect();
        Self(code)
    }

    /// Reads a code as a person types it. Case, dashes and spaces do not
    /// matter, and `I`, `L` and `O` read as `1`, `1` and `0`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let code: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| match c.to_ascii_uppercase() {
                'I' | 'L' => '1',
                'O' => '0',
                other => other,
            })
            .collect();
        let valid = code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b));
        valid.then_some(Self(code))
    }
}

/// The code with a dash in the middle, so a person reads it easily.
impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (head, tail) = self.0.split_at(CODE_LEN / 2);
        write!(f, "{head}-{tail}")
    }
}

/// What [`issue_decision`] does with a request for a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    /// Answer with the live code that the sender holds already.
    Reuse,
    /// Make a new code.
    New,
    /// Refuse: too many codes are live.
    Full,
}

/// The issue decision. `formal/verus/src/pairing.rs` proves it.
///
/// - A sender with a live code gets the same code again.
/// - Another sender gets a new code while fewer than `cap` codes live.
#[must_use]
pub const fn issue_decision(has_live_code: bool, live_codes: usize, cap: usize) -> Issue {
    // mirror-begin issue_decision
    if has_live_code {
        return Issue::Reuse;
    }
    if live_codes < cap {
        Issue::New
    } else {
        Issue::Full
    }
    // mirror-end issue_decision
}

/// Whether a code issued at `issued_at` lives at `now`. A clock that went
/// back counts as expired. `formal/verus/src/pairing.rs` proves it.
#[must_use]
pub const fn is_live(issued_at: u64, now: u64, ttl: u64) -> bool {
    // mirror-begin is_live
    if now < issued_at {
        return false;
    }
    now - issued_at < ttl
    // mirror-end is_live
}

/// One stranger who waits for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The channel of the stranger.
    pub channel: ChannelId,
    /// The stranger.
    pub sender: SenderId,
    /// Where the channel sends a reply, such as a Discord DM channel id.
    pub reply_to: String,
    /// The message that asked for the code. Delivery starts after it.
    pub message_id: MessageId,
    /// When the code was issued, in seconds since the Unix epoch.
    pub issued_at: u64,
}

/// The live pairing codes.
#[derive(Debug, Clone)]
pub struct PendingPairings {
    by_code: BTreeMap<PairingCode, Pending>,
    ttl: u64,
    cap: usize,
}

impl Default for PendingPairings {
    fn default() -> Self {
        Self::new(PAIRING_TTL_SECS, MAX_PENDING)
    }
}

impl PendingPairings {
    /// A registry with codes that live `ttl` seconds, `cap` at a time.
    #[must_use]
    pub const fn new(ttl: u64, cap: usize) -> Self {
        Self {
            by_code: BTreeMap::new(),
            ttl,
            cap,
        }
    }

    /// The number of live codes at `now`.
    #[must_use]
    pub fn live(&self, now: u64) -> usize {
        self.by_code
            .values()
            .filter(|p| is_live(p.issued_at, now, self.ttl))
            .count()
    }

    /// The code for `pending`. A sender with a live code gets it again, so
    /// a repeated message does not make a second code. `fresh` makes a new
    /// code. It runs only when one is needed. `None` means too many codes
    /// are live.
    pub fn issue(
        &mut self,
        pending: Pending,
        fresh: impl FnMut() -> PairingCode,
    ) -> Option<PairingCode> {
        let now = pending.issued_at;
        let ttl = self.ttl;
        self.by_code.retain(|_, p| is_live(p.issued_at, now, ttl));
        let held = self
            .by_code
            .iter()
            .find(|(_, p)| p.channel == pending.channel && p.sender == pending.sender)
            .map(|(code, _)| code.clone());
        match issue_decision(held.is_some(), self.by_code.len(), self.cap) {
            Issue::Reuse => held,
            Issue::Full => None,
            Issue::New => {
                // A clash of 40 random bits is rare. Draw again until the code is free.
                let code =
                    std::iter::repeat_with(fresh).find(|code| !self.by_code.contains_key(code))?;
                self.by_code.insert(code.clone(), pending);
                Some(code)
            }
        }
    }

    /// Takes the stranger of `code` out of the registry. An expired or
    /// unknown code yields `None`, and an expired one is removed too.
    pub fn redeem(&mut self, code: &PairingCode, now: u64) -> Option<Pending> {
        let pending = self.by_code.remove(code)?;
        is_live(pending.issued_at, now, self.ttl).then_some(pending)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn pending(sender: &str, at: u64) -> Pending {
        Pending {
            channel: ChannelId::new("discord").unwrap(),
            sender: SenderId::new(sender).unwrap(),
            reply_to: String::from("dm-1"),
            message_id: MessageId::new("discord:1").unwrap(),
            issued_at: at,
        }
    }

    /// Codes from a counter, so each test knows them.
    fn counter() -> impl FnMut() -> PairingCode {
        let mut n = 0_u8;
        move || {
            n += 1;
            PairingCode::from_random([0, 0, 0, 0, n])
        }
    }

    #[test]
    fn a_code_reads_back_from_its_display_in_any_case() {
        let code = PairingCode::from_random([0xde, 0xad, 0xbe, 0xef, 0x42]);
        let shown = code.to_string();
        assert_eq!(shown.len(), 9);
        assert_eq!(shown.as_bytes()[4], b'-');
        assert_eq!(PairingCode::parse(&shown), Some(code.clone()));
        assert_eq!(PairingCode::parse(&shown.to_lowercase()), Some(code));
    }

    #[test]
    fn look_alike_letters_read_as_digits() {
        assert_eq!(
            PairingCode::parse("oil0-0000"),
            PairingCode::parse("0110 0000")
        );
        assert!(PairingCode::parse("0110-0000").is_some());
    }

    #[test]
    fn a_bad_code_is_refused() {
        assert_eq!(PairingCode::parse(""), None);
        assert_eq!(PairingCode::parse("1234-567"), None);
        assert_eq!(PairingCode::parse("1234-5678-9"), None);
        assert_eq!(PairingCode::parse("UUUU-UUUU"), None);
        assert_eq!(PairingCode::parse("12345678é"), None);
    }

    #[test]
    fn a_sender_gets_one_code_until_it_expires() {
        let mut codes = PendingPairings::new(60, 4);
        let mut fresh = counter();
        let first = codes.issue(pending("discord:7", 100), &mut fresh).unwrap();
        let again = codes.issue(pending("discord:7", 130), &mut fresh).unwrap();
        assert_eq!(first, again);
        assert_eq!(codes.live(130), 1);
        let later = codes.issue(pending("discord:7", 161), &mut fresh).unwrap();
        assert_ne!(first, later);
        assert_eq!(codes.live(161), 1);
    }

    #[test]
    fn a_full_registry_refuses_a_new_sender_but_not_a_known_one() {
        let mut codes = PendingPairings::new(60, 2);
        let mut fresh = counter();
        let a = codes.issue(pending("discord:1", 0), &mut fresh).unwrap();
        codes.issue(pending("discord:2", 0), &mut fresh).unwrap();
        assert_eq!(codes.issue(pending("discord:3", 1), &mut fresh), None);
        assert_eq!(codes.issue(pending("discord:1", 1), &mut fresh), Some(a));
        // Expired codes free their slots.
        assert!(codes.issue(pending("discord:3", 61), &mut fresh).is_some());
    }

    #[test]
    fn a_code_redeems_once_and_only_while_it_lives() {
        let mut codes = PendingPairings::new(60, 4);
        let mut fresh = counter();
        let code = codes.issue(pending("discord:7", 100), &mut fresh).unwrap();
        let paired = codes.redeem(&code, 159).unwrap();
        assert_eq!(paired.sender.as_str(), "discord:7");
        assert_eq!(codes.redeem(&code, 159), None);
        let late = codes.issue(pending("discord:8", 100), &mut fresh).unwrap();
        assert_eq!(codes.redeem(&late, 160), None);
        assert_eq!(codes.redeem(&late, 100), None, "an expired code is gone");
    }

    #[test]
    fn a_clashing_code_is_drawn_again() {
        let mut codes = PendingPairings::new(60, 4);
        let same = PairingCode::from_random([1, 2, 3, 4, 5]);
        let mut draws = vec![
            PairingCode::from_random([9, 9, 9, 9, 9]),
            same.clone(),
            same.clone(),
        ];
        let mut fresh = move || draws.pop().unwrap();
        assert_eq!(codes.issue(pending("discord:1", 0), &mut fresh), Some(same));
        let other = codes.issue(pending("discord:2", 0), &mut fresh).unwrap();
        assert_eq!(other, PairingCode::from_random([9, 9, 9, 9, 9]));
    }

    /// The Verus spec of `formal/verus/src/pairing.rs`, restated in Rust.
    fn spec_issue(has_live_code: bool, live_codes: usize, cap: usize) -> Issue {
        if has_live_code {
            Issue::Reuse
        } else if live_codes < cap {
            Issue::New
        } else {
            Issue::Full
        }
    }

    proptest! {
        #[test]
        fn issue_decision_matches_the_spec(has: bool, live in 0_usize..100, cap in 0_usize..100) {
            prop_assert_eq!(issue_decision(has, live, cap), spec_issue(has, live, cap));
        }

        #[test]
        fn is_live_matches_the_spec(at: u64, now: u64, ttl: u64) {
            let spec = now >= at && u128::from(now) < u128::from(at) + u128::from(ttl);
            prop_assert_eq!(is_live(at, now, ttl), spec);
        }

        #[test]
        fn the_registry_never_holds_more_than_its_cap(
            senders in proptest::collection::vec(0_u8..20, 1..80),
            cap in 1_usize..6,
        ) {
            let mut codes = PendingPairings::new(1_000, cap);
            let mut n = 0_u32;
            for (step, sender) in senders.iter().enumerate() {
                let at = u64::try_from(step).unwrap();
                let mut fresh = || {
                    n += 1;
                    let b = n.to_be_bytes();
                    PairingCode::from_random([0, b[0], b[1], b[2], b[3]])
                };
                let _ = codes.issue(pending(&format!("discord:{sender}"), at), &mut fresh);
                prop_assert!(codes.live(at) <= cap);
            }
        }

        #[test]
        fn every_random_code_parses_back(bytes: [u8; 5]) {
            let code = PairingCode::from_random(bytes);
            prop_assert_eq!(PairingCode::parse(&code.to_string()), Some(code));
        }
    }
}
