//! The raw RFC 5322 message that Gmail sends, and the text of a stored one.
//!
//! The model writes the address, the subject and the body. A new line in
//! an address or a subject would add a header, so each one is refused.
//! The body travels as base64, so no line of it can end the headers.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::Value;

/// The most addresses of one mail.
pub const MAX_RECIPIENTS: usize = 20;

/// The headers that tie a reply to the mail it answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// The `Message-ID` of the mail it answers.
    pub message_id: String,
    /// The `References` of that mail, with its `Message-ID` added.
    pub references: String,
}

/// One mail for Gmail to draft or send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    /// The addresses, as the model wrote them.
    pub to: Vec<String>,
    /// The subject.
    pub subject: String,
    /// The plain text body.
    pub body: String,
}

/// Whether `text` holds a character that can end a header line.
fn breaks_header(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// Whether `address` is a plain address, or a name and an address in
/// angle brackets.
fn plain_address(address: &str) -> bool {
    let inner = match (address.find('<'), address.strip_suffix('>')) {
        (Some(open), Some(rest)) => &rest[open + 1..],
        (None, None) => address,
        _ => return false,
    };
    let mut parts = inner.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    parts.next().is_none()
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !inner.contains(char::is_whitespace)
        && !inner.contains(['<', '>', ',', ';'])
}

impl Mail {
    /// A checked mail. `to` holds one or more addresses split by commas.
    ///
    /// # Errors
    ///
    /// Returns the reason, for the model, when an address is not plain, a
    /// header holds a control character, or there are no or too many
    /// addresses.
    pub fn new(to: &str, subject: &str, body: &str) -> Result<Self, String> {
        if breaks_header(to) || breaks_header(subject) {
            return Err(String::from(
                "an address or the subject holds a line break or a control character",
            ));
        }
        let to: Vec<String> = to
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect();
        if to.is_empty() || to.len() > MAX_RECIPIENTS {
            return Err(format!("send to 1 to {MAX_RECIPIENTS} addresses"));
        }
        if let Some(bad) = to.iter().find(|a| !plain_address(a)) {
            return Err(format!("`{bad}` is not an email address"));
        }
        Ok(Self {
            to,
            subject: subject.to_owned(),
            body: body.to_owned(),
        })
    }

    /// The raw message, in the base64url form that Gmail takes.
    #[must_use]
    pub fn raw(&self, reply: Option<&Reply>) -> String {
        let mut text = format!(
            "To: {}\r\nSubject: {}\r\nMIME-Version: 1.0\r\n",
            self.to.join(", "),
            encode_word(&self.subject)
        );
        if let Some(reply) = reply {
            let _ = write!(
                text,
                "In-Reply-To: {}\r\nReferences: {}\r\n",
                reply.message_id, reply.references
            );
        }
        text.push_str(
            "Content-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        );
        let body = STANDARD.encode(self.body.as_bytes());
        for line in body.as_bytes().chunks(76) {
            text.push_str(&String::from_utf8_lossy(line));
            text.push_str("\r\n");
        }
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }
}

/// `text` as an RFC 2047 encoded word when it is not plain ASCII.
#[must_use]
pub fn encode_word(text: &str) -> String {
    if text.is_ascii() {
        text.to_owned()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(text.as_bytes()))
    }
}

/// The reply headers of a stored mail, from its `Message-ID` and
/// `References`. `None` when it has no `Message-ID`.
#[must_use]
pub fn reply_to(message_id: Option<&str>, references: Option<&str>) -> Option<Reply> {
    let id = message_id.map(str::trim).filter(|id| !id.is_empty())?;
    if breaks_header(id) {
        return None;
    }
    let references = references
        .map(str::trim)
        .filter(|r| !r.is_empty() && !breaks_header(r))
        .map_or_else(|| id.to_owned(), |r| format!("{r} {id}"));
    Some(Reply {
        message_id: id.to_owned(),
        references,
    })
}

/// The data of one body part, decoded from base64url.
fn part_text(part: &Value) -> Option<String> {
    let data = part.pointer("/body/data")?.as_str()?;
    let bytes = URL_SAFE_NO_PAD
        .decode(data.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(data))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The first part of `payload` with `mime`, searched depth first.
fn find_part<'a>(payload: &'a Value, mime: &str) -> Option<&'a Value> {
    if payload["mimeType"].as_str() == Some(mime) && payload.pointer("/body/data").is_some() {
        return Some(payload);
    }
    payload["parts"]
        .as_array()?
        .iter()
        .find_map(|part| find_part(part, mime))
}

