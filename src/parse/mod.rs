//! Parsing layer for `.http` specs files.
//!
//! - [`spec`] parses the line-oriented outer grammar (setup directives,
//!   endpoint blocks, body schemas, etc.) into a [`crate::spec::Spec`] AST.
//! - [`exec`] parses the Exec mini-language used inside `Exec:` directives
//!   into a sequence of [`crate::spec::ExecStage`].
//!
//! No execution happens here: this module only turns source text into AST.

pub mod spec;
pub mod exec;

pub use spec::{parse, ParseResult};
