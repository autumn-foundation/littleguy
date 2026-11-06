//! Newtypes for every id that crosses a boundary.
//!
//! Each constructor checks its text once, so code that holds an id never
//! checks it again.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The longest id in bytes. It bounds what a client can make the daemon hold.
pub const MAX_ID_BYTES: usize = 128;

/// Why a text is not a valid id.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The text is empty.
    #[error("the {kind} is empty")]
    Empty {
        /// The id type.
        kind: &'static str,
    },
    /// The text is longer than [`MAX_ID_BYTES`].
    #[error("the {kind} is longer than {MAX_ID_BYTES} bytes")]
    TooLong {
        /// The id type.
        kind: &'static str,
    },
    /// The text holds a byte that is not printable ASCII.
    #[error("the {kind} holds a byte that is not printable ASCII")]
    NotPrintable {
        /// The id type.
        kind: &'static str,
    },
}

/// Checks the shape that every id shares: printable ASCII, not empty, bounded.
fn check(kind: &'static str, text: &str) -> Result<(), IdError> {
    if text.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if text.len() > MAX_ID_BYTES {
        return Err(IdError::TooLong { kind });
    }
    if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(IdError::NotPrintable { kind });
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Checks `text` and wraps it.
            ///
            /// # Errors
            ///
            /// Returns an error when `text` is empty, longer than
            /// [`MAX_ID_BYTES`], or not printable ASCII.
            pub fn new(text: impl Into<String>) -> Result<Self, IdError> {
                let text = text.into();
                check($kind, &text)?;
                Ok(Self(text))
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::new(text)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// A channel, for example `cli` or `discord`.
    ChannelId,
    "channel id"
);
id_type!(
    /// A sender as the channel names it, for example `uid:1000`.
    SenderId,
    "sender id"
);
id_type!(
    /// The id of one inbound message. It is the idempotency key of its op.
    MessageId,
    "message id"
);
id_type!(
    /// The owner of the daemon. It names the agent `assistant/<owner>`.
    OwnerName,
    "owner name"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_printable_id_is_accepted() {
        let id = MessageId::new("m-1").unwrap();
        assert_eq!(id.as_str(), "m-1");
        assert_eq!(id.to_string(), "m-1");
    }

    #[test]
    fn an_empty_long_or_unprintable_id_is_refused() {
        assert_eq!(SenderId::new(""), Err(IdError::Empty { kind: "sender id" }));
        assert_eq!(
            SenderId::new("x".repeat(MAX_ID_BYTES + 1)),
            Err(IdError::TooLong { kind: "sender id" })
        );
        assert_eq!(
            SenderId::new("a b"),
            Err(IdError::NotPrintable { kind: "sender id" })
        );
        assert!(SenderId::new("x".repeat(MAX_ID_BYTES)).is_ok());
    }

    #[test]
    fn an_id_converts_to_and_from_a_string() {
        let id = ChannelId::try_from(String::from("cli")).unwrap();
        assert_eq!(String::from(id), "cli");
        assert!(OwnerName::try_from(String::from("a\nb")).is_err());
    }
}
