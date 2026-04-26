//! Parser for the Exec mini-language (the value of an `Exec:` directive).
//!
//! Grammar (informal):
//!
//!   exec      := pipeline
//!   pipeline  := stage ("|" stage)*
//!   stage     := source | command
//!   source    := value_ref                       (a single bare ref by itself)
//!   command   := token (ws+ token)*
//!   token     := group | text
//!   group     := "[" piece (ws+ piece)* "]"
//!   piece     := text-without-spaces             (with `{...}` interpolations)
//!   text      := (literal | "{" value_ref "}" | quoted_str)+
//!   value_ref := "%" ident | ":" ident | "^" ident | "@" ident
//!              | "$" | "$." ident ("." ident)*
//!
//! This module is purely syntactic: it produces an `ExecStage` AST. Argv
//! construction and process spawning live in [`crate::exec`].

use crate::diag::Diag;
use crate::spec::{ExecStage, ExecToken, GroupPiece, TextPart, ValueRef};
use chumsky::error::Rich;
use chumsky::prelude::*;

type Extra<'a> = extra::Err<Rich<'a, char>>;

/// Parse the Exec value. `start` is the absolute byte offset of the first
/// character in the source, used to translate spans for diagnostics.
pub fn parse_exec(raw: &str, start: usize) -> Result<Vec<ExecStage>, Diag> {
    let result = pipeline_parser().parse(raw).into_result();
    match result {
        Ok(stages) => Ok(stages
            .into_iter()
            .map(|s| shift_stage(s, start))
            .collect()),
        Err(errs) => {
            let e = errs
                .into_iter()
                .next()
                .expect("chumsky returns >=1 err on failure");
            let span = e.span();
            Err(Diag::error(
                format!("invalid Exec: {}", e),
                (start + span.start)..(start + span.end),
                "syntax error",
            ))
        }
    }
}

// ---------- chumsky grammar ----------

fn ident_parser<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> + Clone {
    any()
        .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .repeated()
        .at_least(1)
        .collect::<String>()
}

fn value_ref_parser<'a>() -> impl Parser<'a, &'a str, ValueRef, Extra<'a>> + Clone {
    let dotted_ident = any()
        .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .repeated()
        .at_least(1)
        .collect::<String>();
    let body_path = just('.')
        .ignore_then(
            dotted_ident
                .clone()
                .separated_by(just('.'))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .or_not();
    let body = just('$')
        .ignore_then(body_path)
        .map(|p| ValueRef::Body {
            path: p.unwrap_or_default(),
        });

    let sigil_ref = choice((
        just('%').ignore_then(ident_parser()).map(ValueRef::Query),
        just(':').ignore_then(ident_parser()).map(ValueRef::Path),
        just('^').ignore_then(ident_parser()).map(ValueRef::Header),
        just('@').ignore_then(ident_parser()).map(ValueRef::Var),
    ));

    choice((body, sigil_ref))
}

fn interp_parser<'a>() -> impl Parser<'a, &'a str, ValueRef, Extra<'a>> + Clone {
    just('{')
        .ignore_then(value_ref_parser().padded_by(one_of(" \t").repeated()))
        .then_ignore(just('}'))
}

/// Bare value reference (sigil-prefixed, no braces). Used inside `[...]` groups
/// and at the stage top-level (where it becomes a Source if it's the only
/// content in the stage).
fn bare_ref_parser<'a>() -> impl Parser<'a, &'a str, ValueRef, Extra<'a>> + Clone {
    value_ref_parser()
}

fn quoted_str<'a>(quote: char) -> impl Parser<'a, &'a str, String, Extra<'a>> + Clone {
    let escape = just('\\').ignore_then(any().map(|c: char| c));
    let normal = any().filter(move |c: &char| *c != quote && *c != '\\');
    just(quote)
        .ignore_then(choice((escape, normal)).repeated().collect::<String>())
        .then_ignore(just(quote))
}

/// A "text token" is a sequence of literal chunks, interpolations and quoted
/// strings, terminated by whitespace or a special char (`[`, `|`, `]`).
fn text_token_parser<'a>() -> impl Parser<'a, &'a str, Vec<TextPart>, Extra<'a>> + Clone {
    let interp = interp_parser().map(TextPart::Interp);
    let quoted = choice((quoted_str('"'), quoted_str('\''))).map(TextPart::Literal);
    let bare = any()
        .filter(|c: &char| {
            !c.is_whitespace() && *c != '|' && *c != '[' && *c != ']' && *c != '{' && *c != '"' && *c != '\''
        })
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(TextPart::Literal);
    choice((interp, quoted, bare))
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .map(merge_literals)
}

/// Inside a `[...]` group: pieces are whitespace-separated. A piece may be a
/// bare value ref (e.g. `%name`, `:user_id`, `$.user.name`), a quoted string,
/// or a literal mixed with `{...}` interps.
fn group_piece_parser<'a>() -> impl Parser<'a, &'a str, GroupPiece, Extra<'a>> + Clone {
    let interp = interp_parser().map(TextPart::Interp);
    let bare_ref = bare_ref_parser().map(TextPart::Interp);
    let quoted = choice((quoted_str('"'), quoted_str('\''))).map(TextPart::Literal);
    let bare = any()
        .filter(|c: &char| {
            !c.is_whitespace()
                && *c != '|'
                && *c != '['
                && *c != ']'
                && *c != '{'
                && *c != '}'
                && *c != '"'
                && *c != '\''
                && *c != '%'
                && *c != ':'
                && *c != '^'
                && *c != '@'
                && *c != '$'
        })
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(TextPart::Literal);
    choice((interp, bare_ref, quoted, bare))
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|parts| GroupPiece {
            parts: merge_literals(parts),
        })
}

