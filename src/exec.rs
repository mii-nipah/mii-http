//! Exec mini-language: chumsky-based parser + safe runtime.
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
//! No shell is ever invoked; argv is built directly. `[..]` groups are
//! conditional: if any required interpolation is missing, the whole group is
//! omitted from argv.

use crate::diag::Diag;
use crate::spec::{ExecStage, ExecToken, GroupPiece, TextPart, ValueRef};
use bytes::Bytes;
use chumsky::error::Rich;
use chumsky::prelude::*;
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

type Extra<'a> = extra::Err<Rich<'a, char>>;

/// Parse the Exec value (the part after `Exec:`). `start` is the absolute byte
/// offset of the first character in the source, used to translate spans for
/// diagnostics.
pub fn parse_exec(raw: &str, start: usize) -> Result<Vec<ExecStage>, Diag> {
    let result = pipeline_parser().parse(raw).into_result();
    match result {
        Ok(stages) => Ok(stages
            .into_iter()
            .map(|s| shift_stage(s, start))
            .collect()),
        Err(errs) => {
            // pick the first error for a single Diag (parser already returns one)
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

// ---------- Runtime ----------

#[derive(Clone, Debug)]
pub enum BodyValue {
    None,
    Text(String),
    Json(serde_json::Value),
    Form(BTreeMap<String, String>),
    Binary(Bytes),
}

impl Default for BodyValue {
    fn default() -> Self {
        BodyValue::None
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecContext {
    pub query: BTreeMap<String, String>,
    pub path: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub vars: BTreeMap<String, String>,
    pub body: BodyValue,
}

impl ExecContext {
    fn resolve_text(&self, r: &ValueRef) -> Option<String> {
        match r {
            ValueRef::Query(n) => self.query.get(n).cloned(),
            ValueRef::Path(n) => self.path.get(n).cloned(),
            ValueRef::Header(n) => self.headers.get(n).cloned(),
            ValueRef::Var(n) => self.vars.get(n).cloned(),
            ValueRef::Body { path } => match &self.body {
                BodyValue::None => None,
                BodyValue::Text(s) if path.is_empty() => Some(s.clone()),
                BodyValue::Text(_) => None,
                BodyValue::Json(v) => {
                    let mut cur = v;
                    for p in path {
                        cur = cur.get(p)?;
                    }
                    Some(json_to_text(cur))
                }
                BodyValue::Form(m) => {
                    if path.is_empty() {
                        Some(form_to_text(m))
                    } else if path.len() == 1 {
                        m.get(&path[0]).cloned()
                    } else {
                        None
                    }
                }
                BodyValue::Binary(_) => None,
            },
        }
    }

    fn resolve_bytes(&self, r: &ValueRef) -> Option<Vec<u8>> {
        if let ValueRef::Body { path } = r {
            if path.is_empty() {
                if let BodyValue::Binary(b) = &self.body {
                    return Some(b.to_vec());
                }
            }
        }
        self.resolve_text(r).map(|s| s.into_bytes())
    }
}

fn json_to_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn form_to_text(m: &BTreeMap<String, String>) -> String {
    let pairs: Vec<String> = m.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    pairs.join("&")
}

fn render_text_parts(parts: &[TextPart], ctx: &ExecContext) -> (String, bool) {
    let mut out = String::new();
    let mut all_present = true;
    for p in parts {
        match p {
            TextPart::Literal(s) => out.push_str(s),
            TextPart::Interp(r) => match ctx.resolve_text(r) {
                Some(s) => out.push_str(&s),
                None => all_present = false,
            },
        }
    }
    (out, all_present)
}

pub fn build_argv(tokens: &[ExecToken], ctx: &ExecContext) -> Vec<String> {
    let mut argv = Vec::new();
    for t in tokens {
        match t {
            ExecToken::Text { parts, .. } => {
                let (s, _) = render_text_parts(parts, ctx);
                argv.push(s);
            }
            ExecToken::Group { pieces, .. } => {
                let mut piece_strs = Vec::with_capacity(pieces.len());
                let mut all_present = true;
                for piece in pieces {
                    let (s, present) = render_text_parts(&piece.parts, ctx);
                    if !present {
                        all_present = false;
                        break;
                    }
                    piece_strs.push(s);
                }
                if all_present {
                    argv.extend(piece_strs);
                }
            }
        }
    }
    argv
}

#[derive(Debug)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub async fn run_pipeline(
    pipeline: &[ExecStage],
    ctx: &ExecContext,
    timeout: Option<std::time::Duration>,
) -> Result<ExecOutput, String> {
    if pipeline.is_empty() {
        return Err("empty exec pipeline".into());
    }
    let fut = run_pipeline_inner(pipeline, ctx);
    if let Some(t) = timeout {
        match tokio::time::timeout(t, fut).await {
            Ok(r) => r,
            Err(_) => Err("execution timed out".into()),
        }
    } else {
        fut.await
    }
}

async fn run_pipeline_inner(
    pipeline: &[ExecStage],
    ctx: &ExecContext,
) -> Result<ExecOutput, String> {
    let mut pending_stdin: Option<Vec<u8>> = None;
    let mut prev_child: Option<tokio::process::Child> = None;

    let mut i = 0;
    while i < pipeline.len() {
        match &pipeline[i] {
            ExecStage::Source { reference, .. } => {
                if prev_child.is_some() {
                    return Err(
                        "value-reference source after a command stage is not supported".into(),
                    );
                }
                let bytes = ctx
                    .resolve_bytes(reference)
                    .ok_or_else(|| format!("unresolved {}", reference.describe()))?;
                pending_stdin = Some(bytes);
                i += 1;
            }
            ExecStage::Command { tokens, .. } => {
                let argv = build_argv(tokens, ctx);
                if argv.is_empty() {
                    return Err("command stage produced empty argv".into());
                }
                let program = argv[0].clone();
                let args = &argv[1..];
                let mut cmd = Command::new(&program);
                cmd.args(args);
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                let stdin_data: Option<Vec<u8>>;
                if let Some(mut prev) = prev_child.take() {
                    if let Some(out) = prev.stdout.take() {
                        let std_out: std::process::Stdio = out
                            .try_into()
                            .map_err(|e: std::io::Error| e.to_string())?;
                        cmd.stdin(std_out);
                    } else {
                        cmd.stdin(Stdio::null());
                    }
                    tokio::spawn(async move {
                        let _ = prev.wait().await;
                    });
                    stdin_data = None;
                } else if let Some(d) = pending_stdin.take() {
                    cmd.stdin(Stdio::piped());
                    stdin_data = Some(d);
                } else {
                    cmd.stdin(Stdio::null());
                    stdin_data = None;
                }

                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("failed to spawn `{}`: {}", program, e))?;
                if let Some(d) = stdin_data {
                    if let Some(mut sin) = child.stdin.take() {
                        sin.write_all(&d).await.map_err(|e| e.to_string())?;
                        drop(sin);
                    }
                }
                prev_child = Some(child);
                i += 1;
            }
        }
    }

    let mut child = prev_child.ok_or_else(|| "pipeline ended without a command".to_string())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            tokio::io::AsyncReadExt::read_to_end(&mut s, &mut buf)
                .await
                .ok();
        }
        buf
    });
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            tokio::io::AsyncReadExt::read_to_end(&mut s, &mut buf)
                .await
                .ok();
        }
        buf
    });
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    Ok(ExecOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}
