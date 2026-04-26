//! Tests for the chumsky-based Exec parser and `build_argv` semantics
//! (group omission, interpolation).

use mii_http::exec::{BodyValue, ExecContext, build_argv};
use mii_http::spec::{ExecStage, ExecToken, ValueRef};
use std::collections::BTreeMap;

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
    let p = parse("echo {%name}-{:id}");
    let toks = cmd_tokens(&p[0]);
    assert_eq!(toks.len(), 2);
    assert!(matches!(toks[1], ExecToken::Text { .. }));
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
    let p = parse("echo Hello, {%name}!");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "World".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Hello,", "World!"]);
}

#[test]
fn build_argv_omits_optional_group_when_value_missing() {
    let p = parse("echo Hello, [%name] [%guest]");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "World".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Hello,", "World"]);
}

#[test]
fn build_argv_renders_text_interp_as_empty_when_missing() {
    let p = parse("echo {%name}");
    let toks = cmd_tokens(&p[0]);
    let ctx = ExecContext::default();
    let argv = build_argv(toks, &ctx);
    // text token is always emitted, but missing interp becomes empty string
    assert_eq!(argv, vec!["echo".to_string(), "".to_string()]);
}

#[test]
fn build_argv_emits_all_pieces_of_present_group() {
    let p = parse("cmd [-flag {%name}]");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query.insert("name".into(), "value".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["cmd", "-flag", "value"]);
}

#[test]
fn build_argv_treats_special_chars_as_literal_data() {
    // Anything sensitive (`;`, `$()`) is just a string, not shell-evaluated.
    let p = parse("echo {%name}");
    let toks = cmd_tokens(&p[0]);
    let mut ctx = ExecContext::default();
    ctx.query
        .insert("name".into(), "$(touch /tmp/pwn); rm -rf /".into());
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "$(touch /tmp/pwn); rm -rf /"]);
}

#[test]
fn build_argv_resolves_body_form_field() {
    let p = parse("echo {$.username}");
    let toks = cmd_tokens(&p[0]);
    let mut form = BTreeMap::new();
    form.insert("username".to_string(), "alice".to_string());
    let ctx = ExecContext {
        body: BodyValue::Form(form),
        ..Default::default()
    };
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "alice"]);
}

#[test]
fn build_argv_resolves_body_json_path() {
    let p = parse("echo {$.user.name}");
    let toks = cmd_tokens(&p[0]);
    let body = serde_json::json!({"user": {"name": "Bob"}});
    let ctx = ExecContext {
        body: BodyValue::Json(body),
        ..Default::default()
    };
    let argv = build_argv(toks, &ctx);
    assert_eq!(argv, vec!["echo", "Bob"]);
}
