//! mii-http library root.
//!
//! Module layout follows the parsing-vs-execution split:
//!
//! - [`spec`] — pure AST types.
//! - [`parse`] — turns source text into AST (spec parser + Exec sub-parser).
//! - [`check`] — semantic validation on the AST.
//! - [`value`] — runtime validation of incoming values against type expressions.
//! - [`exec`] — runtime: argv assembly + pipeline execution (no shell).
//! - [`server`] — axum HTTP server gluing the pieces together.
//! - [`diag`] — diagnostic reporting via ariadne.

pub mod check;
pub mod diag;
pub mod exec;
pub mod parse;
pub mod server;
pub mod spec;
pub mod value;
