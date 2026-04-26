//! Parser acceptance/rejection tests.

use mii_http::diag::DiagKind;
use mii_http::parse::parse;
use mii_http::spec::{
    AuthSpec, BodySpec, ExecStage, ExecToken, JsonFieldType, Method, PathSegment, TextPart,
    TypeExpr, ValueRef, ValueSource,
};

fn must_parse(src: &str) -> mii_http::spec::Spec {
    let r = parse(src);
    let errors: Vec<_> = r
        .diags
        .iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "expected clean parse, got errors: {:#?}\n--- source:\n{}",
        errors,
        src
    );
    r.spec.expect("parser returned no spec")
}

fn parse_errors(src: &str) -> Vec<mii_http::diag::Diag> {
    parse(src)
        .diags
        .into_iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect()
}

// ---------- happy path ----------

#[test]
fn parses_minimal_endpoint() {
    let s = must_parse(
        r#"
GET /status
Exec: echo ok
"#,
    );
    assert_eq!(s.endpoints.len(), 1);
    let ep = &s.endpoints[0];
    assert_eq!(ep.method, Method::Get);
    assert_eq!(ep.path, "/status");
    assert!(matches!(
        ep.exec.pipeline.first(),
        Some(ExecStage::Command { .. })
    ));
}

#[test]
fn parses_full_setup_block() {
    let s = must_parse(
        r#"
VERSION 1
BASE /named
AUTH Bearer [HEADER Authorization]
JWT_VERIFIER [ENV JWT_SECRET]
TOKEN_SECRET [ENV TOK]
MAX_BODY_SIZE 1mb
MAX_QUERY_PARAM_SIZE 100
MAX_HEADER_SIZE 200
TIMEOUT 5s

GET /ping
Exec: echo pong
"#,
    );
    assert_eq!(s.setup.version, Some(1));
    assert_eq!(s.setup.base.as_deref(), Some("/named"));
    assert_eq!(s.setup.max_body_size, Some(1024 * 1024));
    assert_eq!(s.setup.max_query_param_size, Some(100));
    assert_eq!(s.setup.max_header_size, Some(200));
    assert_eq!(s.setup.timeout_ms, Some(5000));
    assert!(matches!(
        s.setup.auth,
        Some(AuthSpec::BearerHeader { ref header, .. }) if header == "Authorization"
    ));
    assert!(matches!(
        s.setup.jwt_verifier,
        Some(ValueSource::Env { ref name, .. }) if name == "JWT_SECRET"
    ));
}

#[test]
fn parses_typed_path_param() {
    let s = must_parse(
        r#"
GET /users/:user_id:uuid
Exec: echo [:user_id]
"#,
    );
    let ep = &s.endpoints[0];
    let param = ep
        .path_segments
        .iter()
        .find_map(|seg| match seg {
            PathSegment::Param { name, ty, .. } => Some((name.clone(), ty.clone())),
            _ => None,
        })
        .expect("expected one path param");
    assert_eq!(param.0, "user_id");
    assert!(matches!(param.1, TypeExpr::Uuid));
}

#[test]
fn parses_query_with_optional_and_regex() {
    let s = must_parse(
        r#"
GET /q
QUERY name: /[a-zA-Z0-9_]+/
QUERY guest?: /[a-zA-Z0-9_]+/
Exec: echo [%name] [%guest]
"#,
    );
    let ep = &s.endpoints[0];
    assert_eq!(ep.query_params.len(), 2);
    assert!(!ep.query_params[0].optional);
    assert!(ep.query_params[1].optional);
    assert!(matches!(ep.query_params[0].ty, TypeExpr::Regex { .. }));
}

