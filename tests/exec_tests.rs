//! Tests for the chumsky-based Exec parser and `build_argv` semantics
//! (group omission, interpolation).

use bytes::Bytes;
use mii_http::exec::{BodyValue, ExecContext, FormFieldValue, build_argv, run_pipeline};
use mii_http::spec::{ExecStage, ExecToken, TextPart, ValueRef};
use std::collections::BTreeMap;
use std::time::Duration;

fn parse(src: &str) -> Vec<ExecStage> {
    mii_http::parse::exec::parse_exec(src, 0).expect("expected exec to parse")
}

fn parse_err(src: &str) {
    let r = mii_http::parse::exec::parse_exec(src, 0);
    assert!(r.is_err(), "expected parse error for {:?}", src);
}

fn cmd_tokens(stage: &ExecStage) -> &[ExecToken] {
    match stage {
        ExecStage::Command { tokens, .. } => tokens,
        _ => panic!("expected command stage"),
    }
}

#[test]
fn parses_simple_command() {
    let p = parse("echo hello");
    assert_eq!(p.len(), 1);
    let toks = cmd_tokens(&p[0]);
    assert_eq!(toks.len(), 2);
}

#[test]
fn parses_pipeline_with_source() {
    let p = parse("$ | xargs echo");
    assert_eq!(p.len(), 2);
    assert!(matches!(
        p[0],
        ExecStage::Source {
            reference: ValueRef::Body { ref path },
            ..
        } if path.is_empty()
    ));
    assert!(matches!(p[1], ExecStage::Command { .. }));
}

#[test]
fn parses_body_path_reference() {
    let p = parse("echo $.user.name");
    let toks = cmd_tokens(&p[0]);
    assert_eq!(toks.len(), 2);
    // The second token should contain a body-path interp
    // (parsed as a bare-ref text token)
    let _ = toks; // detailed AST shape is implementation-specific
}

#[test]
fn parses_all_value_ref_sigils() {
    let p = parse("echo [%q] [:p] [^h] [@v] [$.b]");
    let toks = cmd_tokens(&p[0]);
    // echo + 5 groups
    assert_eq!(toks.len(), 6);
    for tok in &toks[1..] {
        assert!(matches!(tok, ExecToken::Group { .. }));
    }
}

#[test]
fn parses_brace_interpolations() {
    let p = parse(r#"echo "{%name}-{:id}""#);
    let toks = cmd_tokens(&p[0]);
    assert_eq!(toks.len(), 2);
    match &toks[1] {
        ExecToken::Text {
            parts, force_quote, ..
        } => {
            assert!(*force_quote);
            assert!(parts.iter().any(|p| matches!(p, TextPart::Interp(_))));
        }
        _ => panic!("expected text token"),
    }
}

#[test]
fn rejects_brace_interpolation_outside_strings() {
    parse_err("echo {%name}");
    parse_err("echo a{%name}");
    parse_err("echo [{%name}]");
}

#[test]
fn parses_quoted_strings() {
    let p = parse(r#"echo "Hello, world""#);
    let toks = cmd_tokens(&p[0]);
    assert_eq!(toks.len(), 2);
}

#[test]
fn rejects_unclosed_group() {
    parse_err("echo [unclosed");
}

#[test]
fn rejects_empty_input() {
    parse_err("");
}

#[test]
fn build_argv_interpolates_present_values() {
    let p = parse(r#"echo "Hello, {%name}!""#);
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "World".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Hello, World!"]);
}

#[test]
fn build_argv_interpolates_required_shell_piece_groups() {
    let p = parse("echo [X=^X-Custom] [--name %name]");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.headers.insert("X-Custom".into(), "abc".into());
    ctx.query.insert("name".into(), "World".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "X=abc", "--name", "World"]);
}

#[test]
fn build_argv_omits_shell_piece_group_when_optional_value_missing() {
    let p = parse("echo Hello, [%name] [%guest]");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "World".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Hello,", "World"]);
}

#[test]
fn build_argv_renders_text_interp_as_empty_when_missing() {
    let p = parse(r#"echo "{%name}""#);
    let toks = cmd_tokens(&p[0]);
    let ctx = ExecContext::default();
    let argv = build_argv(toks, &ctx);
    // text token is always emitted, but missing interp becomes empty string
    assert_eq!(argv, vec!["echo".to_string(), "".to_string()]);
}

#[test]
fn build_argv_emits_all_pieces_of_present_group() {
    let p = parse("cmd [-flag %name]");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "value".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["cmd", "-flag", "value"]);
}

#[test]
fn build_argv_treats_special_chars_as_literal_data() {
    // Anything sensitive (`;`, `$()`) is just a string, not shell-evaluated.
    let p = parse(r#"echo "{%name}""#);
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query
        .insert("name".into(), "$(touch /tmp/pwn); rm -rf /".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "$(touch /tmp/pwn); rm -rf /"]);
}

#[test]
fn build_argv_resolves_body_form_field() {
    let p = parse(r#"echo "{$.username}""#);
    let toks = cmd_tokens(&p[0]);
    let mut form = BTreeMap::new();
    form.insert(
        "username".to_string(),
        FormFieldValue::Text("alice".to_string()),
    );
    let ctx = ExecContext {
        body: BodyValue::Form(form),
        ..Default::default()
    };
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "alice"]);
}

#[test]
fn build_argv_resolves_body_json_path() {
    let p = parse(r#"echo "{$.user.name}""#);
    let toks = cmd_tokens(&p[0]);
    let body = serde_json::json!({"user": {"name": "Bob"}});
    let ctx = ExecContext {
        body: BodyValue::Json(body),
        ..Default::default()
    };
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Bob"]);
}

#[tokio::test]
async fn run_pipeline_uses_shell_for_spec_syntax() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("shell-redirection-worked");
    let pipeline = parse(&format!("printf worked > {}", marker.display()));

    let output = run_pipeline(std::slice::from_ref(&pipeline), &ExecContext::default(), None)
        .await
        .expect("run pipeline");

    assert_eq!(output.status, 0);
    assert_eq!(std::fs::read_to_string(marker).expect("marker"), "worked");
}

#[tokio::test]
async fn run_pipeline_shell_quotes_request_interpolation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("request-injection-ran");
    let payload = format!("$(printf pwn > {})", marker.display());
    let pipeline = parse(r#"printf %s "{%name}""#);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), payload.clone());

    let output = run_pipeline(std::slice::from_ref(&pipeline), &ctx, None)
        .await
        .expect("run pipeline");

    assert_eq!(output.status, 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout), payload);
    assert!(
        !marker.exists(),
        "request interpolation was executed by shell"
    );
}

