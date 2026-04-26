//! Semantic check (`mii_http::check::check`) tests.

use mii_http::check::check;
use mii_http::diag::DiagKind;
use mii_http::parse::parse;

fn parse_or_panic(src: &str) -> mii_http::spec::Spec {
    let r = parse(src);
    let errors: Vec<_> = r
        .diags
        .iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "parser errors before check: {:#?}",
        errors
    );
    r.spec.expect("parser returned no spec")
}

fn errors_of(src: &str) -> Vec<mii_http::diag::Diag> {
    let s = parse_or_panic(src);
    check(&s)
        .into_iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect()
}

fn warnings_of(src: &str) -> Vec<mii_http::diag::Diag> {
    let s = parse_or_panic(src);
    check(&s)
        .into_iter()
        .filter(|d| d.kind == DiagKind::Warning)
        .collect()
}

#[test]
fn accepts_valid_spec() {
    let s = parse_or_panic(
        r#"
GET /users/:id:uuid
QUERY name: /[a-z]+/
HEADER X-Foo: /[a-zA-Z]+/
Exec: echo [:id] [%name] [^X-Foo]
"#,
    );
    let errs: Vec<_> = check(&s)
        .into_iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect();
    assert!(errs.is_empty(), "expected clean check, got {:?}", errs);
}

#[test]
fn rejects_string_type_on_query_param() {
    let errs = errors_of(
        r#"
GET /x
QUERY a: string
Exec: echo [%a]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("string")),
        "expected `string` rejection, got {:?}",
        errs
    );
}

#[test]
fn rejects_json_type_on_query_param() {
    let errs = errors_of(
        r#"
GET /x
QUERY a: json
Exec: echo [%a]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("json")),
        "expected `json` rejection, got {:?}",
        errs
    );
}

#[test]
fn rejects_binary_type_outside_body() {
    let errs = errors_of(
        r#"
GET /x
QUERY a: binary
Exec: echo [%a]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("binary")),
        "expected `binary` rejection, got {:?}",
        errs
    );
}

#[test]
fn rejects_unresolved_reference_in_exec() {
    let errs = errors_of(
        r#"
GET /x
Exec: echo [%nope]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("unresolved")),
        "expected unresolved reference error, got {:?}",
        errs
    );
}

#[test]
fn rejects_string_body_passed_as_argv() {
    let errs = errors_of(
        r#"
POST /x
BODY string
Exec: echo [$]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("argv")),
        "expected argv rejection for string body, got {:?}",
        errs
    );
}

#[test]
fn allows_string_body_via_stdin() {
    let s = parse_or_panic(
        r#"
POST /x
BODY string
Exec: $ | xargs echo
"#,
    );
    let errs: Vec<_> = check(&s)
        .into_iter()
        .filter(|d| d.kind == DiagKind::Error)
        .collect();
    assert!(errs.is_empty(), "expected clean, got {:?}", errs);
}

#[test]
fn rejects_untyped_json_body_as_argv() {
    let errs = errors_of(
        r#"
POST /x
BODY json
Exec: echo [$]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("argv")),
        "expected argv rejection for untyped json body, got {:?}",
        errs
    );
}

#[test]
fn rejects_duplicate_query_params() {
    let errs = errors_of(
        r#"
GET /x
QUERY a: int
QUERY a: int
Exec: echo [%a]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("duplicate")),
        "expected duplicate error, got {:?}",
        errs
    );
}

#[test]
fn rejects_duplicate_var_names() {
    let errs = errors_of(
        r#"
GET /x
VAR a [ENV A]
VAR a [ENV B]
Exec: echo [@a]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("duplicate")),
        "expected duplicate VAR error, got {:?}",
        errs
    );
}

#[test]
fn warns_on_duplicate_endpoint() {
    let warns = warnings_of(
        r#"
GET /x
Exec: echo a

GET /x
Exec: echo b
"#,
    );
    assert!(
        warns.iter().any(|d| d.message.contains("duplicate")),
        "expected duplicate endpoint warning, got {:?}",
        warns
    );
}

#[test]
fn warns_on_bearer_without_verifier() {
    let warns = warnings_of(
        r#"
AUTH Bearer [HEADER Authorization]

GET /x
Exec: echo ok
"#,
    );
    assert!(
        warns.iter().any(|d| d.message.contains("JWT_VERIFIER")
            || d.message.contains("TOKEN_SECRET")
            || d.message.to_lowercase().contains("auth")),
        "expected auth warning, got {:?}",
        warns
    );
}

#[test]
fn warns_on_permissive_regex() {
    let warns = warnings_of(
        r#"
GET /x
QUERY a: /.*/
Exec: echo [%a]
"#,
    );
    assert!(
        warns
            .iter()
            .any(|d| d.message.contains("permissive") || d.message.contains("`.*`")),
        "expected permissive regex warning, got {:?}",
        warns
    );
}

#[test]
fn warns_on_get_with_body() {
    let warns = warnings_of(
        r#"
GET /x
BODY string
Exec: $ | xargs echo
"#,
    );
    assert!(
        warns
            .iter()
            .any(|d| d.message.to_lowercase().contains("get")),
        "expected GET-with-body warning, got {:?}",
        warns
    );
}

#[test]
fn rejects_unresolved_body_field_reference() {
    let errs = errors_of(
        r#"
POST /x
BODY form {
  name: /[a-z]+/
}
Exec: echo [$.nope]
"#,
    );
    assert!(
        errs.iter().any(|d| d.message.contains("unresolved")),
        "expected unresolved body ref error, got {:?}",
        errs
    );
}

#[test]
fn rejects_string_typed_body_field_as_argv() {
    let errs = errors_of(
        r#"
POST /x
BODY json {
  blob: string
}
Exec: echo [$.blob]
"#,
    );
    // Either the field-type security check OR argv check should error.
    assert!(
        !errs.is_empty(),
        "expected error for `string` json field, got nothing"
    );
}