#[test]
fn parses_int_range_and_union() {
    let s = must_parse(
        r#"
GET /q
QUERY age: int(0..150)
QUERY mode: on|off|auto
Exec: echo [%age] [%mode]
"#,
    );
    let ep = &s.endpoints[0];
    let by_name = |n: &str| ep.query_params.iter().find(|f| f.name == n).unwrap();
    assert!(matches!(
        by_name("age").ty,
        TypeExpr::IntRange { min: 0, max: 150, .. }
    ));
    match &by_name("mode").ty {
        TypeExpr::Union { variants, .. } => {
            assert_eq!(variants, &vec!["on".to_string(), "off".into(), "auto".into()]);
        }
        other => panic!("expected union, got {:?}", other),
    }
}

#[test]
fn parses_form_body_block() {
    let s = must_parse(
        r#"
POST /submit
BODY form {
  username: /[a-zA-Z0-9_]+/
  age?: int(0..150)
}
Exec: echo [$.username] [$.age]
"#,
    );
    let ep = &s.endpoints[0];
    match &ep.body {
        Some(BodySpec::Form { fields, .. }) => {
            assert_eq!(fields.len(), 2);
            assert!(!fields[0].optional);
            assert!(fields[1].optional);
        }
        other => panic!("expected form body, got {:?}", other),
    }
}

#[test]
fn parses_json_body_with_schema_and_array() {
    let s = must_parse(
        r#"
POST /submit
BODY json {
  title: /[a-zA-Z0-9_ ]+/
  count?: int
  tags: [/[a-z]+/]
}
Exec: echo [$.title]
"#,
    );
    let ep = &s.endpoints[0];
    match &ep.body {
        Some(BodySpec::Json { schema: Some(schema), .. }) => {
            assert_eq!(schema.fields.len(), 3);
            assert!(matches!(schema.fields[2].ty, JsonFieldType::Array(_)));
        }
        other => panic!("expected typed json body, got {:?}", other),
    }
}

#[test]
fn parses_string_and_binary_and_unschematized_json_body() {
    for (kind, expected) in &[
        ("string", "string"),
        ("binary", "binary"),
        ("json", "json"),
    ] {
        let src = format!(
            "POST /e\nBODY {}\nExec: echo ok\n",
            kind
        );
        let s = must_parse(&src);
        let ep = &s.endpoints[0];
        let actual = match &ep.body {
            Some(BodySpec::String { .. }) => "string",
            Some(BodySpec::Binary { .. }) => "binary",
            Some(BodySpec::Json { schema: None, .. }) => "json",
            _ => "<other>",
        };
        assert_eq!(&actual, expected);
    }
}

#[test]
fn parses_var_env_and_header() {
    let s = must_parse(
        r#"
GET /v
VAR a [ENV GREETING]
VAR b [HEADER X-Foo]
Exec: echo [@a] [@b]
"#,
    );
    let ep = &s.endpoints[0];
    assert_eq!(ep.vars.len(), 2);
    assert!(matches!(
        ep.vars[0].source,
        ValueSource::Env { ref name, .. } if name == "GREETING"
    ));
    assert!(matches!(
        ep.vars[1].source,
        ValueSource::Header { ref name, .. } if name == "X-Foo"
    ));
}

#[test]
fn parses_exec_with_groups_and_interpolations() {
    let s = must_parse(
        r#"
GET /e
QUERY name: /[a-zA-Z]+/
QUERY guest?: /[a-zA-Z]+/
Exec: echo Hello, [%name] [%guest]
"#,
    );
    let stages = &s.endpoints[0].exec.pipeline;
    assert_eq!(stages.len(), 1);
    let tokens = match &stages[0] {
        ExecStage::Command { tokens, .. } => tokens,
        _ => panic!("expected command stage"),
    };
    // tokens: echo, Hello,, [%name], [%guest]
    assert_eq!(tokens.len(), 4);
    assert!(matches!(tokens[2], ExecToken::Group { .. }));
    assert!(matches!(tokens[3], ExecToken::Group { .. }));
}