#[tokio::test]
async fn run_pipeline_preserves_quoted_literal_as_one_shell_word() {
    let pipeline = parse(r#"printf %s "a;b""#);

    let output = run_pipeline(std::slice::from_ref(&pipeline), &ExecContext::default(), None)
        .await
        .expect("run pipeline");

    assert_eq!(output.status, 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a;b");
}

#[tokio::test]
async fn run_pipeline_materializes_binary_body_as_file_argument() {
    let pipeline = parse("cat [$]");
    let ctx = ExecContext {
        body: BodyValue::Binary(Bytes::from_static(b"abc\0def")),
        ..Default::default()
    };

    let output = run_pipeline(std::slice::from_ref(&pipeline), &ctx, None)
        .await
        .expect("run pipeline");

    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, b"abc\0def");
}

#[tokio::test]
async fn run_pipeline_timeout_kills_final_child() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("final-child-survived");
    let pipeline = parse(&format!(
        "sh -c 'sleep 0.2; printf pwn > {}'",
        marker.display()
    ));

    let err = run_pipeline(
        std::slice::from_ref(&pipeline),
        &ExecContext::default(),
        Some(Duration::from_millis(30)),
    )
    .await
    .expect_err("expected timeout");

    assert!(err.contains("timed out"), "unexpected error: {err}");
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!marker.exists(), "timed-out process kept running");
}

#[tokio::test]
async fn run_pipeline_timeout_kills_prior_pipeline_children() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("prior-child-survived");
    let pipeline = parse(&format!(
        "sh -c 'sleep 0.2; printf pwn > {}' | cat",
        marker.display()
    ));

    let err = run_pipeline(
        std::slice::from_ref(&pipeline),
        &ExecContext::default(),
        Some(Duration::from_millis(30)),
    )
    .await
    .expect_err("expected timeout");

    assert!(err.contains("timed out"), "unexpected error: {err}");
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!marker.exists(), "timed-out pipeline child kept running");
}

#[tokio::test]
async fn run_pipeline_supports_multiline_statements() {
    let dir = tempfile::tempdir().expect("temp dir");
    let f1 = dir.path().join("a");
    let f2 = dir.path().join("b");
    let s1 = mii_http::parse::exec::parse_exec(&format!("printf one > {}", f1.display()), 0).unwrap();
    let s2 = mii_http::parse::exec::parse_exec(&format!("printf two > {}", f2.display()), 0).unwrap();
    let statements = vec![s1, s2];
    let out = run_pipeline(&statements, &ExecContext::default(), None)
        .await
        .expect("ran");
    assert_eq!(out.status, 0);
    assert_eq!(std::fs::read_to_string(&f1).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(&f2).unwrap(), "two");
}

#[tokio::test]
async fn run_pipeline_materializes_binary_form_field() {
    let pipeline = parse("cat [$.file]");
    let mut form = BTreeMap::new();
    form.insert(
        "file".to_string(),
        FormFieldValue::Binary(Bytes::from_static(b"\x00\x01\x02hello")),
    );
    let ctx = ExecContext {
        body: BodyValue::Form(form),
        ..Default::default()
    };
    let output = run_pipeline(std::slice::from_ref(&pipeline), &ctx, None)
        .await
        .expect("run pipeline");
    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, b"\x00\x01\x02hello");
}

#[tokio::test]
async fn run_pipeline_streaming_yields_chunks() {
    use tokio::time::{Duration, timeout};
    let pipeline = parse("printf hello");
    let statements = vec![pipeline];
    let mut streaming = mii_http::exec::run_pipeline_streaming(&statements, &ExecContext::default(), None)
        .await
        .expect("spawn streaming");
    let mut got = Vec::new();
    while let Ok(Some(chunk)) = timeout(Duration::from_secs(2), streaming.stdout_rx.recv()).await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    let completion = streaming.completion.await.expect("join").expect("ok");
    assert_eq!(completion.status, 0);
    assert_eq!(got, b"hello");
}
