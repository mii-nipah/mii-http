//! Exec runtime: build argv from a parsed pipeline and execute it as a chain
//! of child processes. **No shell is ever invoked.**
//!
//! The pipeline AST itself is produced by [`crate::parse::exec`]; this module
//! is concerned only with executing it safely.
//!
//! Semantics:
//!
//! - A [`ValueRef`] is resolved against an [`ExecContext`] (query/path/header/
//!   var maps and a [`BodyValue`]).
//! - A `Text` token is always emitted as one argv element. Missing
//!   interpolations render as the empty string.
//! - A `[..]` `Group` token is emitted only if every required interpolation
//!   resolves; otherwise the whole group is omitted from argv (this is how
//!   optional flags work).
//! - Pipeline stages are wired stdin → stdout. A bare `Source` stage
//!   (`$ | cmd`) feeds a value as stdin to the next command.

use crate::spec::{ExecStage, ExecToken, TextPart, ValueRef};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

// ---------- Context ----------

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

// ---------- argv assembly ----------

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
    tracing::debug!(tokens = tokens.len(), "exec::build_argv");
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

// ---------- pipeline execution ----------

#[derive(Debug)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Render a human-readable, non-executing preview of `pipeline` against `ctx`.
/// Used by `--dry-run` to log the commands that would have run.
pub fn preview_pipeline(pipeline: &[ExecStage], ctx: &ExecContext) -> Vec<String> {
    tracing::debug!(stages = pipeline.len(), "exec::preview_pipeline");
    let mut out = Vec::with_capacity(pipeline.len());
    for stage in pipeline {
        match stage {
            ExecStage::Source { reference, .. } => {
                let resolved = ctx
                    .resolve_text(reference)
                    .map(|s| {
                        if s.len() > 200 {
                            format!("{}…", &s[..200])
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| "<unresolved>".into());
                out.push(format!(
                    "stdin <- {} = {:?}",
                    reference.describe(),
                    resolved
                ));
            }
            ExecStage::Command { tokens, .. } => {
                let argv = build_argv(tokens, ctx);
                out.push(format!("argv: {:?}", argv));
            }
        }
    }
    out
}

pub async fn run_pipeline(
    pipeline: &[ExecStage],
    ctx: &ExecContext,
    timeout: Option<std::time::Duration>,
) -> Result<ExecOutput, String> {
    tracing::debug!(stages = pipeline.len(), ?timeout, "exec::run_pipeline");
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
                tracing::debug!(program = %program, args = ?args, "exec::run_pipeline: spawning");
                let mut cmd = Command::new(&program);
                cmd.args(args);
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                let stdin_data: Option<Vec<u8>>;
                if let Some(mut prev) = prev_child.take() {
                    if let Some(out) = prev.stdout.take() {
                        let std_out: std::process::Stdio =
                            out.try_into().map_err(|e: std::io::Error| e.to_string())?;
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