#[test]
fn parses_exec_pipeline_with_source_stage() {
    let s = must_parse(
        r#"
POST /echo
BODY string
Exec: $ | xargs echo
"#,
    );
    let stages = &s.endpoints[0].exec.pipeline;
    assert_eq!(stages.len(), 2);
    assert!(matches!(
        stages[0],
        ExecStage::Source {
            reference: ValueRef::Body { ref path },
            ..
        } if path.is_empty()
    ));
    assert!(matches!(stages[1], ExecStage::Command { .. }));
}

#[test]
fn ignores_comments_and_blank_lines() {
    let s = must_parse(
        r#"
# top comment

VERSION 1

# before endpoint
GET /a
# inside-ish (line outside endpoint)
Exec: echo a

GET /b
Exec: echo b
"#,
    );
    assert_eq!(s.endpoints.len(), 2);
}

#[test]
fn parses_multiple_methods() {
    let s = must_parse(
        r#"
GET /a
Exec: echo a

POST /a
Exec: echo b

PUT /a
Exec: echo c

DELETE /a
Exec: echo d

PATCH /a
Exec: echo e
"#,
    );
    let methods: Vec<_> = s.endpoints.iter().map(|e| e.method).collect();
    assert_eq!(
        methods,
        vec![
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Delete,
            Method::Patch
        ]
    );
}

#[test]
fn header_interpolation_uses_header_ref() {
    let s = must_parse(
        r#"
GET /h
HEADER X-Custom: /[a-z]+/
Exec: echo {^X-Custom}
"#,
    );
    let stages = &s.endpoints[0].exec.pipeline;
    let tokens = match &stages[0] {
        ExecStage::Command { tokens, .. } => tokens,
        _ => panic!(),
    };
    // last token contains an Interp(Header)
    let last = tokens.last().unwrap();
    let parts = match last {
        ExecToken::Text { parts, .. } => parts,
        _ => panic!("expected text token"),
    };
    assert!(parts.iter().any(|p| matches!(
        p,
        TextPart::Interp(ValueRef::Header(n)) if n == "X-Custom"
    )));
}

// ---------- rejection ----------

#[test]
fn rejects_unknown_method() {
    let errs = parse_errors(
        r#"
WHATEVER /x
Exec: echo nope
"#,
    );
    assert!(!errs.is_empty(), "expected error for unknown method");
}

#[test]
fn rejects_endpoint_without_exec() {
    let errs = parse_errors(
        r#"
GET /x
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.to_lowercase().contains("exec")),
        "expected error mentioning Exec, got {:?}",
        errs
    );
}

#[test]
fn rejects_invalid_type_expr() {
    let errs = parse_errors(
        r#"
GET /x
QUERY a: notatype
Exec: echo [%a]
"#,
    );
    assert!(!errs.is_empty(), "expected type parse error");
}

#[test]
fn rejects_unterminated_json_body_block() {
    let errs = parse_errors(
        r#"
POST /x
BODY json {
  title: /[a-z]+/
"#,
    );
    assert!(!errs.is_empty(), "expected error for unterminated body block");
}

#[test]
fn rejects_invalid_exec_syntax() {
    let errs = parse_errors(
        r#"
GET /x
Exec: echo [unclosed
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("Exec")),
        "expected exec parse error, got {:?}",
        errs
    );
}

#[test]
fn rejects_invalid_int_range() {
    let errs = parse_errors(
        r#"
GET /x
QUERY a: int(notanumber..10)
Exec: echo [%a]
"#,
    );
    assert!(!errs.is_empty(), "expected error on bad int range");
}

#[test]
fn rejects_unknown_setup_directive() {
    // Unknown directive at setup region should be flagged or stop parsing.
    let r = parse(
        r#"
NOT_A_DIRECTIVE foo
GET /x
Exec: echo ok
"#,
    );
    let has_err = r.diags.iter().any(|d| d.kind == DiagKind::Error);
    assert!(has_err, "expected error for unknown setup directive");
}
