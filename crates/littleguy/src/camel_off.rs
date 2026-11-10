//! The camel channel of a build without the `camel` feature.
//!
//! [`Camel`] has no value here, so `lg serve` keeps one signature and the
//! compiler drops each branch that would need a camel route. A config
//! with `[[camel.route]]` fails at start ([`crate::features::check`]).

/// A running camel context. This build has none.
#[derive(Debug)]
pub enum Camel {}