fn merge_literals(parts: Vec<TextPart>) -> Vec<TextPart> {
    let mut out: Vec<TextPart> = Vec::with_capacity(parts.len());
    for p in parts {
        match (p, out.last_mut()) {
            (TextPart::Literal(s), Some(TextPart::Literal(prev))) => {
                prev.push_str(&s);
            }
            (p, _) => out.push(p),
        }
    }
    out
}

fn hws<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    one_of(" \t").repeated().ignored()
}

fn group_parser<'a>() -> impl Parser<'a, &'a str, ExecToken, Extra<'a>> + Clone {
    just('[')
        .ignore_then(hws())
        .ignore_then(
            group_piece_parser()
                .separated_by(hws().then(empty()))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then_ignore(hws())
        .then_ignore(just(']'))
        .map_with(|pieces, e| {
            let span: SimpleSpan = e.span();
            ExecToken::Group {
                pieces,
                span: span.start..span.end,
            }
        })
}

fn token_parser<'a>() -> impl Parser<'a, &'a str, ExecToken, Extra<'a>> + Clone {
    choice((
        group_parser(),
        text_token_parser().map_with(|parts, e| {
            let span: SimpleSpan = e.span();
            ExecToken::Text {
                parts,
                span: span.start..span.end,
            }
        }),
    ))
}

fn stage_parser<'a>() -> impl Parser<'a, &'a str, ExecStage, Extra<'a>> + Clone {
    // Try a bare value-ref-only stage first (Source). Then fall back to a
    // command stage. The Source path requires the ref to be alone (only ws
    // before the next `|` or end).
    let source_only = hws()
        .ignore_then(bare_ref_parser())
        .then_ignore(hws())
        .then_ignore(choice((just('|').rewind().ignored(), end())))
        .map_with(|reference, e| {
            let span: SimpleSpan = e.span();
            ExecStage::Source {
                reference,
                span: span.start..span.end,
            }
        });

    let command = hws().ignore_then(
        token_parser()
            .separated_by(hws().then(empty()).then(hws()))
            .at_least(1)
            .collect::<Vec<_>>()
            .then_ignore(hws())
            .map_with(|tokens: Vec<ExecToken>, e| {
                let span: SimpleSpan = e.span();
                ExecStage::Command {
                    tokens,
                    span: span.start..span.end,
                }
            }),
    );

    choice((source_only, command))
}

fn pipeline_parser<'a>() -> impl Parser<'a, &'a str, Vec<ExecStage>, Extra<'a>> + Clone {
    stage_parser()
        .separated_by(just('|'))
        .at_least(1)
        .collect::<Vec<_>>()
        .then_ignore(hws())
        .then_ignore(end())
}

// ---------- span shifting ----------

fn shift_stage(s: ExecStage, base: usize) -> ExecStage {
    match s {
        ExecStage::Source { reference, span } => ExecStage::Source {
            reference,
            span: (span.start + base)..(span.end + base),
        },
        ExecStage::Command { tokens, span } => ExecStage::Command {
            tokens: tokens.into_iter().map(|t| shift_token(t, base)).collect(),
            span: (span.start + base)..(span.end + base),
        },
    }
}

fn shift_token(t: ExecToken, base: usize) -> ExecToken {
    match t {
        ExecToken::Text { parts, span } => ExecToken::Text {
            parts,
            span: (span.start + base)..(span.end + base),
        },
        ExecToken::Group { pieces, span } => ExecToken::Group {
            pieces,
            span: (span.start + base)..(span.end + base),
        },
    }
}
