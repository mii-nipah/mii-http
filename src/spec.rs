//! AST for the .http specs file.

use std::collections::BTreeMap;
use std::ops::Range;

pub type Span = Range<usize>;

#[derive(Debug, Clone)]
pub struct Spec {
    pub setup: Setup,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub version: Option<u32>,
    pub base: Option<String>,
    pub auth: Option<AuthSpec>,
    pub jwt_verifier: Option<ValueSource>,
    pub token_secret: Option<ValueSource>,
    pub max_body_size: Option<u64>,
    pub max_query_param_size: Option<u64>,
    pub max_header_size: Option<u64>,
    pub timeout_ms: Option<u64>,
    /// span of the setup region (for diagnostics)
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum AuthSpec {
    /// Bearer token expected in given header name.
    BearerHeader { header: String, span: Span },
}

#[derive(Debug, Clone)]
pub enum ValueSource {
    Env { name: String, span: Span },
    Header { name: String, span: Span },
    Literal { value: String, span: Span },
}

impl ValueSource {
    pub fn span(&self) -> Span {
        match self {
            ValueSource::Env { span, .. }
            | ValueSource::Header { span, .. }
            | ValueSource::Literal { span, .. } => span.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub method: Method,
    pub path: String,
    /// Parsed segments of the path: literal or `:name` typed param.
    pub path_segments: Vec<PathSegment>,
    pub response_type: Option<String>,
    pub query_params: Vec<NamedField>,
    pub headers: Vec<NamedField>,
    pub vars: Vec<VarDef>,
    pub body: Option<BodySpec>,
    pub exec: ExecSpec,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum PathSegment {
    Literal(String),
    Param {
        name: String,
        ty: TypeExpr,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct NamedField {
    pub name: String,
    pub optional: bool,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct VarDef {
    pub name: String,
    pub source: ValueSource,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum BodySpec {
    /// Raw textual body, no schema (`BODY json` unschematized, `BODY string`).
    Json {
        schema: Option<JsonSchema>,
        span: Span,
    },
    Form {
        fields: Vec<NamedField>,
        span: Span,
    },
    String {
        span: Span,
    },
    Binary {
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct JsonSchema {
    pub fields: Vec<JsonField>,
}

#[derive(Debug, Clone)]
pub struct JsonField {
    pub name: String,
    pub optional: bool,
    pub ty: JsonFieldType,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum JsonFieldType {
    Scalar(TypeExpr),
    Array(TypeExpr),
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    Int,
    Float,
    Boolean,
    Uuid,
    String,
    Json,
    Binary,
    IntRange { min: i64, max: i64, span: Span },
    FloatRange { min: f64, max: f64, span: Span },
    Union { variants: Vec<String>, span: Span },
    Regex { pattern: String, span: Span },
}

impl TypeExpr {
    pub fn name(&self) -> &'static str {
        match self {
            TypeExpr::Int => "int",
            TypeExpr::Float => "float",
            TypeExpr::Boolean => "boolean",
            TypeExpr::Uuid => "uuid",
            TypeExpr::String => "string",
            TypeExpr::Json => "json",
            TypeExpr::Binary => "binary",
            TypeExpr::IntRange { .. } => "int(range)",
            TypeExpr::FloatRange { .. } => "float(range)",
            TypeExpr::Union { .. } => "union",
            TypeExpr::Regex { .. } => "regex",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub raw: String,
    pub span: Span,
    pub pipeline: Vec<ExecStage>,
}

/// A pipeline stage: either a value source piped to next, or a command.
#[derive(Debug, Clone)]
pub enum ExecStage {
    /// A bare value reference (e.g. `$`, `$.path`, `%name`) used as stdin into next stage.
    Source {
        reference: ValueRef,
        span: Span,
    },
    Command {
        tokens: Vec<ExecToken>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub enum ExecToken {
    /// A token built from text + quoted-string `{...}` interpolations. Always emitted.
    Text {
        parts: Vec<TextPart>,
        force_quote: bool,
        span: Span,
    },
    /// A `[...]` shell-piece group; if any interpolation is missing, omit the whole group.
    Group { pieces: Vec<GroupPiece>, span: Span },
}

#[derive(Debug, Clone)]
pub enum TextPart {
    Literal(String),
    Interp(ValueRef),
}

#[derive(Debug, Clone)]
pub struct GroupPiece {
    pub parts: Vec<TextPart>,
    pub force_quote: bool,
}

#[derive(Debug, Clone)]
pub enum ValueRef {
    Query(String),
    Path(String),
    Header(String),
    Var(String),
    /// Whole body or a JSON path into the body. Empty path = whole body.
    Body {
        path: Vec<String>,
    },
}

impl ValueRef {
    pub fn describe(&self) -> String {
        match self {
            ValueRef::Query(n) => format!("query param `{}`", n),
            ValueRef::Path(n) => format!("path param `{}`", n),
            ValueRef::Header(n) => format!("header `{}`", n),
            ValueRef::Var(n) => format!("var `{}`", n),
            ValueRef::Body { path } if path.is_empty() => "body".to_string(),
            ValueRef::Body { path } => format!("body field `{}`", path.join(".")),
        }
    }
}

/// Helper map type used by validators.
pub type FieldMap = BTreeMap<String, NamedField>;