/// The readable text of a Gmail message payload: the plain part, or the
/// HTML part as text.
#[must_use]
pub fn body_text(payload: &Value) -> String {
    if let Some(text) = find_part(payload, "text/plain").and_then(part_text) {
        return text;
    }
    find_part(payload, "text/html")
        .and_then(part_text)
        .map(|html| crate::html::to_text(&html))
        .unwrap_or_default()
}

/// The value of header `name` in a Gmail message payload.
#[must_use]
pub fn header<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload["headers"]
        .as_array()?
        .iter()
        .find(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h["value"].as_str())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn decoded(raw: &str) -> String {
        String::from_utf8(URL_SAFE_NO_PAD.decode(raw).unwrap()).unwrap()
    }

    #[test]
    fn a_mail_has_its_headers_and_a_base64_body() {
        let mail = Mail::new("a@x.org, Bo <b@y.org>", "Hi", "line one\nline two").unwrap();
        let text = decoded(&mail.raw(None));
        assert!(text.starts_with("To: a@x.org, Bo <b@y.org>\r\nSubject: Hi\r\n"));
        assert!(!text.contains("In-Reply-To"));
        let body = text.split("\r\n\r\n").nth(1).unwrap().replace("\r\n", "");
        assert_eq!(STANDARD.decode(body).unwrap(), b"line one\nline two");
    }

    #[test]
    fn a_reply_carries_its_thread_headers() {
        let reply = reply_to(Some("<m1@x>"), Some("<m0@x>")).unwrap();
        assert_eq!(reply.references, "<m0@x> <m1@x>");
        assert_eq!(reply_to(Some("<m1@x>"), None).unwrap().references, "<m1@x>");
        assert_eq!(reply_to(None, Some("<m0@x>")), None);
        assert_eq!(reply_to(Some("<m1@x>\r\nBcc: e@v.il"), None), None);
        let mail = Mail::new("a@x.org", "Re: hi", "ok").unwrap();
        let text = decoded(&mail.raw(Some(&reply)));
        assert!(text.contains("In-Reply-To: <m1@x>\r\nReferences: <m0@x> <m1@x>\r\n"));
    }

    #[test]
    fn a_subject_outside_ascii_is_an_encoded_word() {
        assert_eq!(encode_word("plain"), "plain");
        assert_eq!(encode_word("héllo"), "=?UTF-8?B?aMOpbGxv?=");
    }

    #[test]
    fn bad_mails_are_refused() {
        assert!(Mail::new("a@x.org\r\nBcc: e@v.il", "s", "b").is_err());
        assert!(Mail::new("a@x.org", "s\nBcc: e@v.il", "b").is_err());
        assert!(Mail::new("", "s", "b").is_err());
        assert!(Mail::new("not-an-address", "s", "b").is_err());
        assert!(Mail::new("a@b@c.org", "s", "b").is_err());
        assert!(Mail::new("a@localhost", "s", "b").is_err());
        assert!(Mail::new("Bo <b@y.org", "s", "b").is_err());
        assert!(Mail::new("a b@x.org", "s", "b").is_err());
        let many = vec!["a@x.org"; MAX_RECIPIENTS + 1].join(",");
        assert!(Mail::new(&many, "s", "b").is_err());
    }

    #[test]
    fn the_plain_part_wins_and_html_is_read_as_text() {
        let encode = |t: &str| URL_SAFE_NO_PAD.encode(t);
        let both = json!({"mimeType": "multipart/alternative", "parts": [
            {"mimeType": "text/html", "body": {"data": encode("<p>html</p>")}},
            {"mimeType": "text/plain", "body": {"data": encode("plain")}},
        ]});
        assert_eq!(body_text(&both), "plain");
        let html = json!({"mimeType": "multipart/mixed", "parts": [
            {"mimeType": "multipart/alternative", "parts": [
                {"mimeType": "text/html", "body": {"data": encode("<p>only <b>html</b></p>")}},
            ]},
        ]});
        assert_eq!(body_text(&html), "only html");
        let padded = json!({"mimeType": "text/plain", "body": {"data": URL_SAFE.encode("a")}});
        assert_eq!(body_text(&padded), "a");
        assert_eq!(body_text(&json!({"mimeType": "image/png"})), "");
        let headers = json!({"headers": [{"name": "subject", "value": "Hi"}]});
        assert_eq!(header(&headers, "Subject"), Some("Hi"));
        assert_eq!(header(&headers, "From"), None);
    }

    proptest! {
        /// No subject or address that passes the check adds a header line.
        #[test]
        fn no_accepted_input_adds_a_header(to in ".{0,40}", subject in ".{0,40}") {
            if let Ok(mail) = Mail::new(&format!("a@x.org,{to}"), &subject, "body") {
                let text = decoded(&mail.raw(None));
                let head = text.split("\r\n\r\n").next().unwrap_or_default();
                prop_assert_eq!(head.lines().count(), 5);
            }
        }
    }
}
