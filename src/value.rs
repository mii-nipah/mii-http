//! Value validation against type expressions.

use crate::spec::{JsonField, JsonFieldType, JsonSchema, TypeExpr};
use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub message: String,
}

impl ValidationError {
    fn new(s: impl Into<String>) -> Self {
        Self { message: s.into() }
    }
}

/// Validate a textual value (e.g. query/header/path/form field) against a type.
pub fn validate_text(value: &str, ty: &TypeExpr) -> Result<(), ValidationError> {
    match ty {
        TypeExpr::Int => value
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| ValidationError::new("expected integer")),
        TypeExpr::Float => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| ValidationError::new("expected float")),
        TypeExpr::Boolean => match value {
            "true" | "false" => Ok(()),
            _ => Err(ValidationError::new("expected boolean (true/false)")),
        },
        TypeExpr::Uuid => uuid::Uuid::parse_str(value)
            .map(|_| ())
            .map_err(|_| ValidationError::new("expected uuid")),
        TypeExpr::IntRange { min, max, .. } => {
            let n: i64 = value
                .parse()
                .map_err(|_| ValidationError::new("expected integer"))?;
            if n < *min || n > *max {
                Err(ValidationError::new(format!(
                    "value {} out of range [{}..{}]",
                    n, min, max
                )))
            } else {
                Ok(())
            }
        }
        TypeExpr::FloatRange { min, max, .. } => {
            let n: f64 = value
                .parse()
                .map_err(|_| ValidationError::new("expected float"))?;
            if n < *min || n > *max {
                Err(ValidationError::new(format!(
                    "value {} out of range [{}..{}]",
                    n, min, max
                )))
            } else {
                Ok(())
            }
        }
        TypeExpr::Union { variants, .. } => {
            if variants.iter().any(|v| v == value) {
                Ok(())
            } else {
                Err(ValidationError::new(format!(
                    "expected one of {}",
                    variants.join(", ")
                )))
            }
        }
        TypeExpr::Regex { pattern, .. } => {
            let re = Regex::new(&format!("^(?:{})$", pattern))
                .map_err(|e| ValidationError::new(format!("invalid regex: {}", e)))?;
            if re.is_match(value) {
                Ok(())
            } else {
                Err(ValidationError::new(format!(
                    "value does not match pattern /{}/",
                    pattern
                )))
            }
        }
        TypeExpr::String | TypeExpr::Json | TypeExpr::Binary => Ok(()),
    }
}

pub fn validate_json(value: &Value, schema: &JsonSchema) -> Result<(), ValidationError> {
    let obj = value
        .as_object()
        .ok_or_else(|| ValidationError::new("expected JSON object"))?;
    for f in &schema.fields {
        match obj.get(&f.name) {
            None => {
                if !f.optional {
                    return Err(ValidationError::new(format!(
                        "missing required field `{}`",
                        f.name
                    )));
                }
            }
            Some(v) => validate_json_field(v, f)?,
        }
    }
    Ok(())
}

fn validate_json_field(v: &Value, f: &JsonField) -> Result<(), ValidationError> {
    match &f.ty {
        JsonFieldType::Scalar(t) => validate_json_value(v, t)
            .map_err(|e| ValidationError::new(format!("field `{}`: {}", f.name, e.message))),
        JsonFieldType::Array(t) => {
            let arr = v.as_array().ok_or_else(|| {
                ValidationError::new(format!("field `{}` expected array", f.name))
            })?;
            for item in arr {
                validate_json_value(item, t).map_err(|e| {
                    ValidationError::new(format!("field `{}`: {}", f.name, e.message))
                })?;
            }
            Ok(())
        }
    }
}

fn validate_json_value(v: &Value, ty: &TypeExpr) -> Result<(), ValidationError> {
    match ty {
        TypeExpr::Int => v
            .as_i64()
            .map(|_| ())
            .ok_or_else(|| ValidationError::new("expected integer")),
        TypeExpr::Float => v
            .as_f64()
            .map(|_| ())
            .ok_or_else(|| ValidationError::new("expected float")),
        TypeExpr::Boolean => v
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| ValidationError::new("expected boolean")),
        TypeExpr::Uuid => v
            .as_str()
            .ok_or_else(|| ValidationError::new("expected string"))
            .and_then(|s| validate_text(s, ty)),
        TypeExpr::String | TypeExpr::Json => Ok(()),
        TypeExpr::Binary => Err(ValidationError::new("binary not allowed in JSON schema")),
        TypeExpr::IntRange { .. } => v
            .as_i64()
            .map(|n| n.to_string())
            .ok_or_else(|| ValidationError::new("expected integer"))
            .and_then(|s| validate_text(&s, ty)),
        TypeExpr::FloatRange { .. } => v
            .as_f64()
            .map(|n| n.to_string())
            .ok_or_else(|| ValidationError::new("expected float"))
            .and_then(|s| validate_text(&s, ty)),
        TypeExpr::Union { .. } | TypeExpr::Regex { .. } => v
            .as_str()
            .ok_or_else(|| ValidationError::new("expected string"))
            .and_then(|s| validate_text(s, ty)),
    }
}
