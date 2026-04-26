//! Exec runtime: render the parsed Exec mini-language into shell text and run
//! it through `/bin/sh`.
//!
//! The pipeline AST itself is produced by [`crate::parse::exec`]; this module
//! is concerned with turning typed request values into shell-safe words.
//!
//! Semantics:
//!
//! - A [`ValueRef`] is resolved against an [`ExecContext`] (query/path/header/
//!   var maps and a [`BodyValue`]).
//! - A `Text` token is always emitted as one argv element. Missing
//!   interpolations render as the empty string.
//! - A `[..]` `Group` token is used for shell words that contain interpolation.
//!   It is emitted only if every interpolation resolves; otherwise the whole
//!   group is omitted.
//! - Request values interpolated into shell text are single-quoted. Literal
//!   shell syntax written by the spec author remains literal shell syntax.
//! - A binary body used outside stdin is written to a temp file and the path is
//!   interpolated as a quoted shell word.
//! - Pipeline stages are wired stdin → stdout. A bare `Source` stage
//!   (`$ | cmd`) feeds a value as stdin to the next command.

use crate::spec::{ExecStage, ExecToken, TextPart, ValueRef};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

// ---------- Context ----------

#[derive(Clone, Debug, Default)]
pub enum BodyValue {
    #[default]
    None,
    Text(String),
    Json(serde_json::Value),
    Form(BTreeMap<String, String>),
    Binary(Bytes),
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
        if let ValueRef::Body { path } = r
            && path.is_empty()
            && let BodyValue::Binary(b) = &self.body
        {
            return Some(b.to_vec());
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
    match render_shell_with_mode(pipeline, ctx, false) {
        Ok(rendered) => vec![format!("shell: {}", rendered.script)],
        Err(err) => vec![format!("shell: <unresolved: {}>", err)],
    }
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
    let rendered = render_shell(pipeline, ctx)?;
    run_shell(rendered, timeout).await
}

struct RenderedShell {
    script: String,
    stdin: Option<Vec<u8>>,
    _temp_files: Vec<NamedTempFile>,
}

struct ShellRenderer<'a> {
    ctx: &'a ExecContext,
    temp_files: Vec<NamedTempFile>,
    materialize_binary: bool,
}

impl<'a> ShellRenderer<'a> {
    fn new(ctx: &'a ExecContext, materialize_binary: bool) -> Self {
        Self {
            ctx,
            temp_files: Vec::new(),
            materialize_binary,
        }
    }

    fn resolve_shell_text(&mut self, r: &ValueRef) -> Result<Option<String>, String> {
        if let ValueRef::Body { path } = r
            && path.is_empty()
            && let BodyValue::Binary(bytes) = &self.ctx.body
        {
            if !self.materialize_binary {
                return Ok(Some("<binary temp file>".into()));
            }
            let mut file = NamedTempFile::new().map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
            let path = file.path().to_string_lossy().to_string();
            self.temp_files.push(file);
            return Ok(Some(path));
        }
        Ok(self.ctx.resolve_text(r))
    }

    fn render_command(&mut self, tokens: &[ExecToken]) -> Result<String, String> {
        let mut words = Vec::new();
        for token in tokens {
            match token {
                ExecToken::Text {
                    parts, force_quote, ..
                } => {
                    words.push(self.render_text_word(parts, *force_quote, false)?);
                }
                ExecToken::Group { pieces, .. } => {
                    let mut group_words = Vec::with_capacity(pieces.len());
                    let mut all_present = true;
                    for piece in pieces {
                        match self.render_optional_word(&piece.parts, piece.force_quote)? {
                            Some(word) => group_words.push(word),
                            None => {
                                all_present = false;
                                break;
                            }
                        }
                    }
                    if all_present {
                        words.extend(group_words);
                    }
                }
            }
        }
        if words.is_empty() {
            return Err("command stage produced empty shell command".into());
        }
        Ok(words.join(" "))
    }

