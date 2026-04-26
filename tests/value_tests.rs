//! Tests for runtime value validation against type expressions.

use mii_http::spec::{JsonField, JsonFieldType, JsonSchema, TypeExpr};
use mii_http::value::{validate_json, validate_text};

fn span() -> std::ops::Range<usize> {
    0..0
}

#[test]
fn validates_int() {
    assert!(validate_text("42", &TypeExpr::Int).is_ok());
    assert!(validate_text("-7", &TypeExpr::Int).is_ok());
    assert!(validate_text("3.14", &TypeExpr::Int).is_err());
    assert!(validate_text("abc", &TypeExpr::Int).is_err());
}

#[test]
fn validates_float() {
    assert!(validate_text("3.14", &TypeExpr::Float).is_ok());
    assert!(validate_text("42", &TypeExpr::Float).is_ok());
    assert!(validate_text("foo", &TypeExpr::Float).is_err());
}

#[test]
fn validates_boolean() {
    assert!(validate_text("true", &TypeExpr::Boolean).is_ok());
    assert!(validate_text("false", &TypeExpr::Boolean).is_ok());
    assert!(validate_text("True", &TypeExpr::Boolean).is_err());
    assert!(validate_text("1", &TypeExpr::Boolean).is_err());
}

#[test]
fn validates_uuid() {
    assert!(validate_text("123e4567-e89b-12d3-a456-426614174000", &TypeExpr::Uuid).is_ok());
    assert!(validate_text("not-a-uuid", &TypeExpr::Uuid).is_err());
}

#[test]
fn validates_int_range() {
    let ty = TypeExpr::IntRange { min: 0, max: 10, span: span() };
    assert!(validate_text("0", &ty).is_ok());
    assert!(validate_text("5", &ty).is_ok());
    assert!(validate_text("10", &ty).is_ok());
    assert!(validate_text("-1", &ty).is_err());
    assert!(validate_text("11", &ty).is_err());
}

#[test]
fn validates_union() {
    let ty = TypeExpr::Union {
        variants: vec!["on".into(), "off".into()],
        span: span(),
    };
    assert!(validate_text("on", &ty).is_ok());
    assert!(validate_text("off", &ty).is_ok());
    assert!(validate_text("auto", &ty).is_err());
    assert!(validate_text("ON", &ty).is_err());
}

#[test]
fn validates_regex_anchored_full_match() {
    let ty = TypeExpr::Regex {
        pattern: "[a-z]+".into(),
        span: span(),
    };
    assert!(validate_text("hello", &ty).is_ok());
    // anchoring: must be a full match, not a prefix
    assert!(validate_text("hello!", &ty).is_err());
    assert!(validate_text("Hello", &ty).is_err());
    assert!(validate_text("", &ty).is_err());
}

#[test]
fn regex_rejects_command_injection_payload() {
    let ty = TypeExpr::Regex {
        pattern: "[a-zA-Z0-9_]+".into(),
        span: span(),
    };
    assert!(validate_text("alice", &ty).is_ok());
    assert!(validate_text("$(touch /tmp/pwn)", &ty).is_err());
    assert!(validate_text("alice; rm -rf /", &ty).is_err());
    assert!(validate_text("`whoami`", &ty).is_err());
}

#[test]
fn validates_json_schema_required_and_optional() {
    let schema = JsonSchema {
        fields: vec![
            JsonField {
                name: "title".into(),
                optional: false,
                ty: JsonFieldType::Scalar(TypeExpr::Regex {
                    pattern: "[a-zA-Z ]+".into(),
                    span: span(),
                }),
                span: span(),
            },
            JsonField {
                name: "count".into(),
                optional: true,
                ty: JsonFieldType::Scalar(TypeExpr::Int),
                span: span(),
            },
        ],
    };
    let ok: serde_json::Value = serde_json::json!({"title": "Hello", "count": 3});
    assert!(validate_json(&ok, &schema).is_ok());

    let optional_missing: serde_json::Value = serde_json::json!({"title": "Hi"});
    assert!(validate_json(&optional_missing, &schema).is_ok());

    let required_missing: serde_json::Value = serde_json::json!({"count": 1});
    assert!(validate_json(&required_missing, &schema).is_err());

    let bad_count: serde_json::Value = serde_json::json!({"title": "Hi", "count": "nope"});
    assert!(validate_json(&bad_count, &schema).is_err());

    let bad_title: serde_json::Value = serde_json::json!({"title": "Hi!"}); // `!` not allowed
    assert!(validate_json(&bad_title, &schema).is_err());
}

#[test]
fn validates_json_array_field() {
    let schema = JsonSchema {
        fields: vec![JsonField {
            name: "tags".into(),
            optional: false,
            ty: JsonFieldType::Array(TypeExpr::Regex {
                pattern: "[a-z]+".into(),
                span: span(),
            }),
            span: span(),
        }],
    };
    let ok: serde_json::Value = serde_json::json!({"tags": ["foo", "bar"]});
    assert!(validate_json(&ok, &schema).is_ok());

    let not_array: serde_json::Value = serde_json::json!({"tags": "foo"});
    assert!(validate_json(&not_array, &schema).is_err());

    let bad_item: serde_json::Value = serde_json::json!({"tags": ["foo", "BAR"]});
    assert!(validate_json(&bad_item, &schema).is_err());
}