    fn render_text_word(
        &mut self,
        parts: &[TextPart],
        force_quote: bool,
        omit_missing: bool,
    ) -> Result<String, String> {
        let mut out = String::new();
        let mut has_interp = false;
        for part in parts {
            match part {
                TextPart::Literal(s) => out.push_str(s),
                TextPart::Interp(r) => {
                    has_interp = true;
                    match self.resolve_shell_text(r)? {
                        Some(value) => out.push_str(&value),
                        None if omit_missing => return Ok(String::new()),
                        None => {}
                    }
                }
            }
        }
        Ok(shell_word(&out, force_quote || has_interp))
    }

    fn render_optional_word(
        &mut self,
        parts: &[TextPart],
        force_quote: bool,
    ) -> Result<Option<String>, String> {
        let mut out = String::new();
        let mut has_interp = false;
        for part in parts {
            match part {
                TextPart::Literal(s) => out.push_str(s),
                TextPart::Interp(r) => {
                    has_interp = true;
                    let Some(value) = self.resolve_shell_text(r)? else {
                        return Ok(None);
                    };
                    out.push_str(&value);
                }
            }
        }
        Ok(Some(shell_word(&out, force_quote || has_interp)))
    }
}

fn render_shell(pipeline: &[ExecStage], ctx: &ExecContext) -> Result<RenderedShell, String> {
    render_shell_with_mode(pipeline, ctx, true)
}

fn render_shell_with_mode(
    pipeline: &[ExecStage],
    ctx: &ExecContext,
    materialize_binary: bool,
) -> Result<RenderedShell, String> {
    let mut pending_stdin: Option<Vec<u8>> = None;
    let mut commands = Vec::new();
    let mut saw_command = false;
    let mut renderer = ShellRenderer::new(ctx, materialize_binary);

    for stage in pipeline {
        match stage {
            ExecStage::Source { reference, .. } => {
                if saw_command {
                    return Err(
                        "value-reference source after a command stage is not supported".into(),
                    );
                }
                let bytes = ctx
                    .resolve_bytes(reference)
                    .ok_or_else(|| format!("unresolved {}", reference.describe()))?;
                pending_stdin = Some(bytes);
            }
            ExecStage::Command { tokens, .. } => {
                saw_command = true;
                commands.push(renderer.render_command(tokens)?);
            }
        }
    }
    if commands.is_empty() {
        return Err("pipeline ended without a command".to_string());
    }
    Ok(RenderedShell {
        script: commands.join(" | "),
        stdin: pending_stdin,
        _temp_files: renderer.temp_files,
    })
}

async fn run_shell(
    rendered: RenderedShell,
    timeout: Option<std::time::Duration>,
) -> Result<ExecOutput, String> {
    tracing::debug!(script = %rendered.script, "exec::run_shell: spawning");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&rendered.script);
    cmd.kill_on_drop(true);
    cmd.process_group(0);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if rendered.stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {}", e))?;
    let child_id = child.id();
    if let Some(stdin) = rendered.stdin
        && let Some(mut sin) = child.stdin.take()
    {
        sin.write_all(&stdin).await.map_err(|e| e.to_string())?;
        drop(sin);
    }

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

    let status = if let Some(timeout) = timeout {
        tokio::select! {
            status = child.wait() => status.map_err(|e| e.to_string())?,
            _ = tokio::time::sleep(timeout) => {
                if let Some(pid) = child_id {
                    kill_process_group(pid);
                }
                let _ = child.kill().await;
                return Err("execution timed out".into());
            }
        }
    } else {
        child.wait().await.map_err(|e| e.to_string())?
    };
    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    if let Some(pid) = child_id {
        kill_process_group(pid);
    }
    Ok(ExecOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pgid = -(pid as libc::pid_t);
    // The shell may have already exited; ESRCH is fine here. This is a best
    // effort cleanup for shell-spawned descendants after normal completion.
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn shell_word(value: &str, force_quote: bool) -> String {
    if force_quote || value.is_empty() || value.chars().any(char::is_whitespace) {
        shell_quote(value)
    } else {
        value.to_string()
    }
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}
